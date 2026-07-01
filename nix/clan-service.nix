# Optional clan service: a thin wrapper over the NixOS module (DECISIONS §11).
# ssync is leaderless, so there is a single role, `peer`; every machine in an
# instance runs the same daemon as an equal. Exposed from flake.nix as
# `clanModules.default`.
#
# The shared age identity is generated and distributed by clan.vars (a `share`d
# generator running `age-keygen -pq`), so the user configures nothing about age.
{ self }:
{ ... }:
{
  _class = "clan.service";
  manifest.name = "ssync";
  manifest.description = "p2p sync of coding-agent session files";
  manifest.categories = [ "Utility" ];

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
      { settings, ... }:
      {
        nixosModule =
          { config, pkgs, ... }:
          {
            imports = [ self.nixosModules.default ];

            # one shared age key for the whole instance, generated once and
            # deployed to every peer.
            clan.core.vars.generators.ssync-age = {
              share = true;
              files.key = {
                secret = true;
                deploy = true;
                owner = settings.user;
              };
              runtimeInputs = [ pkgs.age ];
              # age-keygen writes the identity file directly (ssync reads the
              # AGE-SECRET-KEY line, ignoring the comment lines), so no grep is
              # needed in the sandbox.
              script = ''
                age-keygen -pq -o "$out"/key
              '';
            };

            services.ssync = {
              enable = true;
              inherit (settings) user agent;
              ageIdentityFile = config.clan.core.vars.generators.ssync-age.files.key.path;
            }
            // pkgs.lib.optionalAttrs (settings.sessionDir != null) {
              inherit (settings) sessionDir;
            };
          };
      };
  };
}
