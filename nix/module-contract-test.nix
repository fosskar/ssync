{ pkgs, self }:
let
  inherit (pkgs) lib;
  agents = [
    {
      agent = "pi";
      sessionDir = "/sessions";
    }
  ];
  nixosConfig =
    extra:
    import (pkgs.path + "/nixos/lib/eval-config.nix") {
      system = null;
      modules = [
        { nixpkgs.hostPlatform = pkgs.stdenv.hostPlatform; }
        self.nixosModules.default
        {
          services.ssync = {
            enable = true;
            user = "root";
            inherit agents;
          }
          // extra;
        }
      ];
    };
  nixosService = extra: (nixosConfig extra).config.systemd.services.ssync.serviceConfig;
  nixosDefault = nixosService { };
  nixosCustom = nixosService { dataDir = "/srv/ssync-data"; };
  nixosExternal = nixosService {
    dataDir = "/srv/ssync-data";
    ageIdentityFile = "/run/secrets/age.key";
    nodeKeyFile = "/run/secrets/node.key";
    clusterFile = "/run/secrets/cluster.toml";
  };
  unsafeDataDirsRejected =
    lib.all
      (
        dataDir:
        !(builtins.tryEval (nixosConfig { inherit dataDir; }).config.system.build.toplevel.drvPath).success
      )
      [
        "/srv"
        "/srv/"
      ];

  hmStub =
    { lib, ... }:
    {
      options = {
        assertions = lib.mkOption {
          type = lib.types.listOf lib.types.anything;
          default = [ ];
        };
        home.packages = lib.mkOption {
          type = lib.types.listOf lib.types.package;
          default = [ ];
        };
        home.homeDirectory = lib.mkOption { type = lib.types.str; };
        home.username = lib.mkOption { type = lib.types.str; };
        xdg.dataHome = lib.mkOption { type = lib.types.str; };
        xdg.configFile = lib.mkOption {
          type = lib.types.attrsOf lib.types.anything;
          default = { };
        };
        systemd.user.tmpfiles.rules = lib.mkOption {
          type = lib.types.listOf lib.types.str;
          default = [ ];
        };
        systemd.user.services = lib.mkOption {
          type = lib.types.attrsOf lib.types.anything;
          default = { };
        };
        systemd.user.timers = lib.mkOption {
          type = lib.types.attrsOf lib.types.anything;
          default = { };
        };
      };
      config = {
        home.homeDirectory = "/home/alice";
        home.username = "alice";
        xdg.dataHome = "/home/alice/.local/share";
      };
    };
  hmService =
    (lib.evalModules {
      specialArgs = { inherit pkgs; };
      modules = [
        hmStub
        self.homeManagerModules.default
        {
          services.ssync = {
            enable = true;
            inherit agents;
          };
        }
      ];
    }).config.systemd.user.services.ssync.Service;

  hardeningContract =
    service:
    assert service.NoNewPrivileges;
    assert service.ProtectSystem == "strict";
    assert service.ProtectHome == "read-only";
    assert service.PrivateTmp;
    assert service.PrivateDevices;
    assert service.ProtectClock;
    assert service.ProtectHostname;
    assert service.ProtectKernelTunables;
    assert service.ProtectKernelModules;
    assert service.ProtectKernelLogs;
    assert service.ProtectControlGroups;
    assert service.ProtectProc == "invisible";
    assert service.ProcSubset == "pid";
    assert service.RestrictNamespaces;
    assert service.RestrictRealtime;
    assert service.RestrictSUIDSGID;
    assert
      (
        if builtins.isList service.RestrictAddressFamilies then
          service.RestrictAddressFamilies
        else
          lib.splitString " " service.RestrictAddressFamilies
      ) == [
        "AF_INET"
        "AF_INET6"
        "AF_UNIX"
        "AF_NETLINK"
      ];
    assert service.LockPersonality;
    assert service.MemoryDenyWriteExecute;
    assert service.RemoveIPC;
    assert service.CapabilityBoundingSet == "";
    assert service.AmbientCapabilities == "";
    assert
      service.SystemCallFilter == [
        "@system-service"
        "~@privileged"
        "~@resources"
      ];
    assert service.SystemCallErrorNumber == "EPERM";
    assert service.SystemCallArchitectures == "native";
    assert service.UMask == "0077";
    true;
in
assert unsafeDataDirsRejected;
assert hardeningContract nixosDefault;
assert hardeningContract hmService;
assert nixosDefault.StateDirectory == "ssync";
assert nixosDefault.StateDirectoryMode == "0700";
assert nixosDefault.ReadWritePaths == [ "/sessions" ];
assert !(nixosCustom ? StateDirectory);
assert
  nixosCustom.ReadWritePaths == [
    "/sessions"
    "/srv/ssync-data"
  ];
assert nixosExternal.ReadWritePaths == nixosCustom.ReadWritePaths;
assert
  hmService.ReadWritePaths == [
    "/sessions"
    "/home/alice/.local/share/ssync"
  ];
pkgs.runCommand "ssync-module-contract" { } "touch $out"
