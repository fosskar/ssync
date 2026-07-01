{
  description = "ssync — p2p sync of coding-agent session files";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      treefmt-nix,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forEachSystem = nixpkgs.lib.genAttrs systems;
      pkgsForEach = nixpkgs.legacyPackages;
      treefmtEval = forEachSystem (
        system: treefmt-nix.lib.evalModule pkgsForEach.${system} ./nix/treefmt.nix
      );
    in
    rec {
      packages = forEachSystem (system: {
        default = pkgsForEach.${system}.callPackage ./nix/package.nix { };
      });

      devShells = forEachSystem (system: {
        default = pkgsForEach.${system}.callPackage ./nix/devshell.nix { };
      });

      nixosModules.default = import ./nix/nixos-module.nix { inherit self; };
      homeManagerModules.default = import ./nix/hm-module.nix { inherit self; };
      # clan resolves external services via the `clan.modules.<name>` output.
      clan.modules.ssync = import ./nix/clan-service.nix { inherit self; };

      formatter = forEachSystem (system: treefmtEval.${system}.config.build.wrapper);

      checks = forEachSystem (
        system:
        import ./nix/checks.nix {
          inherit self system;
          pkgs = pkgsForEach.${system};
          treefmtEval = treefmtEval.${system};
        }
      );

      hydraJobs = packages;
    };
}
