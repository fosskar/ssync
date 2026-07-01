# Smoke-test the NixOS module: enabling `services.ssync` starts the daemon, which
# auto-generates its age key and index namespace and comes up as a service.
{
  pkgs,
  self,
  system,
}:
pkgs.testers.runNixOSTest {
  name = "ssync-nixos-module";

  nodes.machine =
    { ... }:
    {
      imports = [ self.nixosModules.default ];
      services.ssync = {
        enable = true;
        user = "root";
      };
    };

  testScript = ''
    machine.wait_for_unit("ssync.service")
    machine.wait_for_file("/var/lib/ssync/age.key")
    machine.wait_for_file("/var/lib/ssync/ticket")
    machine.succeed("test -s /var/lib/ssync/ticket")
  '';
}
