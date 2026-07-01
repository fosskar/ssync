# Optional clan service: a thin wrapper over the NixOS module (DECISIONS §11).
# ssync is leaderless, so there is a single role, `peer`; every machine in an
# instance runs the same daemon as an equal. Exposed from flake.nix as
# `clanModules.default`. This is inert unless evaluated by clan — no clan
# dependency is added to the flake.
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
            type = lib.types.str;
            description = "The agent's session directory to watch (absolute).";
          };
          ageIdentityFile = lib.mkOption {
            type = lib.types.str;
            description = "Shared age identity file (same key on every machine).";
          };
          dataDir = lib.mkOption {
            type = lib.types.str;
            default = "/var/lib/ssync";
            description = "ssync's own managed state.";
          };
        };
      };

    perInstance =
      { settings, ... }:
      {
        nixosModule = {
          imports = [ self.nixosModules.default ];
          services.ssync = {
            enable = true;
            inherit (settings)
              user
              agent
              sessionDir
              ageIdentityFile
              dataDir
              ;
          };
        };
      };
  };
}
