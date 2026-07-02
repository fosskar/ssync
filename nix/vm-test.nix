# Two real NixOS machines on a virtual LAN, each running `ssync daemon` with the
# same shared age key. node2 joins node1's pairing ticket, then a session file
# written on node1 must appear byte-identical on node2 — the close-to-real M2 gate.
#
# Plain nixpkgs runNixOSTest: no clan, no external infra. n0 relay/DNS are
# unreachable in the sandbox, but pairing tickets carry direct addresses so the
# nodes connect over the test LAN directly.
{
  pkgs,
  self,
  system,
}:
let
  ssync = self.packages.${system}.default;

  configToml = pkgs.writeText "config.toml" ''
    age_identity_path = "/root/age.key"
    data_dir = "/var/lib/ssync"

    [[agents]]
    agent = "pi"
    session_dir = "/root/sessions"
  '';

  node = _: {
    environment.systemPackages = [ ssync ];
    environment.etc."ssync/config.toml".source = configToml;
    # allow direct iroh connections over the test LAN
    networking.firewall.enable = false;
  };
in
pkgs.testers.runNixOSTest {
  name = "ssync-two-node-sync";

  nodes.node1 = node;
  nodes.node2 = node;

  testScript = ''
    start_all()
    node1.wait_for_unit("multi-user.target")
    node2.wait_for_unit("multi-user.target")

    cfg = "--config /etc/ssync/config.toml"
    rel = "--proj--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl"
    src = f"/root/sessions/{rel}"

    # generate the shared age key at runtime on node1, then copy it to node2
    # (same key on every machine, as on one user's own machines).
    node1.succeed(f"ssync {cfg} init")
    key = node1.succeed("cat /root/age.key").strip()
    node2.succeed(f"umask 077; printf '%s\\n' '{key}' > /root/age.key")

    # start the daemon on node1; it creates a namespace and writes its ticket
    node1.succeed(f"systemd-run --unit ssyncd --collect ssync {cfg} daemon")
    node1.wait_for_file("/var/lib/ssync/ticket")
    ticket = node1.succeed(f"ssync {cfg} ticket").strip()

    # pair node2 to node1, then start node2's daemon
    node2.succeed(f"ssync {cfg} join '{ticket}'")
    node2.succeed(f"systemd-run --unit ssyncd --collect ssync {cfg} daemon")

    # give node2 a moment to finish joining + subscribing
    node2.sleep(5)

    # create a session on node1
    node1.succeed("mkdir -p /root/sessions/--proj--")
    node1.succeed(f"printf 'hello from node1\\n' > {src}")

    # it must appear, byte-identical, on node2
    node2.wait_until_succeeds(f"test -f {src}", timeout=120)
    sum1 = node1.succeed(f"sha256sum {src} | cut -d' ' -f1").strip()
    sum2 = node2.succeed(f"sha256sum {src} | cut -d' ' -f1").strip()
    assert sum1 == sum2, f"content mismatch: {sum1} != {sum2}"

    # loop-prevention: node2's own write-back must not bounce back as a false
    # conflict (age ciphertext is randomized, so dedup is on plaintext).
    node2.sleep(5)
    node1.succeed(f"ssync {cfg} conflicts | grep -q 'no conflicts'")
    node2.succeed(f"ssync {cfg} conflicts | grep -q 'no conflicts'")
  '';
}
