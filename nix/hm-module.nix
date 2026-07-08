# home-manager module: run `ssync daemon` as a systemd user service. This is the
# primary deployment for ssync since coding-agent sessions are per-user. Wired
# from flake.nix as `homeManagerModules.default` (captures `self` for the default
# package).
{ self }:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.services.ssync;
  configFile = pkgs.writeText "ssync-config.toml" (
    ''
      age_identity_path = "${cfg.ageIdentityFile}"
      data_dir = "${cfg.dataDir}"
    ''
    + lib.optionalString (cfg.recipients != [ ]) ''
      recipients = [ ${
        lib.concatMapStringsSep ", " (r: "\"${lib.removeSuffix "\n" r}\"") cfg.recipients
      } ]
    ''
    + lib.concatMapStrings (a: ''
      [[agents]]
      agent = "${a.agent}"
      session_dir = "${a.sessionDir}"
    '') cfg.agents
  );
in
{
  options.services.ssync = {
    enable = lib.mkEnableOption "ssync session-sync daemon (user service)";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "The ssync package to run.";
    };

    agents = lib.mkOption {
      type = lib.types.listOf (
        lib.types.submodule {
          options = {
            agent = lib.mkOption {
              type = lib.types.str;
              description = "Agent name (pi or omp).";
            };
            sessionDir = lib.mkOption {
              type = lib.types.str;
              description = "The agent's session directory to watch (absolute path).";
            };
          };
        }
      );
      default = [
        {
          agent = "pi";
          sessionDir = "${config.home.homeDirectory}/.pi/agent/sessions";
        }
        {
          agent = "omp";
          sessionDir = "${config.home.homeDirectory}/.omp/agent/sessions";
        }
      ];
      defaultText = lib.literalExpression "pi and omp at the user's home";
      description = "Agents to sync side by side; the default covers every supported agent.";
    };

    ageIdentityFile = lib.mkOption {
      type = lib.types.str;
      default = "${cfg.dataDir}/age.key";
      defaultText = lib.literalExpression "\"\${dataDir}/age.key\"";
      description = ''
        Age identity file. If it does not exist the daemon generates one on
        first run. Shared mode (`recipients = []`): it must be the *same* key on
        every machine, so point this at a secret you distribute (e.g. sops-nix).
        Per-machine mode: each machine keeps its own key and lists the other
        machines' recipients in `recipients`.
      '';
    };

    recipients = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = ''
        The other machines' age recipients (per-machine keys, multi-recipient
        encryption; this machine's own recipient is always included). Empty =
        shared-identity mode.
      '';
    };

    dataDir = lib.mkOption {
      type = lib.types.str;
      default = "${config.xdg.dataHome}/ssync";
      defaultText = lib.literalExpression ''"''${config.xdg.dataHome}/ssync"'';
      description = "ssync's own managed state (node key, blobs, docs, index).";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.ssync = {
      Unit = {
        Description = "ssync coding-agent session sync";
        After = [ "network-online.target" ];
      };
      Install.WantedBy = [ "default.target" ];
      Service = {
        ExecStart = "${cfg.package}/bin/ssync --config ${configFile} daemon";
        Restart = "on-failure";
        RestartSec = 5;
        # cap glibc malloc arenas: transient session read/encrypt buffers across
        # tokio workers otherwise pin the peak-import high-water mark as RSS.
        Environment = [ "MALLOC_ARENA_MAX=2" ];
      };
    };
  };
}
