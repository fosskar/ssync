//! M2 gate: a session created on node A appears, decrypted and byte-identical, in
//! node B's session directory after B joins A's index namespace — no manual copy.
//!
//! Two in-process iroh nodes, one shared age identity (as on a single user's own
//! machines). Uses a poll loop because sync is asynchronous over the network.

use std::path::PathBuf;
use std::time::Duration;

use ssync_adapters::pi::PiAdapter;
use ssync_core::Engine;
use ssync_crypto::AgeIdentity;
use ssync_net::Node;
use ssync_net::iroh::SecretKey;

fn scratch(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "ssync-2node-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

#[tokio::test]
async fn session_created_on_a_appears_on_b() {
    let base = scratch("base");

    // one shared age identity across both "machines"
    let secret = AgeIdentity::generate().unwrap().to_secret_string();

    // --- node A: has a real session file, imports it ---
    let root_a = base.join("a/sessions");
    let rel = "--home-simon-Projects-demo--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
    let src = root_a.join(rel);
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    let contents = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"hello from A\"}\n";
    std::fs::write(&src, contents).unwrap();

    let node_a = Node::spawn(&base.join("a/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut engine_a = Engine::new(
        PiAdapter::new(&root_a),
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_a,
    );
    engine_a.create_namespace().await.unwrap();
    engine_a.import_file(&src).await.unwrap();

    let ticket = engine_a.share().await.unwrap();

    // --- node B: empty session dir, joins A's namespace ---
    let root_b = base.join("b/sessions");
    std::fs::create_dir_all(&root_b).unwrap();
    let node_b = Node::spawn(&base.join("b/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut engine_b = Engine::new(
        PiAdapter::new(&root_b),
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_b,
    );
    engine_b.join(ticket).await.unwrap();

    // sync is async: poll until the file materializes on B, byte-identical.
    let dest = root_b.join(rel);
    let mut ok = false;
    for _ in 0..60 {
        let _ = engine_b.export_all().await;
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
async fn live_write_propagates_without_restart() {
    let base = scratch("live");
    let secret = AgeIdentity::generate().unwrap().to_secret_string();
    let root_a = base.join("a/sessions");
    let root_b = base.join("b/sessions");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();

    let node_a = Node::spawn(&base.join("a/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut engine_a = Engine::new(
        PiAdapter::new(&root_a),
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_a,
    );
    engine_a.create_namespace().await.unwrap();
    let ticket = engine_a.share().await.unwrap();

    let node_b = Node::spawn(&base.join("b/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut engine_b = Engine::new(
        PiAdapter::new(&root_b),
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_b,
    );
    engine_b.join(ticket).await.unwrap();

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
    let mut ok = false;
    for _ in 0..60 {
        if let Ok(got) = std::fs::read(&dest)
            && got == contents
        {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
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

    let mut node_a = Node::spawn(&base.join("a/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut node_b = Node::spawn(&base.join("b/data"), SecretKey::generate())
        .await
        .unwrap();

    // the same secret must yield the same namespace on both — no ticket exchange
    let id_a = node_a.open_shared_namespace(ns_secret).await.unwrap();
    let id_b = node_b.open_shared_namespace(ns_secret).await.unwrap();
    assert_eq!(id_a, id_b, "shared secret must yield the same namespace");

    // connect the two peers (addresses here; in production resolved from node-ids)
    let addr_a = node_a.endpoint_addr();
    let addr_b = node_b.endpoint_addr();
    node_a.sync_with(vec![addr_b]).await.unwrap();
    node_b.sync_with(vec![addr_a]).await.unwrap();

    let engine_a = Engine::new(
        PiAdapter::new(&root_a),
        AgeIdentity::from_secret_string(&age).unwrap(),
        node_a,
    );
    let engine_b = Engine::new(
        PiAdapter::new(&root_b),
        AgeIdentity::from_secret_string(&age).unwrap(),
        node_b,
    );

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

    let node_a = Node::spawn(&base.join("a/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut engine_a = Engine::new(
        PiAdapter::new(&root_a),
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_a,
    );
    engine_a.create_namespace().await.unwrap();
    let ticket = engine_a.share().await.unwrap();

    let node_b = Node::spawn(&base.join("b/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut engine_b = Engine::new(
        PiAdapter::new(&root_b),
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_b,
    );
    engine_b.join(ticket).await.unwrap();

    std::fs::write(root_a.join(rel), b"header\nto-be-deleted\n").unwrap();

    let sa = base.join("a/status.toml");
    let sb = base.join("b/status.toml");
    tokio::spawn(async move { engine_a.run(&sa).await });
    tokio::spawn(async move { engine_b.run(&sb).await });

    // it appears on B
    let dest = root_b.join(rel);
    let mut appeared = false;
    for _ in 0..40 {
        if dest.exists() {
            appeared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(appeared, "session never reached B");

    // delete on A -> must disappear on B and stay gone
    std::fs::remove_file(root_a.join(rel)).unwrap();
    let mut gone = false;
    for _ in 0..80 {
        if !dest.exists() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(gone, "deletion did not propagate to B");

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

    let node_a = Node::spawn(&base.join("a/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut engine_a = Engine::new(
        PiAdapter::new(&root_a),
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_a,
    );
    engine_a.create_namespace().await.unwrap();
    let ticket = engine_a.share().await.unwrap();

    let node_b = Node::spawn(&base.join("b/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut engine_b = Engine::new(
        PiAdapter::new(&root_b),
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_b,
    );
    engine_b.join(ticket).await.unwrap();

    let sa = base.join("a/status.toml");
    let sb = base.join("b/status.toml");
    tokio::spawn(async move { engine_a.run(&sa).await });
    tokio::spawn(async move { engine_b.run(&sb).await });

    // both sessions reach B (the keep session too, so B's dir never goes
    // empty after the delete — the empty-dir guard would swallow the tombstone)
    let on_b = root_b.join(rel);
    let keep_on_b = root_b.join("--proj--/keep_019e0000keepkeepkeep71acbe20keep00001.jsonl");
    let mut appeared = false;
    for _ in 0..40 {
        if on_b.exists() && keep_on_b.exists() {
            appeared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(appeared, "sessions never reached B");

    // delete on B (NOT the author) -> must disappear on A and stay gone
    std::fs::remove_file(&on_b).unwrap();
    let on_a = root_a.join(rel);
    let mut gone = false;
    for _ in 0..80 {
        if !on_a.exists() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(gone, "deletion by non-author did not propagate back to A");

    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(!on_a.exists(), "deleted session resurrected on A");
    assert!(!on_b.exists(), "deleted session resurrected on B");
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

    let node_a = Node::spawn(&base.join("a/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut engine_a = Engine::new(
        PiAdapter::new(&root_a),
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_a,
    );
    engine_a.create_namespace().await.unwrap();
    let ticket = engine_a.share().await.unwrap();

    let node_b = Node::spawn(&base.join("b/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut engine_b = Engine::new(
        PiAdapter::new(&root_b),
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_b,
    );
    engine_b.join(ticket).await.unwrap();

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

    // node A imports its version of the session
    let root_a = base.join("a/sessions");
    let src_a = root_a.join(rel);
    std::fs::create_dir_all(src_a.parent().unwrap()).unwrap();
    std::fs::write(&src_a, b"version from A\n").unwrap();
    let node_a = Node::spawn(&base.join("a/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut engine_a = Engine::new(
        PiAdapter::new(&root_a),
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_a,
    );
    engine_a.create_namespace().await.unwrap();
    engine_a.import_file(&src_a).await.unwrap();
    let ticket = engine_a.share().await.unwrap();

    // node B joins, then imports its OWN divergent version of the same session
    let root_b = base.join("b/sessions");
    let src_b = root_b.join(rel);
    std::fs::create_dir_all(src_b.parent().unwrap()).unwrap();
    std::fs::write(&src_b, b"different version from B\n").unwrap();
    let node_b = Node::spawn(&base.join("b/data"), SecretKey::generate())
        .await
        .unwrap();
    let mut engine_b = Engine::new(
        PiAdapter::new(&root_b),
        AgeIdentity::from_secret_string(&secret).unwrap(),
        node_b,
    );
    engine_b.join(ticket).await.unwrap();
    engine_b.import_file(&src_b).await.unwrap();

    // once A's entry syncs in, B sees two authors for the same key: a conflict.
    let mut ok = false;
    for _ in 0..60 {
        if !engine_b.conflict_paths().await.unwrap().is_empty() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "divergent writes were not detected as a conflict");
}
