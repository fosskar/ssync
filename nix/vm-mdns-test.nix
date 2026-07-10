# Shared-namespace pairing with mDNS-only connectivity (issue #10): peers are
# named by node-id alone — no ticket (so no embedded direct addresses), and the
# sandbox reaches neither the n0 relays nor DNS. The only way the nodes can
# find each other is mDNS address lookup on the virtual LAN.
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
  name = "ssync-mdns-shared-namespace";

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

    # shared namespace secret: generate on node1, copy (binary) to node2
    node1.succeed("ssync keygen-namespace /root/ns.secret")
    ns_b64 = node1.succeed("base64 -w0 /root/ns.secret").strip()
    node2.succeed(f"umask 077; printf '%s' '{ns_b64}' | base64 -d > /root/ns.secret")

    # per-node iroh keys; each config lists only the OTHER node's node-id
    id1 = node1.succeed("ssync keygen-node /root/node.key").strip()
    id2 = node2.succeed("ssync keygen-node /root/node.key").strip()

    def write_config(machine, peer_id):
        machine.succeed(
            "umask 077; cat > /root/config.toml <<EOF\n"
            'age_identity_path = "/root/age.key"\n'
            'data_dir = "/var/lib/ssync"\n'
            'namespace_secret_path = "/root/ns.secret"\n'
            'node_key_path = "/root/node.key"\n'
            f'peers = [ "{peer_id}" ]\n'
            "\n"
            "[[agents]]\n"
            'agent = "pi"\n'
            'session_dir = "/root/sessions"\n'
            "EOF\n"
        )

    write_config(node1, id2)
    write_config(node2, id1)

    # shared age key: generate on node1, copy to node2
    node1.succeed(f"ssync {cfg} init")
    key = node1.succeed("cat /root/age.key").strip()
    node2.succeed(f"umask 077; printf '%s\\n' '{key}' > /root/age.key")

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
