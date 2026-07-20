//! M2 gate: a session created on one node appears, decrypted and byte-identical,
//! in the other nodes' session directories after they join the index namespace —
//! no manual copy.
//!
//! Two (or three, for the multi-recipient mesh) in-process iroh nodes, shared or
//! per-machine age identities. All mutation goes through `tick_once`/`run` — the
//! production path. Uses poll loops because sync is asynchronous over the network.

mod harness;

use std::path::Path;
use std::time::Duration;

use harness::*;
use ssync_adapters::Adapter;
use ssync_adapters::blob_store::BlobStoreAdapter;
use ssync_adapters::pi::PiAdapter;
use ssync_crypto::AgeIdentity;
use ssync_net::iroh::SecretKey;

#[tokio::test]
async fn session_created_on_a_appears_on_b() {
    let sim = Sim::new("base");

    // --- node A: has a real session file ---
    let rel = "--home-simon-Projects-demo--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
    let contents = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"hello from A\"}\n";

    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut peer_a = sim.pi_peer("a", "pi", node_a);
    peer_a.write(rel, contents);
    peer_a.tick().await;

    // --- node B: empty session dir, joins A's namespace ---
    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let mut peer_b = sim.pi_peer("b", "pi", node_b);

    // sync is async: tick B until the file materializes, byte-identical.
    let dest = peer_b.path(rel);
    let ok = converge(&mut [&mut peer_b], || file_eq(&dest, contents)).await;
    assert!(ok, "session did not sync to node B within timeout");
}

#[tokio::test]
async fn per_machine_identities_sync_both_directions() {
    let sim = Sim::new("permachine");
    let id_a = AgeIdentity::generate().unwrap();
    let id_b = AgeIdentity::generate().unwrap();

    // --- node A: own key, B listed as recipient, has a session ---
    let rel_a = "--home-simon-Projects-demo--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl";
    let contents_a = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"from A\"}\n";

    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut ident_a = AgeIdentity::from_secret_string(&id_a.to_secret_string()).unwrap();
    ident_a.add_recipients([id_b.recipient_string()]);
    let mut peer_a = sim.pi_peer_as("a", "pi", ident_a, node_a);
    peer_a.write(rel_a, contents_a);
    peer_a.tick().await;

    // --- node B: own key, A listed as recipient ---
    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let mut ident_b = AgeIdentity::from_secret_string(&id_b.to_secret_string()).unwrap();
    ident_b.add_recipients([id_a.recipient_string()]);
    let mut peer_b = sim.pi_peer_as("b", "pi", ident_b, node_b);

    // A → B: B decrypts A's blob with its own key.
    let dest_a = peer_b.path(rel_a);
    let ok = converge(&mut [&mut peer_b], || file_eq(&dest_a, contents_a)).await;
    assert!(ok, "A's session did not reach B under per-machine keys");

    // B → A: symmetric direction.
    let rel_b = "--home-simon-Projects-demo--/2026-05-23T07-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4c.jsonl";
    let contents_b = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"from B\"}\n";
    peer_b.write(rel_b, contents_b);
    let dest_b = peer_a.path(rel_b);
    let ok = converge(&mut [&mut peer_b, &mut peer_a], || {
        file_eq(&dest_b, contents_b)
    })
    .await;
    assert!(ok, "B's session did not reach A under per-machine keys");
}

#[tokio::test]
async fn per_machine_identities_reach_a_third_machine() {
    // recipients are not pairwise: a blob authored on A may reach C via B, so
    // every machine must encrypt to *all* peers. three nodes, full recipient
    // mesh, one shared namespace — a session from A must land on B and C, and
    // one from C must land on A and B.
    let sim = Sim::new("threenode");
    let ids: Vec<AgeIdentity> = (0..3).map(|_| AgeIdentity::generate().unwrap()).collect();
    let recipients: Vec<String> = ids.iter().map(|i| i.recipient_string()).collect();
    let ident = |n: usize| {
        let mut id = AgeIdentity::from_secret_string(&ids[n].to_secret_string()).unwrap();
        id.add_recipients(recipients.clone());
        id
    };

    let rel_a = "--home-simon-Projects-demo--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4d.jsonl";
    let contents_a = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"from A\"}\n";

    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket_b = node_a.share().await.unwrap();
    let ticket_c = node_a.share().await.unwrap();
    let mut peer_a = sim.pi_peer_as("a", "pi", ident(0), node_a);
    peer_a.write(rel_a, contents_a);
    peer_a.tick().await;

    let mut node_b = sim.node("b").await;
    node_b.join(ticket_b).await.unwrap();
    let mut peer_b = sim.pi_peer_as("b", "pi", ident(1), node_b);

    let mut node_c = sim.node("c").await;
    node_c.join(ticket_c).await.unwrap();
    let mut peer_c = sim.pi_peer_as("c", "pi", ident(2), node_c);

    // A → B and A → C: both peers decrypt A's blob with their own keys.
    let dest_b = peer_b.path(rel_a);
    let dest_c = peer_c.path(rel_a);
    let ok = converge(&mut [&mut peer_b, &mut peer_c], || {
        file_eq(&dest_b, contents_a) && file_eq(&dest_c, contents_a)
    })
    .await;
    assert!(ok, "A's session did not reach both B and C");

    // C → A and C → B: the reverse direction from a joined (non-author) node.
    let rel_c = "--home-simon-Projects-demo--/2026-05-23T07-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4e.jsonl";
    let contents_c = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"from C\"}\n";
    peer_c.write(rel_c, contents_c);
    let dest_a2 = peer_a.path(rel_c);
    let dest_b2 = peer_b.path(rel_c);
    let ok = converge(&mut [&mut peer_a, &mut peer_b, &mut peer_c], || {
        file_eq(&dest_a2, contents_c) && file_eq(&dest_b2, contents_c)
    })
    .await;
    assert!(ok, "C's session did not reach both A and B");
}

#[tokio::test]
async fn live_write_propagates_without_restart() {
    let sim = Sim::new("live");

    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let peer_a = sim.pi_peer("a", "pi", node_a);

    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let peer_b = sim.pi_peer("b", "pi", node_b);

    // start both daemons and let them enter their loops
    let root_a = peer_a.run();
    let root_b = peer_b.run();
    tokio::time::sleep(Duration::from_secs(2)).await;

    // write a NEW session on A *after* startup (also a brand-new project dir)
    let rel = "--proj--/2026-07-01T00-00-00-000Z_019e9999-eeee-71ac-be20-livewrite00001.jsonl";
    let contents = b"live write after start\n";
    write_file(&root_a.join(rel), contents);

    let dest = root_b.join(rel);
    let ok = eventually(|| file_eq(&dest, contents)).await;
    assert!(ok, "live write after startup did not propagate to node B");
}

#[tokio::test]
async fn shared_namespace_auto_connects_without_ticket() {
    let sim = Sim::new("shared");
    let ns_secret = ssync_net::generate_key_bytes();

    let mut node_a = sim.node("a").await;
    let mut node_b = sim.node("b").await;

    // the same secret must yield the same namespace on both — no ticket exchange
    let id_a = node_a.open_shared_namespace(ns_secret).await.unwrap();
    let id_b = node_b.open_shared_namespace(ns_secret).await.unwrap();
    assert_eq!(id_a, id_b, "shared secret must yield the same namespace");

    // connect the two peers (addresses here; in production resolved from node-ids)
    let addr_a = node_a.endpoint_addr();
    let addr_b = node_b.endpoint_addr();
    node_a.sync_with(vec![addr_b]).await.unwrap();
    node_b.sync_with(vec![addr_a]).await.unwrap();

    let peer_a = sim.pi_peer("a", "pi", node_a);
    let peer_b = sim.pi_peer("b", "pi", node_b);

    let root_a = peer_a.run();
    let root_b = peer_b.run();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let rel = "--proj--/2026-01-01T00-00-00-000Z_019eshared0001eeee71acbe20shared01.jsonl";
    let contents = b"shared namespace session\n";
    write_file(&root_a.join(rel), contents);

    let dest = root_b.join(rel);
    let mut ok = false;
    for _ in 0..80 {
        if file_eq(&dest, contents) {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "session did not sync over the shared namespace");
}

#[tokio::test]
async fn deletion_propagates_and_does_not_resurrect() {
    let sim = Sim::new("del");
    let rel = "--proj--/2026-01-01T00-00-00-000Z_019edddd0001eeee71acbe20delete001.jsonl";

    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let peer_a = sim.pi_peer("a", "pi", node_a);
    // keep a second unrelated session so the dir is never empty (deletion guard)
    peer_a.write(
        "--proj--/keep_019e0000keepkeepkeep71acbe20keep00001.jsonl",
        b"keep\n",
    );

    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let peer_b = sim.pi_peer("b", "pi", node_b);

    peer_a.write(rel, b"header\nto-be-deleted\n");
    // the session's artifact dir (omp subagent transcript) syncs and must be
    // swept on B once the deletion empties it.
    let artifact_dir_rel = rel.strip_suffix(".jsonl").unwrap();
    let artifact_rel = format!("{artifact_dir_rel}/Sub.jsonl");
    peer_a.write(&artifact_rel, b"header\nsub\n");

    let root_a = peer_a.run();
    let root_b = peer_b.run();

    // both files appear on B
    let dest = root_b.join(rel);
    let dest_artifact = root_b.join(&artifact_rel);
    assert!(
        eventually(|| dest.exists() && dest_artifact.exists()).await,
        "session (and artifact) never reached B"
    );

    // delete on A (file victims only, as cleanup does) -> must disappear on B,
    // including the emptied artifact dir, and stay gone
    std::fs::remove_file(root_a.join(&artifact_rel)).unwrap();
    std::fs::remove_file(root_a.join(rel)).unwrap();
    assert!(
        eventually(|| !dest.exists()).await,
        "deletion did not propagate to B"
    );
    assert!(
        eventually(|| !root_b.join(artifact_dir_rel).exists()).await,
        "emptied artifact dir not removed on B"
    );

    // confirm it does not resurrect
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!dest.exists(), "deleted session resurrected on B");
}

#[tokio::test]
async fn deletion_by_non_author_propagates_back() {
    // a session created on A, deleted on B, must disappear on A (and stay gone)
    // even though A authored the index entry (TODO "deletion by any participant").
    let sim = Sim::new("xdel");
    let rel = "--proj--/2026-01-01T00-00-00-000Z_019exdel0001eeee71acbe20xdele001.jsonl";

    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let peer_a = sim.pi_peer("a", "pi", node_a);
    // second session so neither dir ever goes empty (deletion guard)
    peer_a.write(
        "--proj--/keep_019e0000keepkeepkeep71acbe20keep00001.jsonl",
        b"keep\n",
    );
    peer_a.write(rel, b"header\nauthored on A\n");

    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let peer_b = sim.pi_peer("b", "pi", node_b);

    let root_a = peer_a.run();
    let root_b = peer_b.run();

    // both sessions reach B (the keep session too, so B's dir never goes
    // empty after the delete — the empty-dir guard would swallow the tombstone)
    let on_b = root_b.join(rel);
    let keep_on_b = root_b.join("--proj--/keep_019e0000keepkeepkeep71acbe20keep00001.jsonl");
    assert!(
        eventually(|| on_b.exists() && keep_on_b.exists()).await,
        "sessions never reached B"
    );

    // delete on B (NOT the author) -> must disappear on A and stay gone
    std::fs::remove_file(&on_b).unwrap();
    let on_a = root_a.join(rel);
    assert!(
        eventually(|| !on_a.exists()).await,
        "deletion by non-author did not propagate back to A"
    );

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!on_a.exists(), "deleted session resurrected on A");
    assert!(!on_b.exists(), "deleted session resurrected on B");
}

#[tokio::test]
async fn deletion_while_daemon_down_is_not_reimported() {
    // engine 1 imports two sessions and persists its state; one file is
    // deleted "while the daemon is down"; engine 2 (same state file, same
    // node dir) must tombstone the deleted session instead of re-importing it.
    let sim = Sim::new("down-del");
    let rel = "--proj--/2026-01-01T00-00-00-000Z_019edown0001eeee71acbe20downdel01.jsonl";
    let keep = "--proj--/keep_019e0000keepkeepkeep71acbe20keep00001.jsonl";
    let state_path = sim.base.join("state.toml");

    let ns = {
        let mut node = sim.node("n").await;
        let ns = node.create_namespace().await.unwrap();
        let mut peer = sim.pi_peer("n", "pi", node);
        peer.write(rel, b"header\ndelete me\n");
        peer.write(keep, b"keep\n");
        peer.engine.persist_state(&state_path);
        peer.tick().await;
        peer.shutdown().await.unwrap();
        ns
    };

    // daemon down: the session file disappears
    std::fs::remove_file(sim.root("n").join(rel)).unwrap();

    let mut node = sim.node("n").await;
    node.open_namespace(ns).await.unwrap();
    let mut peer = sim.pi_peer("n", "pi", node);
    peer.engine.persist_state(&state_path);
    peer.tick().await;

    // the key must now be a tombstone (deleted), not a live re-import
    let report = peer.engine.status_report().await.unwrap();
    assert_eq!(report.sessions, 1, "deleted session was re-imported");
}

#[tokio::test]
async fn divergent_sessions_merge_and_converge() {
    let sim = Sim::new("merge");
    let rel = "--proj--/2026-01-01T00-00-00-000Z_019eccccdddd71acbe20merge0000001.jsonl";

    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let peer_a = sim.pi_peer("a", "pi", node_a);

    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let peer_b = sim.pi_peer("b", "pi", node_b);

    // each machine has its own divergent version of the same session
    peer_a.write(rel, b"header\ncommon\nonly-on-a\n");
    peer_b.write(rel, b"header\ncommon\nonly-on-b\n");

    let root_a = peer_a.run();
    let root_b = peer_b.run();

    // both files must converge to the lossless union (nothing dropped)
    let want_lines = ["header", "common", "only-on-a", "only-on-b"];
    let mut ok = false;
    for _ in 0..80 {
        let a = std::fs::read_to_string(root_a.join(rel)).unwrap_or_default();
        let b = std::fs::read_to_string(root_b.join(rel)).unwrap_or_default();
        let a_ok = want_lines.iter().all(|l| a.lines().any(|x| x == *l));
        let b_ok = want_lines.iter().all(|l| b.lines().any(|x| x == *l));
        if a_ok && b_ok && a == b {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "divergent sessions did not merge/converge to the union");
}

#[tokio::test]
async fn divergent_writes_are_detected_as_conflict() {
    let sim = Sim::new("conflict");
    let rel = "--proj--/2026-01-01T00-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";

    // node A publishes its version of the session
    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut peer_a = sim.pi_peer("a", "pi", node_a);
    peer_a.write(rel, b"version from A\n");
    peer_a.tick().await;

    // node B joins, then publishes its OWN divergent version of the same session
    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let mut peer_b = sim.pi_peer("b", "pi", node_b);
    peer_b.write(rel, b"different version from B\n");
    peer_b.tick().await;

    // once A's entry and blob sync in, B sees two authors whose contents do
    // not collapse into the winner: a conflict in the status report.
    let mut ok = false;
    for _ in 0..60 {
        if !peer_b
            .engine
            .status_report()
            .await
            .unwrap()
            .conflicts
            .is_empty()
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "divergent writes were not detected as a conflict");
}

#[tokio::test]
async fn pi_and_omp_sessions_sync_side_by_side() {
    let sim = Sim::new("multiagent");

    // node A: one pi session and one omp session in their own roots
    let pi_root_a = sim.base.join("a/pi-sessions");
    let omp_root_a = sim.base.join("a/omp-sessions");
    let pi_rel = "--home-simon-Projects-demo--/2026-07-02T08-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
    let omp_rel =
        "-Projects-demo/2026-07-02T08-01-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl";
    let pi_contents = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"from pi\"}\n";
    let omp_contents = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"from omp\"}\n";
    write_file(&pi_root_a.join(pi_rel), pi_contents);
    write_file(&omp_root_a.join(omp_rel), omp_contents);

    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut peer_a = sim.peer(
        "a",
        vec![
            Box::new(PiAdapter::new("pi", &pi_root_a)) as Box<dyn Adapter>,
            Box::new(PiAdapter::new("omp", &omp_root_a)),
        ],
        sim.identity(),
        node_a,
    );
    peer_a.tick().await;

    // node B: both agents configured, empty roots
    let pi_root_b = sim.base.join("b/pi-sessions");
    let omp_root_b = sim.base.join("b/omp-sessions");
    std::fs::create_dir_all(&pi_root_b).unwrap();
    std::fs::create_dir_all(&omp_root_b).unwrap();
    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let mut peer_b = sim.peer(
        "b",
        vec![
            Box::new(PiAdapter::new("pi", &pi_root_b)) as Box<dyn Adapter>,
            Box::new(PiAdapter::new("omp", &omp_root_b)),
        ],
        sim.identity(),
        node_b,
    );

    // both sessions must land on B, each under its own agent's root
    let pi_dest = pi_root_b.join(pi_rel);
    let omp_dest = omp_root_b.join(omp_rel);
    let ok = converge(&mut [&mut peer_b], || {
        file_eq(&pi_dest, pi_contents) && file_eq(&omp_dest, omp_contents)
    })
    .await;
    assert!(ok, "pi+omp sessions did not both sync to node B");
}

#[tokio::test]
async fn omp_blob_store_syncs_binary_blobs() {
    let sim = Sim::new("blobstore");

    // node A: one blob as omp writes it — bare hash plus a `.png` alias,
    // identical binary (non-UTF8) content.
    let blob_root_a = sim.base.join("a/blobs");
    std::fs::create_dir_all(&blob_root_a).unwrap();
    let hash = "a2a7f46769739a24d0d13eb5544a6041f830ac69395805c2da51d8de11b62711";
    let contents: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR\xff\xfe";
    std::fs::write(blob_root_a.join(hash), contents).unwrap();
    std::fs::write(blob_root_a.join(format!("{hash}.png")), contents).unwrap();

    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut peer_a = sim.peer(
        "a",
        vec![Box::new(BlobStoreAdapter::new("omp-blobs", &blob_root_a)) as Box<dyn Adapter>],
        sim.identity(),
        node_a,
    );
    peer_a.tick().await;

    // node B: empty blob store, joins A's namespace
    let blob_root_b = sim.base.join("b/blobs");
    std::fs::create_dir_all(&blob_root_b).unwrap();
    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let mut peer_b = sim.peer(
        "b",
        vec![Box::new(BlobStoreAdapter::new("omp-blobs", &blob_root_b)) as Box<dyn Adapter>],
        sim.identity(),
        node_b,
    );

    // both blob files must land on B, byte-identical
    let dest1 = blob_root_b.join(hash);
    let dest2 = blob_root_b.join(format!("{hash}.png"));
    let ok = converge(&mut [&mut peer_b], || {
        file_eq(&dest1, contents) && file_eq(&dest2, contents)
    })
    .await;
    assert!(ok, "blob files did not sync to node B within timeout");
}

#[tokio::test]
async fn missed_content_download_is_fetched_on_write() {
    let sim = Sim::new("fetch");
    let ns_secret = ssync_net::generate_key_bytes();

    let rel = "--proj--/2026-01-01T00-00-00-000Z_019efeeed0001eee71acbe20fetch0001.jsonl";
    let contents = b"content the live engine failed to download\n";

    let mut node_a = sim.node("a").await;
    let mut node_b = sim.node("b").await;
    node_a.open_shared_namespace(ns_secret).await.unwrap();
    node_b.open_shared_namespace(ns_secret).await.unwrap();
    // B syncs index entries but never auto-downloads content — the prod
    // failure mode (a missed live download is never retried by iroh-docs).
    node_b.disable_auto_download().await.unwrap();
    let addr_a = node_a.endpoint_addr();
    let addr_b = node_b.endpoint_addr();
    node_a.sync_with(vec![addr_b]).await.unwrap();
    node_b.sync_with(vec![addr_a]).await.unwrap();

    let mut peer_a = sim.pi_peer("a", "pi", node_a);
    peer_a.write(rel, contents);
    peer_a.tick().await;
    let mut peer_b = sim.pi_peer("b", "pi", node_b);

    // the file can only materialize via the explicit peer fetch
    let dest = peer_b.path(rel);
    let ok = converge(&mut [&mut peer_b], || file_eq(&dest, contents)).await;
    assert!(ok, "missed content was never fetched from the peer");
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_recovers_when_peer_comes_up_late() {
    let sim = Sim::new("resync");
    let ns_secret = ssync_net::generate_key_bytes();

    let rel = "--proj--/2026-01-01T00-00-00-000Z_019eresync001eee71acbe20resync001.jsonl";
    let contents = b"written while the peer was unavailable\n";

    let mut node_a = sim.node("a").await;
    let mut node_b = sim.node("b").await;
    node_a.open_shared_namespace(ns_secret).await.unwrap();
    let addr_b = node_b.endpoint_addr();
    // B's namespace is not open yet: A's startup sync goes nowhere, like
    // dialing a peer that is down. B never learns about A on its own.
    node_a.sync_with(vec![addr_b]).await.unwrap();

    let mut peer_a = sim.pi_peer("a", "pi", node_a);
    peer_a.write(rel, contents);
    peer_a.engine.set_resync_interval(Duration::from_secs(2));
    peer_a.run();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // B comes up late, like a restarted peer: it opens the namespace and
    // starts syncing, but its own dial goes nowhere (bogus peer id — in prod
    // the dial-back can be lost the same way). Only A's periodic re-sync can
    // establish the link.
    node_b.open_shared_namespace(ns_secret).await.unwrap();
    let bogus = ssync_net::iroh::SecretKey::generate().public();
    node_b
        .sync_with(vec![ssync_net::iroh::EndpointAddr::from(bogus)])
        .await
        .unwrap();
    let peer_b = sim.pi_peer("b", "pi", node_b);
    let root_b = peer_b.run();

    let dest = root_b.join(rel);
    let ok = eventually(|| file_eq(&dest, contents)).await;
    assert!(
        ok,
        "session never reached the late peer (no periodic re-sync)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn ticket_issuer_learns_peers_and_recovers_missed_content() {
    // The ticket issuer starts with an empty peer list (`join` only records
    // peers on the joining side), so a missed content download on the issuer
    // is unrecoverable unless it learns the joiner from live sync events.
    let sim = Sim::new("learn-peer");

    let rel = "--proj--/2026-01-01T00-00-00-000Z_019efeeed0001eee71acbe20learn0001.jsonl";
    let contents = b"content the issuer failed to auto-download\n";

    let mut node_a = sim.node("a").await;
    let mut node_b = sim.node("b").await;
    node_a.create_namespace().await.unwrap();
    // The issuer misses every live content download — the prod failure mode
    // (iroh-docs never retries), made deterministic.
    node_a.disable_auto_download().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    node_b.join(ticket).await.unwrap();

    let peer_b = sim.pi_peer("b", "pi", node_b);
    peer_b.write(rel, contents);
    peer_b.run();

    // Deterministic ordering: wait until the joiner's entry reached the
    // issuer's index *before* the issuer's engine (and thus its event
    // subscription) exists — every live peer event has then already fired
    // unheard, so only the persisted-peer seed can recover the download.
    let mut synced = false;
    for _ in 0..60 {
        if !node_a.index_records().await.unwrap().is_empty() {
            synced = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(synced, "joiner's entry never reached the issuer's index");

    let peer_a = sim.pi_peer("a", "pi", node_a);
    let root_a = peer_a.run();

    let dest = root_a.join(rel);
    let ok = eventually(|| file_eq(&dest, contents)).await;
    assert!(ok, "issuer never learned the joiner as a fetch peer");
}

/// Issue #14: an excluded project is frozen from both sides — never
/// published from the machine that excludes it, and never materialized
/// there when a peer publishes it. Non-excluded traffic flows normally.
#[tokio::test]
async fn excluded_projects_neither_publish_nor_materialize() {
    let sim = Sim::new("exclude");

    let rel_normal =
        "--home-x-demo--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
    let rel_secret =
        "--home-x-client-x--/2026-05-23T07-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl";
    let contents = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"hi\"}\n".to_vec();

    // --- node A: excludes *client-x*, has one normal and one excluded session ---
    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut peer_a = sim.pi_peer("a", "pi", node_a);
    for rel in [rel_normal, rel_secret] {
        peer_a.write(rel, &contents);
    }
    peer_a
        .engine
        .set_excludes([("pi".to_string(), vec!["*client-x*".to_string()])].into());
    peer_a.tick().await;

    // --- node B: no excludes ---
    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let mut peer_b = sim.pi_peer("b", "pi", node_b);

    // the normal session reaches B ...
    let dest_normal = peer_b.path(rel_normal);
    let ok = converge(&mut [&mut peer_b], || file_eq(&dest_normal, &contents)).await;
    assert!(ok, "non-excluded session did not sync to B");
    // ... and the excluded one was never published (sync is live, so its
    // absence on B proves A withheld it).
    assert!(
        !peer_b.path(rel_secret).exists(),
        "excluded session leaked to B"
    );

    // --- reverse: B creates a session in a project A excludes ---
    let rel_secret_b =
        "--home-x-client-x--/2026-05-23T08-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4c.jsonl";
    let rel_normal_b =
        "--home-x-other--/2026-05-23T08-05-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4d.jsonl";
    for rel in [rel_secret_b, rel_normal_b] {
        peer_b.write(rel, &contents);
    }
    peer_b.tick().await;

    // when B's normal session lands on A, sync has round-tripped; the
    // excluded key must not have materialized on A.
    let dest_normal_b = peer_a.path(rel_normal_b);
    let ok = converge(&mut [&mut peer_a], || file_eq(&dest_normal_b, &contents)).await;
    assert!(ok, "B's non-excluded session did not sync to A");
    assert!(
        !peer_a.path(rel_secret_b).exists(),
        "A materialized a session from an excluded project"
    );
    // B keeps its own excluded file untouched (freeze, not delete)
    assert!(peer_b.path(rel_secret_b).exists());
}

/// Removing an agent's `[[agents]]` entry must FREEZE its sessions, not
/// tombstone them: the dropped agent's index keys look like "materialised
/// here, now gone" (state still holds the import stamp), which without a
/// guard propagates deletion to every peer.
#[tokio::test]
async fn removing_an_agent_freezes_it_instead_of_tombstoning() {
    let sim = Sim::new("agent-drop");
    let two_adapters = |root_pi: &Path, root_omp: &Path| -> Vec<Box<dyn Adapter>> {
        vec![
            Box::new(PiAdapter::new("pi", root_pi)),
            Box::new(PiAdapter::new("omp", root_omp)),
        ]
    };

    let rel = "--home-x-demo--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
    let contents = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"omp\"}\n".to_vec();

    // --- node A: pi + omp configured, one omp session, persisted state ---
    let (root_pi_a, root_omp_a) = (sim.base.join("a/pi"), sim.base.join("a/omp"));
    let src = root_omp_a.join(rel);
    write_file(&src, &contents);

    let key_a = SecretKey::generate();
    let mut node_a = sim.node_with_key("a", key_a.clone()).await;
    let ns = node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut peer_a = sim.peer(
        "a",
        two_adapters(&root_pi_a, &root_omp_a),
        sim.identity(),
        node_a,
    );
    peer_a.persist();
    peer_a.tick().await;

    // --- node B: same two agents, receives the omp session ---
    let (root_pi_b, root_omp_b) = (sim.base.join("b/pi"), sim.base.join("b/omp"));
    std::fs::create_dir_all(&root_pi_b).unwrap();
    std::fs::create_dir_all(&root_omp_b).unwrap();
    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let b_addr = node_b.endpoint_addr();
    let mut peer_b = sim.peer(
        "b",
        two_adapters(&root_pi_b, &root_omp_b),
        sim.identity(),
        node_b,
    );
    let dest = root_omp_b.join(rel);
    let ok = converge(&mut [&mut peer_b], || file_eq(&dest, &contents)).await;
    assert!(ok, "omp session did not sync to B");

    // --- restart A with omp dropped from the config ---
    peer_a.shutdown().await.unwrap();
    let mut node_a2 = sim.node_with_key("a", key_a).await;
    node_a2.open_namespace(ns).await.unwrap();
    node_a2.sync_with(vec![b_addr]).await.unwrap();
    let mut peer_a2 = sim.peer(
        "a",
        vec![Box::new(PiAdapter::new("pi", &root_pi_a)) as Box<dyn Adapter>],
        sim.identity(),
        node_a2,
    );
    peer_a2.persist();
    peer_a2.tick().await;

    // sentinel: a new pi session on A round-trips to B, proving sync is live
    let rel_pi =
        "--home-x-demo--/2026-05-23T08-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl";
    write_file(&root_pi_a.join(rel_pi), &contents);
    let dest_pi_b = root_pi_b.join(rel_pi);
    let ok = converge(&mut [&mut peer_a2, &mut peer_b], || {
        file_eq(&dest_pi_b, &contents)
    })
    .await;
    assert!(ok, "pi sentinel did not sync after restart");

    // the dropped agent's session survives on B (frozen, not tombstoned) ...
    assert!(
        file_eq(&dest, &contents),
        "B lost the omp session after A dropped the agent"
    );
    // ... and A's own copy on disk is untouched
    assert!(file_eq(&src, &contents));
}

/// Issue #13 (map #42): a machine with a `[[path_map]]` converges with an
/// unmapped machine hosting the project at the canonical path — including the
/// hard case, omp's home-relative wire keys derived via `canonical_home`.
#[tokio::test]
async fn path_mapped_machines_converge() {
    let sim = Sim::new("pathmap");
    let canonical_home = "/canon-home";
    let canonical_cwd = "/canon-home/Projects/x";

    // --- node A: unmapped, hosts the project at the canonical path. Its
    // on-disk form IS the wire form: home-relative dir + canonical header.
    let rel_a = "-Projects-x/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
    let canonical_bytes = format!(
        "{{\"type\":\"title\",\"v\":1,\"title\":\"t\"}}\n{{\"type\":\"session\",\"version\":3,\"id\":\"019e539d-f6ab-71ac-be20-d3ae2b23ea4a\",\"cwd\":\"{canonical_cwd}\"}}\n{{\"msg\":\"from A\"}}\n"
    );

    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut peer_a = sim.pi_peer("a", "omp", node_a);
    peer_a.write(rel_a, &canonical_bytes);
    peer_a.tick().await;
    let src_a = peer_a.path(rel_a);

    // --- node B: hosts the project at a different absolute path, bridged by
    // the map. Its local dir uses B's real-home encoding (outside → legacy).
    let local_prefix = sim.base.join("b/work");
    let local_cwd = local_prefix.join("x");
    std::fs::create_dir_all(&local_cwd).unwrap();
    let map = ssync_core::PathMap::new(vec![(
        local_prefix.to_string_lossy().into_owned(),
        "/canon-home/Projects".to_string(),
    )])
    .unwrap();

    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let mut peer_b = sim.pi_peer("b", "omp", node_b);
    peer_b.engine.set_path_map(map, Some(canonical_home.into()));
    let root_b = peer_b.root.clone();

    // A's session materializes on B under B's LOCAL encoding, header rewritten
    // intentional mirror of PiAdapter's legacy `--abs--` encoding (pi.rs
    // encode_cwd): an independent expectation, not a call into the code
    // under test — drift here means the encoding changed
    let local_component = format!(
        "--{}--",
        local_cwd
            .to_string_lossy()
            .trim_start_matches('/')
            .replace(['/', ':'], "-")
    );
    let dest_b = root_b
        .join(&local_component)
        .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl");
    let mut ok = false;
    for _ in 0..60 {
        peer_b.tick().await;
        if let Ok(got) = std::fs::read_to_string(&dest_b) {
            assert!(
                got.contains(&format!("\"cwd\":\"{}\"", local_cwd.display())),
                "header must carry B's local path: {got}"
            );
            assert!(got.ends_with("{\"msg\":\"from A\"}\n"), "body untouched");
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "mapped session did not materialize on B");

    // B appends locally; A must receive the line with its canonical header
    // intact — proving B republished CANONICAL bytes under the SAME key.
    let mut appended = std::fs::read_to_string(&dest_b).unwrap();
    appended.push_str("{\"msg\":\"from B\"}\n");
    std::fs::write(&dest_b, &appended).unwrap();
    peer_b.tick().await;

    let expect_a = format!("{canonical_bytes}{{\"msg\":\"from B\"}}\n");
    let mut ok = false;
    for _ in 0..60 {
        peer_a.tick().await;
        peer_b.tick().await;
        if std::fs::read_to_string(&src_a).is_ok_and(|g| g == expect_a) {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "B's append did not converge canonically on A");

    // wire hygiene: every index key uses the canonical (home-relative) form;
    // B's machine-local path never leaks into the key space.
    for rec in peer_a
        .engine
        .index_keys()
        .await
        .expect("index keys readable")
    {
        assert!(
            rec.starts_with("omp/-Projects-x/"),
            "non-canonical key on the wire: {rec}"
        );
    }

    // --- artifact ride-along: a header-less file in the session's artifact
    // dir (omp: subagent transcripts, __advisor.jsonl) must land under B's
    // LOCAL project dir via the learned dir translation, bytes untouched.
    let art_rel =
        "-Projects-x/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a/__advisor.jsonl";
    let art_bytes = b"{\"type\":\"advisor\",\"note\":\"no session header here\"}\n";
    peer_a.write(art_rel, art_bytes);
    peer_a.tick().await;

    let art_b = root_b
        .join(&local_component)
        .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a/__advisor.jsonl");
    let mut ok = false;
    for _ in 0..60 {
        peer_a.tick().await;
        peer_b.tick().await;
        if std::fs::read(&art_b).is_ok_and(|g| g == art_bytes) {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "artifact did not ride along into B's local project dir");
}

/// A path map must not disturb adapters without cwd semantics: omp-blobs
/// (single-component keys, no header) keeps syncing both ways while a map
/// is active for another agent.
#[tokio::test]
async fn path_map_leaves_cwdless_adapters_alone() {
    let sim = Sim::new("pathmap-blobs");
    let adapters = |root_pi: &Path, root_blobs: &Path| -> Vec<Box<dyn Adapter>> {
        vec![
            Box::new(PiAdapter::new("pi", root_pi)),
            Box::new(BlobStoreAdapter::new("omp-blobs", root_blobs)),
        ]
    };

    // --- node A: unmapped, one blob ---
    let (root_pi_a, root_blobs_a) = (sim.base.join("a/pi"), sim.base.join("a/blobs"));
    std::fs::create_dir_all(&root_blobs_a).unwrap();
    std::fs::create_dir_all(&root_pi_a).unwrap();
    let blob = b"opaque-blob-bytes".to_vec();
    let hash_a = "a".repeat(64);
    std::fs::write(root_blobs_a.join(&hash_a), &blob).unwrap();

    let mut node_a = sim.node("a").await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut peer_a = sim.peer(
        "a",
        adapters(&root_pi_a, &root_blobs_a),
        sim.identity(),
        node_a,
    );
    peer_a.tick().await;

    // --- node B: pi is path-mapped; blobs must be untouched by that ---
    let (root_pi_b, root_blobs_b) = (sim.base.join("b/pi"), sim.base.join("b/blobs"));
    std::fs::create_dir_all(&root_pi_b).unwrap();
    std::fs::create_dir_all(&root_blobs_b).unwrap();
    let mut node_b = sim.node("b").await;
    node_b.join(ticket).await.unwrap();
    let mut peer_b = sim.peer(
        "b",
        adapters(&root_pi_b, &root_blobs_b),
        sim.identity(),
        node_b,
    );
    peer_b.engine.set_path_map(
        ssync_core::PathMap::new(vec![("/srv/elsewhere".into(), "/canon/x".into())]).unwrap(),
        None,
    );

    // inbound: A's blob materializes on B despite the active map
    let dest = root_blobs_b.join(&hash_a);
    let ok = converge(&mut [&mut peer_b], || file_eq(&dest, &blob)).await;
    assert!(ok, "blob did not materialize on the mapped machine");

    // outbound: B's new blob reaches A
    let blob2 = b"blob-from-b".to_vec();
    let hash_b = "b".repeat(64);
    std::fs::write(root_blobs_b.join(&hash_b), &blob2).unwrap();
    peer_b.tick().await;
    let dest_a = root_blobs_a.join(&hash_b);
    let ok = converge(&mut [&mut peer_a, &mut peer_b], || file_eq(&dest_a, &blob2)).await;
    assert!(ok, "mapped machine did not publish its blob");
}

/// An unresolvable mapping must FREEZE, never tombstone (#49: skips are
/// per-key errors, and a skip that reads as deletion propagates mesh-wide).
/// Here: sessions synced without a map; the machine restarts with a map
/// that swallows the project but cannot resolve it (omp without
/// canonical_home) — peers must keep their copies.
#[tokio::test]
async fn unresolvable_mapping_freezes_instead_of_tombstoning() {
    let sim = Sim::new("pathmap-freeze");

    let rel = "-Projects-x/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
    // header cwd points INSIDE the prefix B will later map
    let bytes = b"{\"type\":\"session\",\"version\":3,\"id\":\"019e539d-f6ab-71ac-be20-d3ae2b23ea4a\",\"cwd\":\"/data/Projects/x\"}\n{\"m\":\"1\"}\n".to_vec();

    // --- node B first: it owns the session and will get the bad map ---
    let key_b = SecretKey::generate();
    let mut node_b = sim.node_with_key("b", key_b.clone()).await;
    let ns = node_b.create_namespace().await.unwrap();
    let ticket = node_b.share().await.unwrap();
    let mut peer_b = sim.pi_peer("b", "omp", node_b);
    peer_b.write(rel, &bytes);
    let src_b = peer_b.path(rel);
    peer_b.persist();
    peer_b.tick().await;

    // --- node A receives it ---
    let mut node_a = sim.node("a").await;
    node_a.join(ticket).await.unwrap();
    let a_addr = node_a.endpoint_addr();
    let mut peer_a = sim.pi_peer("a", "omp", node_a);
    let dest_a = peer_a.path(rel);
    let ok = converge(&mut [&mut peer_a], || file_eq(&dest_a, &bytes)).await;
    assert!(ok, "session did not sync to A");

    // --- restart B with a map that swallows the project's cwd but cannot
    // resolve it: omp home-relative canonical without canonical_home ---
    peer_b.shutdown().await.unwrap();
    let mut node_b2 = sim.node_with_key("b", key_b).await;
    node_b2.open_namespace(ns).await.unwrap();
    node_b2.sync_with(vec![a_addr]).await.unwrap();
    let mut peer_b2 = sim.pi_peer("b", "omp", node_b2);
    peer_b2.engine.set_path_map(
        ssync_core::PathMap::new(vec![("/data".into(), "/other-canon".into())]).unwrap(),
        None, // omp cannot encode home-relative canonicals without this
    );
    peer_b2.persist();

    // several passes; then prove sync is otherwise live via A-side traffic
    for _ in 0..6 {
        peer_b2.tick().await;
        peer_a.tick().await;
    }
    let rel2 = "-Projects-y/2026-05-23T08-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl";
    // sentinel cwd OUTSIDE B's mapped prefix: pass-through on both sides
    let sentinel = b"{\"type\":\"session\",\"version\":3,\"id\":\"019e539d-f6ab-71ac-be20-d3ae2b23ea4b\",\"cwd\":\"/elsewhere/y\"}\n".to_vec();
    peer_a.write(rel2, &sentinel);
    let dest_b_rel2 = peer_b2.path(rel2);
    let ok = converge(&mut [&mut peer_a, &mut peer_b2], || dest_b_rel2.exists()).await;
    assert!(ok, "sentinel did not sync after restart");

    // the frozen session survives everywhere
    assert!(
        file_eq(&dest_a, &bytes),
        "A lost the session to a tombstone from B's unresolvable map"
    );
    assert!(file_eq(&src_b, &bytes));
}
