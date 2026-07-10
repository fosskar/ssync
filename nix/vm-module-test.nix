# Smoke-test the NixOS module: enabling `services.ssync` starts the daemon, which
# auto-generates its age key and index namespace and comes up as a service. Also
# covers per-machine mode: `recipients` must land in the rendered config and the
# daemon must come up encrypting to itself plus that peer. `autoCleanup` must
# install an enabled timer whose service runs `cleanup --apply` with the
# configured selectors.
{
  pkgs,
  self,
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
        # a real (throwaway) peer recipient: per-machine mode; the daemon still
        # generates its own key and must start normally.
        recipients = [ "age10p370q7mfmpxpxxxuz765r7ddhcgr25uxthwtfcpd6ylg8mx5pmqt9mkc9" ];
        autoCleanup = {
          enable = true;
          schedule = "daily";
          unnamed = true;
        };
      };
    };

  testScript = ''
    machine.wait_for_unit("ssync.service")
    machine.wait_for_file("/var/lib/ssync/age.key")
    machine.wait_for_file("/var/lib/ssync/ticket")
    machine.succeed("test -s /var/lib/ssync/ticket")
    cfg = machine.succeed("systemctl cat ssync | grep -oP '(?<=--config )\S+'").strip()
    machine.succeed(f"grep -q 'recipients = ' {cfg}")

    machine.wait_for_unit("ssync-cleanup.timer")
    machine.succeed("systemctl list-timers ssync-cleanup.timer | grep -q ssync-cleanup")
    exec = machine.succeed("systemctl cat ssync-cleanup.service | grep ExecStart").strip()
    assert "cleanup --keep 90d --unnamed --apply" in exec, exec
    # oneshot must actually run clean (dry selectors, empty session dirs)
    machine.succeed("systemctl start ssync-cleanup.service")
  '';
}
