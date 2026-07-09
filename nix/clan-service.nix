# Optional clan service: a thin wrapper over the NixOS module (DECISIONS §11).
# ssync is leaderless, so there is a single role, `peer`; every machine in an
# instance runs the same daemon as an equal. Exposed from flake.nix as
# `clan.modules.ssync`.
#
# clan.vars provides everything so the user configures nothing but the peers:
#   - a per-machine age key whose public recipient is shared to the other peers
#     (multi-recipient encryption),
#   - a shared namespace secret (one deterministic namespace, no ticket) that
#     rotates on every membership change: the departed machine keeps only the
#     abandoned namespace (eviction, issue #22) and every remaining daemon
#     re-imports under the current recipient set, and
#   - a per-machine node key whose public node-id is shared to the other peers,
#     so they auto-connect.
{ self }:
{ clanLib, ... }:
{
  _class = "clan.service";
  manifest.name = "ssync";
  manifest.description = "p2p sync of coding-agent session files";
  manifest.categories = [ "Utility" ];
  manifest.readme = ''
    Runs the ssync daemon on each machine as an equal peer (role `peer`), syncing
    coding-agent session files. clan.vars generates a per-machine age key (each
    peer encrypts to all peers' recipients), a shared namespace secret, and each
    machine's node-id, so peers connect automatically with no manual pairing.
    Just list the peer machines.
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
        };
      };

    perInstance =
      {
        settings,
        machine,
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
            otherPeers = lib.filterAttrs (name: _: name != machine.name) roles.peer.machines;
            # public var of each other peer; generators print with a trailing
            # newline, strip it so the value stays a single TOML string.
            fromPeers =
              generator: file:
              lib.mapAttrsToList (
                name: _:
                lib.removeSuffix "\n" (
                  clanLib.getPublicValue {
                    flake = config.clan.core.settings.directory;
                    machine = name;
                    inherit generator file;
                  }
                )
              ) otherPeers;
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
              validation.peers = lib.concatStringsSep " " (lib.attrNames roles.peer.machines);
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
              namespaceSecretFile = gens.ssync-namespace.files.secret.path;
              nodeKeyFile = gens.ssync-node.files.key.path;
              peers = fromPeers "ssync-node" "id";
              recipients = fromPeers "ssync-age" "recipient";
            };
          };
      };
  };
}
