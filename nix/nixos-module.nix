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
  cleanupArgs = lib.concatStringsSep " " (
    lib.optionals (cfg.autoCleanup.keep != null) [
      "--keep"
      cfg.autoCleanup.keep
    ]
    ++ lib.optional cfg.autoCleanup.unnamed "--unnamed"
  );
  # --- hardening (parity with the home-manager module and `ssync service
  # install`, crates/ssync/src/service.rs — change all three together) ---
  # The daemon needs: RW to the session dirs (under $HOME) and its StateDirectory,
  # read access to the secrets it is pointed at (/run/secrets, /nix/store),
  # and outbound QUIC/UDP plus netlink for iroh. Everything else is denied.
  # The cleanup oneshot reuses the same set (it only deletes session files).
  hardening = {
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
              description = "Agent name (see `adapter_for` for the supported set).";
            };
            sessionDir = lib.mkOption {
              type = lib.types.str;
              description = "The agent's watched directory (absolute path).";
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
        {
          # omp stores pasted images out-of-line in a content-addressed blob
          # store; sessions reference them, so blobs sync alongside.
          agent = "omp-blobs";
          sessionDir = "${config.users.users.${cfg.user}.home}/.omp/agent/blobs";
        }
      ];
      defaultText = lib.literalExpression "pi and omp (sessions + blob store) at the user's home";
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

    autoCleanup = {
      enable = lib.mkEnableOption ''
        scheduled session cleanup. Deletions propagate MESH-WIDE: the daemon
        tombstones every pruned session and all peers delete their copies, so
        a timer on one machine prunes all machines (enabling it on several is
        harmless but redundant)
      '';

      schedule = lib.mkOption {
        type = lib.types.str;
        default = "weekly";
        example = "*-*-* 03:00:00";
        description = "systemd `OnCalendar` expression for the cleanup timer.";
      };

      keep = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = "90d";
        description = ''
          Delete sessions created more than this long ago (`ssync cleanup
          --keep`; `<n>` + `d`/`w`/`m`/`y`). Null to select by `unnamed` only.
        '';
      };

      unnamed = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Also delete sessions whose title record is present but empty
          (combines with `keep` as AND).
        '';
      };
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = !cfg.autoCleanup.enable || cfg.autoCleanup.keep != null || cfg.autoCleanup.unnamed;
        message = "services.ssync.autoCleanup selects nothing: set keep or unnamed";
      }
    ];

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
      }
      // hardening;
    };

    # scheduled cleanup: prune old sessions via the plain cleanup CLI; the
    # running daemon tombstones the deletions and every peer follows.
    systemd.services.ssync-cleanup = lib.mkIf cfg.autoCleanup.enable {
      description = "ssync scheduled session cleanup";
      after = [ "ssync.service" ];
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${cfg.package}/bin/ssync --config ${configFile} cleanup ${cleanupArgs} --apply";
        User = cfg.user;
      }
      // hardening;
    };

    systemd.timers.ssync-cleanup = lib.mkIf cfg.autoCleanup.enable {
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnCalendar = cfg.autoCleanup.schedule;
        # a machine that slept through the window catches up on next boot
        Persistent = true;
        RandomizedDelaySec = "1h";
      };
    };
  };
}
