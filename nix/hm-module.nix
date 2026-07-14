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
    + lib.optionalString (cfg.clusterFile != null) ''
      cluster_path = "${cfg.clusterFile}"
    ''
    + lib.optionalString (cfg.recipients != [ ]) ''
      recipients = [ ${
        lib.concatMapStringsSep ", " (r: "\"${lib.removeSuffix "\n" r}\"") cfg.recipients
      } ]
    ''
    + lib.optionalString (cfg.canonicalHome != null) ''
      canonical_home = "${cfg.canonicalHome}"
    ''
    + lib.concatMapStrings (m: ''
      [[path_map]]
      local = "${m.local}"
      canonical = "${m.canonical}"
    '') cfg.pathMap
    + lib.concatMapStrings (
      a:
      ''
        [[agents]]
        agent = "${a.agent}"
        session_dir = "${a.sessionDir}"
      ''
      + lib.optionalString (a.exclude != [ ]) ''
        exclude = [ ${lib.concatMapStringsSep ", " (p: "\"${p}\"") a.exclude} ]
      ''
    ) cfg.agents
  );
  cleanupArgs = lib.concatStringsSep " " (
    lib.optionals (cfg.autoCleanup.keep != null) [
      "--keep"
      cfg.autoCleanup.keep
    ]
    ++ lib.optional cfg.autoCleanup.unnamed "--unnamed"
  );
  # --- hardening (parity with the NixOS module and `ssync service install`,
  # crates/ssync/src/service.rs — change all three together) ---
  # The daemon needs: RW to the session dirs and dataDir (both under
  # $HOME), the RuntimeDirectory for age key temp files, read access to
  # the secrets it is pointed at, and outbound QUIC/UDP plus netlink for
  # iroh. Everything else is denied. Sandboxing in user units needs
  # unprivileged user namespaces (default on NixOS; some distros restrict).
  # The cleanup oneshot reuses the same set, but path grants are per unit:
  # sharing RuntimeDirectory would let the oneshot's exit remove it under
  # the running daemon (systemd#5394 — runtime dirs are not ref-counted),
  # and the oneshot only deletes session files, so no dataDir write access.
  hardening = {
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
    RestrictAddressFamilies = "AF_INET AF_INET6 AF_UNIX AF_NETLINK";
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
              description = "Agent name (see `adapter_for` for the supported set).";
            };
            sessionDir = lib.mkOption {
              type = lib.types.str;
              description = "The agent's watched directory (absolute path).";
            };
            exclude = lib.mkOption {
              type = lib.types.listOf lib.types.str;
              default = [ ];
              example = [ "*client-x*" ];
              description = ''
                `*`-glob patterns against the session-dir-relative path;
                matching sessions are withheld from sync on this machine and
                frozen everywhere (never published, materialized, or deleted).
              '';
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
        {
          # omp stores pasted images out-of-line in a content-addressed blob
          # store; sessions reference them, so blobs sync alongside.
          agent = "omp-blobs";
          sessionDir = "${config.home.homeDirectory}/.omp/agent/blobs";
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
        first run — that only works under `dataDir`, the one place outside the
        session dirs the hardened unit may write; a custom path must already
        exist (readable is enough). Shared mode (`recipients = []`): it must be
        the *same* key on every machine, so point this at a secret you
        distribute (e.g. sops-nix). Per-machine mode: each machine keeps its
        own key and lists the other machines' recipients in `recipients`.
      '';
    };

    pathMap = lib.mkOption {
      type = lib.types.listOf (
        lib.types.submodule {
          options = {
            local = lib.mkOption {
              type = lib.types.str;
              description = "This machine's path prefix (absolute).";
            };
            canonical = lib.mkOption {
              type = lib.types.str;
              description = "The mesh-wide canonical prefix (absolute, identical everywhere).";
            };
          };
        }
      );
      default = [ ];
      description = ''
        Prefix pairs bridging differing absolute paths (docs/setup.md,
        "Differing absolute paths"). Only machines whose paths differ from
        the canonical form need entries.
      '';
    };

    canonicalHome = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        The home dir canonical paths are relative to; required when an omp
        pathMap entry's canonical path lies under a home.
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

    clusterFile = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = ''
        Cluster membership artifact (same file on every peer): shared
        namespace secret, every machine's age recipient, and node-ids.
        Manage it with `ssync cluster`; mutually exclusive with `recipients`
        (the artifact carries them). Must be readable by the user.
      '';
    };

    dataDir = lib.mkOption {
      type = lib.types.str;
      default = "${config.xdg.dataHome}/ssync";
      defaultText = lib.literalExpression ''"''${config.xdg.dataHome}/ssync"'';
      description = "ssync's own managed state (node key, blobs, docs, index).";
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
      {
        assertion = cfg.clusterFile == null || cfg.recipients == [ ];
        message = "services.ssync.clusterFile replaces recipients — the artifact carries them";
      }
    ];

    # the daemon runs from the store path, but status/conflicts/ticket/cleanup
    # are user-facing — put the CLI on PATH and its config at the CLI's
    # default path.
    home.packages = [ cfg.package ];
    xdg.configFile."ssync/config.toml".source = configFile;

    # ReadWritePaths requires the paths to exist at unit start.
    systemd.user.tmpfiles.rules = map (d: "d \"${d}\" 0700 - - -") (
      (map (a: a.sessionDir) cfg.agents) ++ [ cfg.dataDir ]
    );

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
        Environment = [
          # cap glibc malloc arenas: transient session read/encrypt buffers across
          # tokio workers otherwise pin the peak-import high-water mark as RSS.
          "MALLOC_ARENA_MAX=2"
          # ssync-crypto's SecretFile prefers $XDG_RUNTIME_DIR, but
          # ProtectSystem=strict leaves /run/user/$UID read-only in a user unit;
          # point it at the unit's own (writable) RuntimeDirectory.
          "XDG_RUNTIME_DIR=%t/ssync"
        ];
        ReadWritePaths = (map (a: a.sessionDir) cfg.agents) ++ [ cfg.dataDir ];
        RuntimeDirectory = "ssync";
      }
      // hardening;
    };

    # scheduled cleanup: prune old sessions via the plain cleanup CLI; the
    # running daemon tombstones the deletions and every peer follows.
    systemd.user.services.ssync-cleanup = lib.mkIf cfg.autoCleanup.enable {
      Unit = {
        Description = "ssync scheduled session cleanup";
        After = [ "ssync.service" ];
      };
      Service = {
        Type = "oneshot";
        ExecStart = "${cfg.package}/bin/ssync --config ${configFile} cleanup ${cleanupArgs} --apply";
        ReadWritePaths = map (a: a.sessionDir) cfg.agents;
      }
      // hardening;
    };

    systemd.user.timers.ssync-cleanup = lib.mkIf cfg.autoCleanup.enable {
      Unit.Description = "ssync scheduled session cleanup timer";
      Timer = {
        OnCalendar = cfg.autoCleanup.schedule;
        # a machine that slept through the window catches up on next login
        Persistent = true;
        RandomizedDelaySec = "1h";
      };
      Install.WantedBy = [ "timers.target" ];
    };
  };
}
