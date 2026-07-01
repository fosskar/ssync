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
use ssync_net::iroh::SecretKey;
use ssync_net::Node;

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
        if let Ok(got) = std::fs::read(&dest) {
            if got == contents {
                ok = true;
                break;
            }
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
        if let Ok(got) = std::fs::read(&dest) {
            if got == contents {
                ok = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(ok, "live write after startup did not propagate to node B");
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
