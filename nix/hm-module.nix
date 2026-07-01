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
  configFile = pkgs.writeText "ssync-config.toml" ''
    agent = "${cfg.agent}"
    session_dir = "${cfg.sessionDir}"
    age_identity_path = "${cfg.ageIdentityFile}"
    data_dir = "${cfg.dataDir}"
  '';
in
{
  options.services.ssync = {
    enable = lib.mkEnableOption "ssync session-sync daemon (user service)";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "The ssync package to run.";
    };

    agent = lib.mkOption {
      type = lib.types.str;
      default = "pi";
      description = "Agent whose sessions to sync (v1: pi).";
    };

    sessionDir = lib.mkOption {
      type = lib.types.str;
      default = "${config.home.homeDirectory}/.pi/agent/sessions";
      defaultText = lib.literalExpression ''"''${config.home.homeDirectory}/.pi/agent/sessions"'';
      description = "The agent's session directory to watch (absolute path).";
    };

    ageIdentityFile = lib.mkOption {
      type = lib.types.str;
      description = ''
        Path to the shared age identity (same key on every machine). Provision it
        out of band (e.g. sops-nix); it must not be world-readable.
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
      };
    };
  };
}
