# NixOS module: run `ssync daemon` as a system service for a given user.
# Wired from flake.nix as `nixosModules.default` (captures `self` for the default
# package). pi sessions are per-user, so `user` must own the session dir.
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
    enable = lib.mkEnableOption "ssync session-sync daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "The ssync package to run.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      description = "User to run the daemon as; must own {option}`sessionDir`.";
    };

    agent = lib.mkOption {
      type = lib.types.str;
      default = "pi";
      description = "Agent whose sessions to sync (v1: pi).";
    };

    sessionDir = lib.mkOption {
      type = lib.types.str;
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
      default = "/var/lib/ssync";
      description = "ssync's own managed state (node key, blobs, docs, index).";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.ssync = {
      description = "ssync coding-agent session sync";
      wantedBy = [ "multi-user.target" ];
      wants = [ "network-online.target" ];
      after = [ "network-online.target" ];
      serviceConfig = {
        ExecStart = "${cfg.package}/bin/ssync --config ${configFile} daemon";
        User = cfg.user;
        Restart = "on-failure";
        RestartSec = 5;
      };
    };
  };
}
