# Optional clan service: a thin wrapper over the NixOS module (DECISIONS §11).
# ssync is leaderless, so there is a single role, `peer`; every machine in an
# instance runs the same daemon as an equal. Exposed from flake.nix as
# `clan.modules.ssync`.
#
# clan.vars provides everything so the user configures nothing but the peers:
#   - a per-machine age key whose public recipient is shared to the other peers
#     (multi-recipient encryption),
#   - a per-machine node key whose public node-id is shared to the other peers,
#     so they auto-connect, and
#   - a shared namespace secret. The peer set is part of the generator's
#     identity: any membership change regenerates the secret, rotating the
#     namespace — the departed machine keeps only the abandoned namespace
#     (eviction, issue #22).
#
# The daemon consumes the same cluster artifact as the manual `ssync cluster`
# flow (issue #38): each machine assembles cluster.toml at service start
# (`ssync cluster render --secret-file` in preStart) from the shared secret
# plus the eval-time member list — deterministic, so every machine holds the
# identical file. Cross-machine publics stay at module eval, where they
# resolve after `clan vars generate`; putting them inside a generator script
# would deadlock the first generate. `extraMembers` admits peers outside the
# clan: hand them the assembled artifact and run `ssync cluster join` there.
{ self }:
{ clanLib, ... }:
{
  _class = "clan.service";
  manifest.name = "ssync";
  manifest.description = "p2p sync of coding-agent session files";
  manifest.categories = [ "Utility" ];
  manifest.readme = ''
    Runs the ssync daemon on each machine as an equal peer (role `peer`), syncing
    coding-agent session files. clan.vars generates a per-machine age key and
    node key plus a shared namespace secret; each machine assembles the cluster
    artifact (namespace secret + every peer's recipient and node-id) at service
    start, so peers connect automatically with no manual pairing. Just list the
    peer machines. Peers outside the clan can be added via `extraMembers`.
  '';

  roles.peer = {
    description = "An equal peer that syncs its agent sessions with the others.";
    interface =
      { lib, ... }:
      {
        options = {
          user = lib.mkOption {
            type = lib.types.str;
            description = "User to run the daemon as; must own the agents' session dirs.";
          };
          extraMembers = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            example = [ "age1pq1...:ee9d0d54..." ];
            description = ''
              Cluster members outside the clan, as `recipient[:node-id]`.
              Give such a machine the assembled cluster.toml (any clan
              machine's /run/ssync/cluster.toml) and run `ssync cluster join`
              there. Any membership change — clan machines or this list —
              rotates the namespace secret (removal must evict), so
              redistribute the artifact to external members afterwards.
            '';
          };
        };
      };

    perInstance =
      {
        settings,
        roles,
        ...
      }:
      {
        nixosModule =
          {
            config,
            pkgs,
            lib,
            ...
          }:
          let
            ssync = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
            gens = config.clan.core.vars.generators;
            # public var of a machine; generators print with a trailing
            # newline, strip it so the value stays a single TOML string.
            publicOf =
              name: generator: file:
              lib.removeSuffix "\n" (
                clanLib.getPublicValue {
                  flake = config.clan.core.settings.directory;
                  machine = name;
                  inherit generator file;
                }
              );
            # every peer (self included) as `recipient:node-id`, plus the
            # non-clan extras — the member list of the cluster artifact,
            # identical on every machine. Resolved at module eval, after
            # `clan vars generate` (a generator script embedding these
            # publics would deadlock the first generate).
            members =
              map (name: "${publicOf name "ssync-age" "recipient"}:${publicOf name "ssync-node" "id"}") (
                lib.attrNames roles.peer.machines
              )
              ++ settings.extraMembers;
          in
          {
            imports = [ self.nixosModules.default ];

            # per-machine age key; its public recipient is non-secret and read
            # by the other peers so everyone encrypts to everyone.
            clan.core.vars.generators.ssync-age = {
              files.key = {
                secret = true;
                deploy = true;
                owner = settings.user;
              };
              files.recipient = {
                secret = false;
                deploy = true;
              };
              runtimeInputs = [ pkgs.age ];
              script = ''
                age-keygen -pq -o "$out"/key
                age-keygen -y "$out"/key > "$out"/recipient
              '';
            };

            # shared namespace secret — one value across all peers. The peer
            # set is part of the generator's identity: any membership change
            # regenerates the secret, rotating the namespace (issue #22).
            clan.core.vars.generators.ssync-namespace = {
              share = true;
              validation.peers = lib.concatStringsSep " " (
                lib.attrNames roles.peer.machines ++ settings.extraMembers
              );
              files.secret = {
                secret = true;
                deploy = true;
                owner = settings.user;
              };
              runtimeInputs = [ ssync ];
              script = ''
                ssync keygen-namespace "$out"/secret
              '';
            };

            # per-machine node key; its public node-id is non-secret and read by
            # the other peers to connect.
            clan.core.vars.generators.ssync-node = {
              files.key = {
                secret = true;
                deploy = true;
                owner = settings.user;
              };
              files.id = {
                secret = false;
                deploy = true;
              };
              runtimeInputs = [ ssync ];
              script = ''
                ssync keygen-node "$out"/key > "$out"/id
              '';
            };

            services.ssync = {
              enable = true;
              inherit (settings) user;
              ageIdentityFile = gens.ssync-age.files.key.path;
              clusterFile = "/run/ssync/cluster.toml";
              nodeKeyFile = gens.ssync-node.files.key.path;
            };

            # assemble the artifact fresh on every start: shared secret from
            # vars + eval-time member list, through the CLI serializer (one
            # format — the file is byte-identical on every machine and can be
            # handed to non-clan members for `ssync cluster join`).
            systemd.services.ssync.serviceConfig.RuntimeDirectory = "ssync";
            systemd.services.ssync.preStart = ''
              ${ssync}/bin/ssync cluster render \
                --out /run/ssync/cluster.toml \
                --secret-file ${gens.ssync-namespace.files.secret.path} \
                ${lib.escapeShellArgs members}
            '';
          };
      };
  };
}
