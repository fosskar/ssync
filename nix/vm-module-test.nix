# Smoke-test the NixOS module: enabling `services.ssync` starts the daemon as a
# systemd service that comes up and creates its index namespace. The shared age
# key is generated at runtime (ExecStartPre), so no secret is embedded anywhere.
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
        sessionDir = "/root/sessions";
        ageIdentityFile = "/var/lib/ssync/age.key";
      };
      # generate a throwaway age key at boot if none exists (test only).
      systemd.services.ssync.serviceConfig.ExecStartPre = pkgs.writeShellScript "ssync-test-age-key" ''
        set -eu
        if [ ! -f /var/lib/ssync/age.key ]; then
          mkdir -p /var/lib/ssync
          umask 077
          ${pkgs.age}/bin/age-keygen -pq 2>/dev/null | grep AGE-SECRET-KEY > /var/lib/ssync/age.key
        fi
      '';
    };

  testScript = ''
    machine.wait_for_unit("ssync.service")
    machine.wait_for_file("/var/lib/ssync/ticket")
    machine.succeed("test -s /var/lib/ssync/ticket")
  '';
}
