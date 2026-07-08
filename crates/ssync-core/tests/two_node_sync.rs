//! M2 gate: a session created on node A appears, decrypted and byte-identical, in
//! node B's session directory after B joins A's index namespace — no manual copy.
//!
//! Two in-process iroh nodes, one shared age identity (as on a single user's own
//! machines). All mutation goes through `tick_once`/`run` — the production path.
//! Uses poll loops because sync is asynchronous over the network.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ssync_adapters::Adapter;
use ssync_adapters::pi::PiAdapter;
use ssync_core::Engine;
use ssync_crypto::AgeIdentity;
use ssync_net::Node;
use ssync_net::iroh::SecretKey;

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ssync-two-node-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

async fn spawn_node(data_dir: &Path) -> Node {
    Node::spawn(data_dir, SecretKey::generate()).await.unwrap()
}

fn pi_engine(root: &Path, age_secret: &str, node: Node) -> Engine {
    Engine::new(
        PiAdapter::new("pi", root),
        AgeIdentity::from_secret_string(age_secret).unwrap(),
        node,
    )
}

/// Poll `cond` every 500ms until it holds or ~30s elapse.
async fn eventually(mut cond: impl FnMut() -> bool) -> bool {
    for _ in 0..60 {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

#[tokio::test]
async fn session_created_on_a_appears_on_b() {
    let base = scratch("base");
    let secret = AgeIdentity::generate().unwrap().to_secret_string();

    // --- node A: has a real session file ---
    let root_a = base.join("a/sessions");
    let rel = "--home-simon-Projects-demo--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
    let src = root_a.join(rel);
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    let contents = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"hello from A\"}\n";
    std::fs::write(&src, contents).unwrap();

    let mut node_a = spawn_node(&base.join("a/data")).await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut engine_a = pi_engine(&root_a, &secret, node_a);
    engine_a.tick_once().await;

    // --- node B: empty session dir, joins A's namespace ---
    let root_b = base.join("b/sessions");
    std::fs::create_dir_all(&root_b).unwrap();
    let mut node_b = spawn_node(&base.join("b/data")).await;
    node_b.join(ticket).await.unwrap();
    let mut engine_b = pi_engine(&root_b, &secret, node_b);

    // sync is async: tick B until the file materializes, byte-identical.
    let dest = root_b.join(rel);
    let mut ok = false;
    for _ in 0..60 {
        engine_b.tick_once().await;
        if let Ok(got) = std::fs::read(&dest)
            && got == contents
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "session did not sync to node B within timeout");
}

#[tokio::test]
async fn per_machine_identities_sync_both_directions() {
    let base = scratch("permachine");
    let id_a = AgeIdentity::generate().unwrap();
    let id_b = AgeIdentity::generate().unwrap();

    // --- node A: own key, B listed as recipient, has a session ---
    let root_a = base.join("a/sessions");
    let rel_a = "--home-simon-Projects-demo--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl";
    let src = root_a.join(rel_a);
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    let contents_a = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"from A\"}\n";
    std::fs::write(&src, contents_a).unwrap();

    let mut node_a = spawn_node(&base.join("a/data")).await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut ident_a = AgeIdentity::from_secret_string(&id_a.to_secret_string()).unwrap();
    ident_a.add_recipients([id_b.recipient_string()]);
    let mut engine_a = Engine::new(PiAdapter::new("pi", &root_a), ident_a, node_a);
    engine_a.tick_once().await;

    // --- node B: own key, A listed as recipient ---
    let root_b = base.join("b/sessions");
    std::fs::create_dir_all(&root_b).unwrap();
    let mut node_b = spawn_node(&base.join("b/data")).await;
    node_b.join(ticket).await.unwrap();
    let mut ident_b = AgeIdentity::from_secret_string(&id_b.to_secret_string()).unwrap();
    ident_b.add_recipients([id_a.recipient_string()]);
    let mut engine_b = Engine::new(PiAdapter::new("pi", &root_b), ident_b, node_b);

    // A → B: B decrypts A's blob with its own key.
    let dest_a = root_b.join(rel_a);
    let mut ok = false;
    for _ in 0..60 {
        engine_b.tick_once().await;
        if let Ok(got) = std::fs::read(&dest_a)
            && got == contents_a
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "A's session did not reach B under per-machine keys");

    // B → A: symmetric direction.
    let rel_b = "--home-simon-Projects-demo--/2026-05-23T07-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4c.jsonl";
    let contents_b = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"from B\"}\n";
    std::fs::write(root_b.join(rel_b), contents_b).unwrap();
    let dest_b = root_a.join(rel_b);
    let mut ok = false;
    for _ in 0..60 {
        engine_b.tick_once().await;
        engine_a.tick_once().await;
        if let Ok(got) = std::fs::read(&dest_b)
            && got == contents_b
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "B's session did not reach A under per-machine keys");
}

#[tokio::test]
async fn live_write_propagates_without_restart() {
    let base = scratch("live");
    let secret = AgeIdentity::generate().unwrap().to_secret_string();
    let root_a = base.join("a/sessions");
    let root_b = base.join("b/sessions");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    let mut node_a = spawn_node(&base.join("a/data")).await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut engine_a = pi_engine(&root_a, &secret, node_a);

    let mut node_b = spawn_node(&base.join("b/data")).await;
    node_b.join(ticket).await.unwrap();
    let mut engine_b = pi_engine(&root_b, &secret, node_b);

    // start both daemons and let them enter their loops
    let sa = base.join("a/status.toml");
    let sb = base.join("b/status.toml");
    tokio::spawn(async move { engine_a.run(&sa).await });
    tokio::spawn(async move { engine_b.run(&sb).await });
    tokio::time::sleep(Duration::from_secs(2)).await;

    // write a NEW session on A *after* startup (also a brand-new project dir)
    let rel = "--proj--/2026-07-01T00-00-00-000Z_019e9999-eeee-71ac-be20-livewrite00001.jsonl";
    let src = root_a.join(rel);
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    let contents = b"live write after start\n";
    std::fs::write(&src, contents).unwrap();

    let dest = root_b.join(rel);
    let ok = eventually(|| std::fs::read(&dest).is_ok_and(|got| got == contents)).await;
    assert!(ok, "live write after startup did not propagate to node B");
}

#[tokio::test]
async fn shared_namespace_auto_connects_without_ticket() {
    let base = scratch("shared");
    let ns_secret = ssync_net::generate_key_bytes();
    let age = AgeIdentity::generate().unwrap().to_secret_string();

    let root_a = base.join("a/sessions");
    std::fs::create_dir_all(&root_a).unwrap();
    let root_b = base.join("b/sessions");
    std::fs::create_dir_all(&root_b).unwrap();

    let mut node_a = spawn_node(&base.join("a/data")).await;
    let mut node_b = spawn_node(&base.join("b/data")).await;

    // the same secret must yield the same namespace on both — no ticket exchange
    let id_a = node_a.open_shared_namespace(ns_secret).await.unwrap();
    let id_b = node_b.open_shared_namespace(ns_secret).await.unwrap();
    assert_eq!(id_a, id_b, "shared secret must yield the same namespace");

    // connect the two peers (addresses here; in production resolved from node-ids)
    let addr_a = node_a.endpoint_addr();
    let addr_b = node_b.endpoint_addr();
    node_a.sync_with(vec![addr_b]).await.unwrap();
    node_b.sync_with(vec![addr_a]).await.unwrap();

    let mut engine_a = pi_engine(&root_a, &age, node_a);
    let mut engine_b = pi_engine(&root_b, &age, node_b);

    let sa = base.join("a/status.toml");
    let sb = base.join("b/status.toml");
    tokio::spawn(async move { engine_a.run(&sa).await });
    tokio::spawn(async move { engine_b.run(&sb).await });
    tokio::time::sleep(Duration::from_secs(2)).await;

    let rel = "--proj--/2026-01-01T00-00-00-000Z_019eshared0001eeee71acbe20shared01.jsonl";
    let src = root_a.join(rel);
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    let contents = b"shared namespace session\n";
    std::fs::write(&src, contents).unwrap();

    let dest = root_b.join(rel);
    let mut ok = false;
    for _ in 0..80 {
        if let Ok(got) = std::fs::read(&dest)
            && got == contents
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "session did not sync over the shared namespace");
}

#[tokio::test]
async fn deletion_propagates_and_does_not_resurrect() {
    let base = scratch("del");
    let secret = AgeIdentity::generate().unwrap().to_secret_string();
    let rel = "--proj--/2026-01-01T00-00-00-000Z_019edddd0001eeee71acbe20delete001.jsonl";

    let root_a = base.join("a/sessions");
    std::fs::create_dir_all(root_a.join("--proj--")).unwrap();
    // keep a second unrelated session so the dir is never empty (deletion guard)
    std::fs::write(
        root_a.join("--proj--/keep_019e0000keepkeepkeep71acbe20keep00001.jsonl"),
        b"keep\n",
    )
    .unwrap();
    let root_b = base.join("b/sessions");
    std::fs::create_dir_all(&root_b).unwrap();

    let mut node_a = spawn_node(&base.join("a/data")).await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut engine_a = pi_engine(&root_a, &secret, node_a);

    let mut node_b = spawn_node(&base.join("b/data")).await;
    node_b.join(ticket).await.unwrap();
    let mut engine_b = pi_engine(&root_b, &secret, node_b);

    std::fs::write(root_a.join(rel), b"header\nto-be-deleted\n").unwrap();

    let sa = base.join("a/status.toml");
    let sb = base.join("b/status.toml");
    tokio::spawn(async move { engine_a.run(&sa).await });
    tokio::spawn(async move { engine_b.run(&sb).await });

    // it appears on B
    let dest = root_b.join(rel);
    assert!(
        eventually(|| dest.exists()).await,
        "session never reached B"
    );

    // delete on A -> must disappear on B and stay gone
    std::fs::remove_file(root_a.join(rel)).unwrap();
    assert!(
        eventually(|| !dest.exists()).await,
        "deletion did not propagate to B"
    );

    // confirm it does not resurrect
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!dest.exists(), "deleted session resurrected on B");
}

#[tokio::test]
async fn deletion_by_non_author_propagates_back() {
    // a session created on A, deleted on B, must disappear on A (and stay gone)
    // even though A authored the index entry (TODO "deletion by any participant").
    let base = scratch("xdel");
    let secret = AgeIdentity::generate().unwrap().to_secret_string();
    let rel = "--proj--/2026-01-01T00-00-00-000Z_019exdel0001eeee71acbe20xdele001.jsonl";

    let root_a = base.join("a/sessions");
    std::fs::create_dir_all(root_a.join("--proj--")).unwrap();
    // second session so neither dir ever goes empty (deletion guard)
    std::fs::write(
        root_a.join("--proj--/keep_019e0000keepkeepkeep71acbe20keep00001.jsonl"),
        b"keep\n",
    )
    .unwrap();
    std::fs::write(root_a.join(rel), b"header\nauthored on A\n").unwrap();
    let root_b = base.join("b/sessions");
    std::fs::create_dir_all(&root_b).unwrap();

    let mut node_a = spawn_node(&base.join("a/data")).await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut engine_a = pi_engine(&root_a, &secret, node_a);

    let mut node_b = spawn_node(&base.join("b/data")).await;
    node_b.join(ticket).await.unwrap();
    let mut engine_b = pi_engine(&root_b, &secret, node_b);

    let sa = base.join("a/status.toml");
    let sb = base.join("b/status.toml");
    tokio::spawn(async move { engine_a.run(&sa).await });
    tokio::spawn(async move { engine_b.run(&sb).await });

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
    let base = scratch("down-del");
    let secret = AgeIdentity::generate().unwrap().to_secret_string();
    let root = base.join("sessions");
    let state_path = base.join("state.toml");
    let rel = "--proj--/2026-01-01T00-00-00-000Z_019edown0001eeee71acbe20downdel01.jsonl";
    let keep = "--proj--/keep_019e0000keepkeepkeep71acbe20keep00001.jsonl";
    std::fs::create_dir_all(root.join("--proj--")).unwrap();
    std::fs::write(root.join(rel), b"header\ndelete me\n").unwrap();
    std::fs::write(root.join(keep), b"keep\n").unwrap();

    let ns = {
        let mut node = spawn_node(&base.join("data")).await;
        let ns = node.create_namespace().await.unwrap();
        let mut engine = pi_engine(&root, &secret, node);
        engine.persist_state(&state_path);
        engine.tick_once().await;
        engine.shutdown().await.unwrap();
        ns
    };

    // daemon down: the session file disappears
    std::fs::remove_file(root.join(rel)).unwrap();

    let mut node = spawn_node(&base.join("data")).await;
    node.open_namespace(ns).await.unwrap();
    let mut engine = pi_engine(&root, &secret, node);
    engine.persist_state(&state_path);
    engine.tick_once().await;

    // the key must now be a tombstone (deleted), not a live re-import
    let report = engine.status_report().await.unwrap();
    assert_eq!(report.sessions, 1, "deleted session was re-imported");
}

#[tokio::test]
async fn divergent_sessions_merge_and_converge() {
    let base = scratch("merge");
    let secret = AgeIdentity::generate().unwrap().to_secret_string();
    let rel = "--proj--/2026-01-01T00-00-00-000Z_019eccccdddd71acbe20merge0000001.jsonl";

    let root_a = base.join("a/sessions");
    std::fs::create_dir_all(root_a.join("--proj--")).unwrap();
    let root_b = base.join("b/sessions");
    std::fs::create_dir_all(root_b.join("--proj--")).unwrap();

    let mut node_a = spawn_node(&base.join("a/data")).await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut engine_a = pi_engine(&root_a, &secret, node_a);

    let mut node_b = spawn_node(&base.join("b/data")).await;
    node_b.join(ticket).await.unwrap();
    let mut engine_b = pi_engine(&root_b, &secret, node_b);

    // each machine has its own divergent version of the same session
    std::fs::write(root_a.join(rel), b"header\ncommon\nonly-on-a\n").unwrap();
    std::fs::write(root_b.join(rel), b"header\ncommon\nonly-on-b\n").unwrap();

    let sa = base.join("a/status.toml");
    let sb = base.join("b/status.toml");
    tokio::spawn(async move { engine_a.run(&sa).await });
    tokio::spawn(async move { engine_b.run(&sb).await });

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
    let base = scratch("conflict");
    let secret = AgeIdentity::generate().unwrap().to_secret_string();
    let rel = "--proj--/2026-01-01T00-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";

    // node A publishes its version of the session
    let root_a = base.join("a/sessions");
    let src_a = root_a.join(rel);
    std::fs::create_dir_all(src_a.parent().unwrap()).unwrap();
    std::fs::write(&src_a, b"version from A\n").unwrap();
    let mut node_a = spawn_node(&base.join("a/data")).await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut engine_a = pi_engine(&root_a, &secret, node_a);
    engine_a.tick_once().await;

    // node B joins, then publishes its OWN divergent version of the same session
    let root_b = base.join("b/sessions");
    let src_b = root_b.join(rel);
    std::fs::create_dir_all(src_b.parent().unwrap()).unwrap();
    std::fs::write(&src_b, b"different version from B\n").unwrap();
    let mut node_b = spawn_node(&base.join("b/data")).await;
    node_b.join(ticket).await.unwrap();
    let mut engine_b = pi_engine(&root_b, &secret, node_b);
    engine_b.tick_once().await;

    // once A's entry and blob sync in, B sees two authors whose contents do
    // not collapse into the winner: a conflict in the status report.
    let mut ok = false;
    for _ in 0..60 {
        if !engine_b.status_report().await.unwrap().conflicts.is_empty() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "divergent writes were not detected as a conflict");
}

#[tokio::test]
async fn pi_and_omp_sessions_sync_side_by_side() {
    let base = scratch("multiagent");
    let secret = AgeIdentity::generate().unwrap().to_secret_string();

    // node A: one pi session and one omp session in their own roots
    let pi_root_a = base.join("a/pi-sessions");
    let omp_root_a = base.join("a/omp-sessions");
    let pi_rel = "--home-simon-Projects-demo--/2026-07-02T08-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
    let omp_rel =
        "-Projects-demo/2026-07-02T08-01-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl";
    let pi_src = pi_root_a.join(pi_rel);
    let omp_src = omp_root_a.join(omp_rel);
    std::fs::create_dir_all(pi_src.parent().unwrap()).unwrap();
    std::fs::create_dir_all(omp_src.parent().unwrap()).unwrap();
    let pi_contents = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"from pi\"}\n";
    let omp_contents = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"from omp\"}\n";
    std::fs::write(&pi_src, pi_contents).unwrap();
    std::fs::write(&omp_src, omp_contents).unwrap();

    let mut node_a = spawn_node(&base.join("a/data")).await;
    node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();
    let mut engine_a = Engine::with_adapters(
        vec![
            Box::new(PiAdapter::new("pi", &pi_root_a)) as Box<dyn Adapter>,
            Box::new(PiAdapter::new("omp", &omp_root_a)),
        ],
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_a,
    );
    engine_a.tick_once().await;

    // node B: both agents configured, empty roots
    let pi_root_b = base.join("b/pi-sessions");
    let omp_root_b = base.join("b/omp-sessions");
    std::fs::create_dir_all(&pi_root_b).unwrap();
    std::fs::create_dir_all(&omp_root_b).unwrap();
    let mut node_b = spawn_node(&base.join("b/data")).await;
    node_b.join(ticket).await.unwrap();
    let mut engine_b = Engine::with_adapters(
        vec![
            Box::new(PiAdapter::new("pi", &pi_root_b)) as Box<dyn Adapter>,
            Box::new(PiAdapter::new("omp", &omp_root_b)),
        ],
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_b,
    );

    // both sessions must land on B, each under its own agent's root
    let pi_dest = pi_root_b.join(pi_rel);
    let omp_dest = omp_root_b.join(omp_rel);
    let mut ok = false;
    for _ in 0..60 {
        engine_b.tick_once().await;
        if std::fs::read(&pi_dest).is_ok_and(|got| got == pi_contents)
            && std::fs::read(&omp_dest).is_ok_and(|got| got == omp_contents)
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "pi+omp sessions did not both sync to node B");
}

#[tokio::test]
async fn missed_content_download_is_fetched_on_write() {
    let base = scratch("fetch");
    let ns_secret = ssync_net::generate_key_bytes();
    let age = AgeIdentity::generate().unwrap().to_secret_string();

    let root_a = base.join("a/sessions");
    let rel = "--proj--/2026-01-01T00-00-00-000Z_019efeeed0001eee71acbe20fetch0001.jsonl";
    let src = root_a.join(rel);
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    let contents = b"content the live engine failed to download\n";
    std::fs::write(&src, contents).unwrap();
    let root_b = base.join("b/sessions");
    std::fs::create_dir_all(&root_b).unwrap();

    let mut node_a = spawn_node(&base.join("a/data")).await;
    let mut node_b = spawn_node(&base.join("b/data")).await;
    node_a.open_shared_namespace(ns_secret).await.unwrap();
    node_b.open_shared_namespace(ns_secret).await.unwrap();
    // B syncs index entries but never auto-downloads content — the prod
    // failure mode (a missed live download is never retried by iroh-docs).
    node_b.disable_auto_download().await.unwrap();
    let addr_a = node_a.endpoint_addr();
    let addr_b = node_b.endpoint_addr();
    node_a.sync_with(vec![addr_b]).await.unwrap();
    node_b.sync_with(vec![addr_a]).await.unwrap();

    let mut engine_a = pi_engine(&root_a, &age, node_a);
    engine_a.tick_once().await;
    let mut engine_b = pi_engine(&root_b, &age, node_b);

    // the file can only materialize via the explicit peer fetch
    let dest = root_b.join(rel);
    let mut ok = false;
    for _ in 0..60 {
        engine_b.tick_once().await;
        if let Ok(got) = std::fs::read(&dest)
            && got == contents
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "missed content was never fetched from the peer");
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_recovers_when_peer_comes_up_late() {
    let base = scratch("resync");
    let ns_secret = ssync_net::generate_key_bytes();
    let age = AgeIdentity::generate().unwrap().to_secret_string();

    let root_a = base.join("a/sessions");
    let rel = "--proj--/2026-01-01T00-00-00-000Z_019eresync001eee71acbe20resync001.jsonl";
    let src = root_a.join(rel);
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    let contents = b"written while the peer was unavailable\n";
    std::fs::write(&src, contents).unwrap();
    let root_b = base.join("b/sessions");
    std::fs::create_dir_all(&root_b).unwrap();

    let mut node_a = spawn_node(&base.join("a/data")).await;
    let mut node_b = spawn_node(&base.join("b/data")).await;
    node_a.open_shared_namespace(ns_secret).await.unwrap();
    let addr_b = node_b.endpoint_addr();
    // B's namespace is not open yet: A's startup sync goes nowhere, like
    // dialing a peer that is down. B never learns about A on its own.
    node_a.sync_with(vec![addr_b]).await.unwrap();

    let mut engine_a = pi_engine(&root_a, &age, node_a);
    engine_a.set_resync_interval(Duration::from_secs(2));
    let sa = base.join("a/status.toml");
    tokio::spawn(async move { engine_a.run(&sa).await });
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
    let mut engine_b = pi_engine(&root_b, &age, node_b);
    let sb = base.join("b/status.toml");
    tokio::spawn(async move { engine_b.run(&sb).await });

    let dest = root_b.join(rel);
    let ok = eventually(|| std::fs::read(&dest).is_ok_and(|got| got == contents)).await;
    assert!(
        ok,
        "session never reached the late peer (no periodic re-sync)"
    );
}
