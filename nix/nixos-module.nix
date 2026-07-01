# NixOS module: run `ssync daemon` as a system service for a given user. Wired
# from flake.nix as `nixosModules.default`.
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
      agent = "${cfg.agent}"
      session_dir = "${cfg.sessionDir}"
      age_identity_path = "${cfg.ageIdentityFile}"
      data_dir = "${cfg.dataDir}"
    ''
    + lib.optionalString (cfg.namespaceSecretFile != null) ''
      namespace_secret_path = "${cfg.namespaceSecretFile}"
    ''
    + lib.optionalString (cfg.nodeKeyFile != null) ''
      node_key_path = "${cfg.nodeKeyFile}"
    ''
    + lib.optionalString (cfg.peers != [ ]) ''
      peers = [ ${lib.concatMapStringsSep ", " (p: "\"${p}\"") cfg.peers} ]
    ''
  );
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
      description = ''
        User to run the daemon as; must own {option}`sessionDir`. Not a cross-user
        bridge: for projects under `$HOME` the username is part of the session key,
        so use the *same* username on every machine (see docs/identity.md).
      '';
    };

    agent = lib.mkOption {
      type = lib.types.str;
      default = "pi";
      description = "Agent whose sessions to sync (v1: pi).";
    };

    sessionDir = lib.mkOption {
      type = lib.types.str;
      default = "${config.users.users.${cfg.user}.home}/.pi/agent/sessions";
      defaultText = lib.literalExpression "\"\${user home}/.pi/agent/sessions\"";
      description = "The agent's session directory to watch. Defaults to pi's location.";
    };

    ageIdentityFile = lib.mkOption {
      type = lib.types.str;
      default = "${cfg.dataDir}/age.key";
      defaultText = lib.literalExpression "\"\${dataDir}/age.key\"";
      description = ''
        Shared age identity file. If it does not exist the daemon generates one
        on first run. It must be the *same* key on every machine, so for a
        multi-machine setup point this at a secret you distribute yourself (e.g.
        sops-nix). The clan service handles this for you via clan.vars.
      '';
    };

    dataDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/ssync";
      description = "ssync's own managed state (node key, blobs, docs, index).";
    };

    namespaceSecretFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Shared namespace secret file (same on every peer). When set, peers join
        one deterministic namespace with no ticket exchange. The clan service
        sets this via clan.vars.
      '';
    };

    nodeKeyFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "Override the iroh node key path (default: dataDir/node.key).";
    };

    peers = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Peer node-ids to sync with (the clan service fills these in).";
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
        StateDirectory = "ssync";
        Restart = "on-failure";
        RestartSec = 5;
      };
    };
  };
}
