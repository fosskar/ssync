{
  pkgs,
  self,
  system,
  treefmtEval,
}:
{
  # `nix build .#default` (buildRustPackage) already runs the test suite in its
  # checkPhase, so building it is the test check too.
  package = self.packages.${system}.default;
  devshell = self.devShells.${system}.default;
  formatting = treefmtEval.config.build.check self;

  # close-to-real end-to-end: two NixOS VMs syncing a session over a virtual LAN.
  vm-sync = import ./vm-test.nix { inherit pkgs self system; };

  # the NixOS module brings the daemon up as a service.
  vm-module = import ./vm-module-test.nix { inherit pkgs self; };
}
