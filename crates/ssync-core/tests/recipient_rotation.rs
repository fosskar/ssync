//! Issue #22 regressions: a recipient-set change must re-publish (re-encrypt)
//! every local session under the new set, and a namespace rotation must drop
//! the stale replica — otherwise a revoked machine keeps reading old blobs
//! (plaintext dedup never re-encrypts unchanged sessions) or keeps syncing the
//! abandoned namespace.

use std::path::PathBuf;
use std::time::Duration;

use ssync_adapters::pi::PiAdapter;
use ssync_core::Engine;
use ssync_crypto::AgeIdentity;
use ssync_net::Node;
use ssync_net::iroh::SecretKey;
use ssync_net::iroh_blobs::Hash;

fn scratch(tag: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!("ssync-rotation-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

async fn winner_of(node: &Node, key: &str) -> Option<Hash> {
    node.index_record(key)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.winner)
}

#[tokio::test]
async fn recipient_change_republishes_unchanged_sessions() {
    let base = scratch("recipients");
    let id_a = AgeIdentity::generate().unwrap();
    let id_b = AgeIdentity::generate().unwrap();

    let root = base.join("a/sessions");
    let rel = "--home-simon-Projects-demo--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
    let src = root.join(rel);
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    let contents = b"{\"type\":\"session\",\"version\":3}\n{\"msg\":\"unchanged\"}\n";
    std::fs::write(&src, contents).unwrap();
    let key = format!("pi/{rel}");

    let node_key = SecretKey::generate();
    let mut node_a = Node::spawn(&base.join("a/data"), node_key.clone())
        .await
        .unwrap();
    let ns = node_a.create_namespace().await.unwrap();
    let ticket = node_a.share().await.unwrap();

    // observer peer: inspects the index and fetches blobs like any machine.
    let mut observer = Node::spawn(&base.join("o/data"), SecretKey::generate())
        .await
        .unwrap();
    observer.join(ticket).await.unwrap();

    // run 1: encrypts to {A} only, persists state (and with it the set).
    let state_path = base.join("a/state.toml");
    let ident1 = AgeIdentity::from_secret_string(&id_a.to_secret_string()).unwrap();
    let mut engine = Engine::new(PiAdapter::new("pi", &root), ident1, node_a);
    engine.persist_state(&state_path);
    engine.tick_once().await;

    let mut h1 = None;
    for _ in 0..60 {
        h1 = winner_of(&observer, &key).await;
        if h1.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let h1 = h1.expect("first publish never reached the observer");
    engine.shutdown().await.unwrap();

    // run 2: same machine, file untouched, recipient set grows by B. The old
    // winner still decrypts fine and the plaintext is identical — only the
    // recipient-set change forces the re-publish.
    let mut node_a2 = Node::spawn(&base.join("a/data"), node_key).await.unwrap();
    node_a2.open_namespace(ns).await.unwrap();
    node_a2
        .sync_with(vec![observer.endpoint_addr()])
        .await
        .unwrap();
    let mut ident2 = AgeIdentity::from_secret_string(&id_a.to_secret_string()).unwrap();
    ident2.add_recipients([id_b.recipient_string()]);
    let mut engine2 = Engine::new(PiAdapter::new("pi", &root), ident2, node_a2);
    engine2.persist_state(&state_path);
    engine2.tick_once().await;

    let mut h2 = None;
    for _ in 0..60 {
        if let Some(w) = winner_of(&observer, &key).await
            && w != h1
        {
            h2 = Some(w);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let h2 = h2.expect("recipient-set change did not re-publish the unchanged session");

    // the added machine can decrypt the re-published blob…
    let mut plain = None;
    for _ in 0..60 {
        if let Ok(ciphertext) = observer.get_blob(h2).await {
            plain = AgeIdentity::from_secret_string(&id_b.to_secret_string())
                .unwrap()
                .decrypt(&ciphertext)
                .ok();
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert_eq!(
        plain.as_deref(),
        Some(contents.as_slice()),
        "added recipient cannot decrypt the re-published blob"
    );

    // …and once the set is settled, further ticks do not churn.
    engine2.tick_once().await;
    engine2.tick_once().await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        winner_of(&observer, &key).await,
        Some(h2),
        "settled recipient set must not re-publish again"
    );
}

#[tokio::test]
async fn namespace_rotation_drops_stale_replica() {
    let base = scratch("nsrotate");
    let mut node = Node::spawn(&base.join("data"), SecretKey::generate())
        .await
        .unwrap();
    let old = node.create_namespace().await.unwrap();
    node.publish("pi/x", b"old ciphertext".to_vec())
        .await
        .unwrap();

    // rotated shared secret ⇒ new deterministic namespace; the old replica
    // must not linger in the store (a revoked peer still holds its secret).
    let new = node.open_shared_namespace([7u8; 32]).await.unwrap();
    let dropped = node.drop_stale_replicas().await.unwrap();
    assert_eq!(dropped, vec![old]);
    assert_eq!(node.namespace(), Some(new));

    // idempotent, and the fresh namespace still works.
    assert!(node.drop_stale_replicas().await.unwrap().is_empty());
    node.publish("pi/y", b"new ciphertext".to_vec())
        .await
        .unwrap();
}
