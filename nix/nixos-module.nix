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
  # scalar keys must precede the [[agents]] tables (TOML).
  configFile = pkgs.writeText "ssync-config.toml" (
    ''
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
      peers = [ ${lib.concatMapStringsSep ", " (p: "\"${lib.removeSuffix "\n" p}\"") cfg.peers} ]
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
    enable = lib.mkEnableOption "ssync session-sync daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      description = "The ssync package to run.";
    };

    user = lib.mkOption {
      type = lib.types.str;
      description = ''
        User to run the daemon as; must own the agents' session dirs. Not a cross-user
        bridge: for projects under `$HOME` the username is part of the session key,
        so use the *same* username on every machine (see docs/identity.md).
      '';
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
          sessionDir = "${config.users.users.${cfg.user}.home}/.pi/agent/sessions";
        }
        {
          agent = "omp";
          sessionDir = "${config.users.users.${cfg.user}.home}/.omp/agent/sessions";
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
        every machine, so point this at a secret you distribute yourself (e.g.
        sops-nix). Per-machine mode: each machine keeps its own key and lists
        the other machines' recipients in `recipients`. The clan service
        handles per-machine keys for you via clan.vars.
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

    recipients = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = ''
        The other machines' age recipients (per-machine keys, multi-recipient
        encryption; this machine's own recipient is always included). Empty =
        shared-identity mode. The clan service fills these in.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    # ensure the watched session dirs exist so the sandbox's ReadWritePaths bind
    # succeeds on first boot (owner cfg.user, 0700).
    systemd.tmpfiles.rules = map (a: "d ${a.sessionDir} 0700 ${cfg.user} - - -") cfg.agents;

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
        # cap glibc malloc arenas: transient session read/encrypt buffers across
        # tokio workers otherwise pin the peak-import high-water mark as RSS.
        Environment = [ "MALLOC_ARENA_MAX=2" ];

        # --- hardening (parity with the home-manager module and `ssync service
        # install`, crates/ssync/src/service.rs — change all three together) ---
        # The daemon needs: RW to the session dirs (under $HOME) and its StateDirectory,
        # read access to the secrets it is pointed at (/run/secrets, /nix/store),
        # and outbound QUIC/UDP plus netlink for iroh. Everything else is denied.
        ReadWritePaths = map (a: a.sessionDir) cfg.agents;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = "read-only";
        PrivateTmp = true;
        PrivateDevices = true;
        ProtectClock = true;
        ProtectHostname = true;
        ProtectKernelTunables = true;
        ProtectKernelModules = true;
        ProtectKernelLogs = true;
        ProtectControlGroups = true;
        ProtectProc = "invisible";
        ProcSubset = "pid";
        RestrictNamespaces = true;
        RestrictRealtime = true;
        RestrictSUIDSGID = true;
        RestrictAddressFamilies = [
          "AF_INET"
          "AF_INET6"
          "AF_UNIX"
          "AF_NETLINK"
        ];
        LockPersonality = true;
        MemoryDenyWriteExecute = true;
        RemoveIPC = true;
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
        SystemCallFilter = [
          "@system-service"
          "~@privileged"
          "~@resources"
        ];
        SystemCallErrorNumber = "EPERM";
        SystemCallArchitectures = "native";
        UMask = "0077";
      };
    };
  };
}
