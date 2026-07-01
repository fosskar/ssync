# Optional clan service: a thin wrapper over the NixOS module (DECISIONS §11).
# ssync is leaderless, so there is a single role, `peer`; every machine in an
# instance runs the same daemon as an equal. Exposed from flake.nix as
# `clan.modules.ssync`.
#
# clan.vars provides everything so the user configures nothing but the peers:
#   - a shared age key (encryption),
#   - a shared namespace secret (one deterministic namespace, no ticket), and
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
    coding-agent session files. clan.vars generates and distributes the shared age
    key, a shared namespace secret, and each machine's node-id, so peers connect
    automatically with no manual pairing. Just list the peer machines.
  '';

  roles.peer = {
    description = "An equal peer that syncs its agent sessions with the others.";
    interface =
      { lib, ... }:
      {
        options = {
          user = lib.mkOption {
            type = lib.types.str;
            description = "User to run the daemon as; must own sessionDir.";
          };
          agent = lib.mkOption {
            type = lib.types.str;
            default = "pi";
            description = "Agent whose sessions to sync (v1: pi).";
          };
          sessionDir = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Session directory to watch. Defaults to pi's location.";
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
          in
          {
            imports = [ self.nixosModules.default ];

            # shared age key (encryption) — one value across all peers.
            clan.core.vars.generators.ssync-age = {
              share = true;
              files.key = {
                secret = true;
                deploy = true;
                owner = settings.user;
              };
              runtimeInputs = [ pkgs.age ];
              script = ''
                age-keygen -pq -o "$out"/key
              '';
            };

            # shared namespace secret — one value across all peers.
            clan.core.vars.generators.ssync-namespace = {
              share = true;
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
              inherit (settings) user agent;
              ageIdentityFile = gens.ssync-age.files.key.path;
              namespaceSecretFile = gens.ssync-namespace.files.secret.path;
              nodeKeyFile = gens.ssync-node.files.key.path;
              peers = lib.mapAttrsToList (
                name: _:
                clanLib.getPublicValue {
                  flake = config.clan.core.settings.directory;
                  machine = name;
                  generator = "ssync-node";
                  file = "id";
                }
              ) otherPeers;
            }
            // lib.optionalAttrs (settings.sessionDir != null) {
              inherit (settings) sessionDir;
            };
          };
      };
  };
}
