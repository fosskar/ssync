# Cluster-artifact pairing with mDNS-only connectivity: peers are named by
# node-id alone — no ticket (so no embedded direct addresses), and the sandbox
# reaches neither the n0 relays nor DNS. The only way the nodes can find each
# other is mDNS address lookup on the virtual LAN. Also the e2e test of the
# `ssync cluster` flow (issue #23): init → add → distribute → join.
{
  pkgs,
  self,
  system,
}:
let
  ssync = self.packages.${system}.default;

  node = _: {
    environment.systemPackages = [ ssync ];
    # allow direct iroh connections and mDNS multicast over the test LAN
    networking.firewall.enable = false;
  };
in
pkgs.testers.runNixOSTest {
  name = "ssync-mdns-cluster";

  nodes.node1 = node;
  nodes.node2 = node;

  testScript = ''
    start_all()
    node1.wait_for_unit("multi-user.target")
    node2.wait_for_unit("multi-user.target")

    # swarm-discovery multicasts only on the default-route interface; the test
    # driver's eth0 (SLIRP) is per-VM isolated, so point the default route at
    # the shared vlan — like a real machine whose default route is its LAN.
    node1.succeed("ip route replace default dev eth1")
    node2.succeed("ip route replace default dev eth1")

    cfg = "--config /root/config.toml"

    def write_config(machine):
        machine.succeed(
            "umask 077; cat > /root/config.toml <<EOF\n"
            'age_identity_path = "/root/age.key"\n'
            'data_dir = "/var/lib/ssync"\n'
            "\n"
            "[[agents]]\n"
            'agent = "pi"\n'
            'session_dir = "/root/sessions"\n'
            "EOF\n"
        )

    write_config(node1)
    write_config(node2)

    # per-machine age keys and node keys; init prints both public halves
    def init_ids(machine):
        out = machine.succeed(f"ssync {cfg} init")
        rec = [l for l in out.splitlines() if l.startswith("age recipient: ")][0].split(": ", 1)[1]
        nid = [l for l in out.splitlines() if l.startswith("node-id: ")][0].split(": ", 1)[1]
        return rec, nid

    rec1, id1 = init_ids(node1)
    rec2, id2 = init_ids(node2)

    # cluster flow: init on node1, add node2, distribute, join on node2
    node1.succeed(f"ssync {cfg} cluster init")
    node1.succeed(f"ssync {cfg} cluster add {rec2} --node-id {id2}")
    cluster_b64 = node1.succeed("base64 -w0 /root/cluster.toml").strip()
    node2.succeed(f"umask 077; printf '%s' '{cluster_b64}' | base64 -d > /root/cluster-received.toml")
    node2.succeed(f"ssync {cfg} cluster join /root/cluster-received.toml")

    # both sides must derive the same namespace from the artifact
    ns1 = node1.succeed(f"ssync {cfg} cluster show | grep '^namespace:'").strip()
    ns2 = node2.succeed(f"ssync {cfg} cluster show | grep '^namespace:'").strip()
    assert ns1 == ns2, f"namespace mismatch: {ns1} != {ns2}"

    node1.succeed(f"systemd-run --unit ssyncd --collect ssync {cfg} daemon")
    node2.succeed(f"systemd-run --unit ssyncd --collect ssync {cfg} daemon")

    rel = "--proj--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl"
    src = f"/root/sessions/{rel}"

    # node1 -> node2 (allow for the 60s peer re-sync if the first dial races
    # mDNS discovery)
    node1.succeed("mkdir -p /root/sessions/--proj--")
    node1.succeed(f"printf 'hello over mdns\\n' > {src}")
    node2.wait_until_succeeds(f"test -f {src}", timeout=180)
    sum1 = node1.succeed(f"sha256sum {src} | cut -d' ' -f1").strip()
    sum2 = node2.succeed(f"sha256sum {src} | cut -d' ' -f1").strip()
    assert sum1 == sum2, f"content mismatch: {sum1} != {sum2}"

    # node2 -> node1 (mesh is symmetric, both directions must work)
    rel2 = "--proj--/2026-05-23T07-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl"
    src2 = f"/root/sessions/{rel2}"
    node2.succeed(f"printf 'hello back\\n' > {src2}")
    node1.wait_until_succeeds(f"test -f {src2}", timeout=180)
  '';
}
