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
        first run — that only works under `dataDir`, the one place outside the
        session dirs the hardened unit may write; a custom path must already
        exist (readable is enough). Shared mode (`recipients = []`): it must be
        the *same* key on every machine, so point this at a secret you
        distribute (e.g. sops-nix). Per-machine mode: each machine keeps its
        own key and lists the other machines' recipients in `recipients`.
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

        # --- hardening (parity with the NixOS module, user-manager adapted) ---
        # The daemon needs: RW to the session dirs and dataDir (both under
        # $HOME), the RuntimeDirectory for age key temp files, read access to
        # the secrets it is pointed at, and outbound QUIC/UDP plus netlink for
        # iroh. Everything else is denied. Sandboxing in user units needs
        # unprivileged user namespaces (default on NixOS; some distros restrict).
        ReadWritePaths = (map (a: a.sessionDir) cfg.agents) ++ [ cfg.dataDir ];
        RuntimeDirectory = "ssync";
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
    };
  };
}
