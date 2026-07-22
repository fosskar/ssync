# Smoke-test the NixOS module: enabling `services.ssync` starts the daemon, which
# auto-generates its age key and index namespace and comes up as a service. Also
# covers per-machine mode: `recipients` must land in the rendered config and the
# daemon must come up encrypting to itself plus that peer. `autoCleanup` must
# install an enabled timer whose service runs `cleanup --apply` with the
# configured selectors.
#
# The `cluster` node replicates exactly what the clan service composes (minus
# clan.vars): a shared namespace secret on disk, `clusterFile` pointing into the
# RuntimeDirectory, and a preStart that assembles cluster.toml via
# `ssync cluster render --secret-file` — proving the assembly runs inside the
# hardened sandbox and the daemon consumes the artifact.
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

  nodes.cluster =
    { config, ... }:
    {
      imports = [ self.nixosModules.default ];
      services.ssync = {
        enable = true;
        user = "root";
        clusterFile = "/run/ssync/cluster.toml";
      };
      # the clan service's assembly shape (nix/clan-service.nix): secret from
      # vars (here: generated once into the StateDirectory), members at eval.
      systemd.services.ssync.serviceConfig.RuntimeDirectory = "ssync";
      systemd.services.ssync.preStart = ''
        [ -f /var/lib/ssync/ns.secret ] || ${config.services.ssync.package}/bin/ssync keygen-namespace /var/lib/ssync/ns.secret
        ${config.services.ssync.package}/bin/ssync cluster render \
          --out /run/ssync/cluster.toml \
          --secret-file /var/lib/ssync/ns.secret \
          "age10p370q7mfmpxpxxxuz765r7ddhcgr25uxthwtfcpd6ylg8mx5pmqt9mkc9:ee9d0d54bd6d0f24b56784927a4ef7f7f65b1b6a9d1b8fef9a94ea8c8f4a2f2a"
      '';
    };

  nodes.custom =
    { pkgs, ... }:
    {
      imports = [ self.nixosModules.default ];
      users.groups.ssync-test = { };
      users.users.ssync-test = {
        isSystemUser = true;
        group = "ssync-test";
      };
      services.ssync = {
        enable = true;
        user = "ssync-test";
        ageIdentityFile = "/run/ssync-test-key/age.key";
        dataDir = "/srv/ssync-data";
      };
      system.activationScripts.ssyncExternalAge = ''
        install -d -m 0700 -o ssync-test -g ssync-test /run/ssync-test-key
        ${pkgs.age}/bin/age-keygen -pq -o /run/ssync-test-key/age.key
        chmod 0600 /run/ssync-test-key/age.key
        chown ssync-test:ssync-test /run/ssync-test-key/age.key
      '';
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

    # clan-shape assembly: preStart renders the artifact inside the sandbox,
    # the daemon opens the shared namespace from it (no ticket file in
    # cluster mode), and the artifact is private.
    cluster.wait_for_unit("ssync.service")
    cluster.wait_for_file("/run/ssync/cluster.toml")
    cluster.succeed("test $(stat -c%a /run/ssync/cluster.toml) = 600")
    ccfg = cluster.succeed("systemctl cat ssync | grep -oP '(?<=--config )\S+'").strip()
    cluster.succeed(f"grep -q 'cluster_path = ' {ccfg}")
    cluster.succeed(f"! grep -q 'recipients = ' {ccfg}")
    cluster.wait_until_succeeds("journalctl -u ssync | grep -q 'cluster namespace'", timeout=60)
    cluster.succeed("! test -e /var/lib/ssync/ticket")

    # A custom dataDir carries its storage ownership and sandbox grant with it.
    custom.wait_for_unit("ssync.service")
    custom.wait_for_file("/run/ssync-test-key/age.key")
    custom.wait_for_file("/srv/ssync-data/ticket")
    custom.succeed("test $(stat -c%a /srv/ssync-data) = 700")
    custom.succeed("test $(stat -c%U /srv/ssync-data) = ssync-test")
    custom.succeed("systemctl show ssync -p ReadWritePaths --value | grep -q /srv/ssync-data")
    custom.succeed("systemctl show ssync -p ReadWritePaths --value | grep -qv /run/ssync-test-key")
    custom.succeed("test -z \"$(systemctl show ssync -p StateDirectory --value)\"")
  '';
}
