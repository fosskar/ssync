//! Regression: a large initial import must not deadlock the daemon.
//!
//! iroh-docs delivers replica events to subscribers over bounded channels and
//! the docs actor *awaits* those sends. `Engine::run` subscribes but reads no
//! events while `step` runs, so a big enough publish flood (local inserts +
//! remote inserts, ~1100 events) filled every buffer and wedged the actor —
//! the daemon then hung forever mid-initial-import (seen in prod on every
//! boot with ~560 sessions per side). The fix drains the subscription on a
//! dedicated task so the channel never backs up.

use std::path::PathBuf;
use std::time::Duration;

use ssync_adapters::pi::PiAdapter;
use ssync_core::Engine;
use ssync_crypto::AgeIdentity;
use ssync_net::Node;
use ssync_net::iroh::SecretKey;

fn scratch() -> PathBuf {
    let base = std::env::temp_dir().join(format!("ssync-event-flood-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

#[tokio::test(flavor = "multi_thread")]
async fn initial_import_of_many_sessions_does_not_wedge() {
    let base = scratch();
    let age = AgeIdentity::generate().unwrap().to_secret_string();

    let root = base.join("sessions");
    std::fs::create_dir_all(root.join("--proj--")).unwrap();
    for i in 0..1500u32 {
        let name = format!(
            "--proj--/2026-01-01T00-00-00-000Z_019e{i:04x}f00d1eee71acbe20flood{i:04x}.jsonl"
        );
        std::fs::write(root.join(name), format!("payload {i}\n")).unwrap();
    }

    let mut node = Node::spawn(&base.join("data"), SecretKey::generate())
        .await
        .unwrap();
    node.create_namespace().await.unwrap();
    let mut engine = Engine::new(
        PiAdapter::new("pi", &root),
        AgeIdentity::from_secret_string(&age).unwrap(),
        node,
    );

    // run() writes the status snapshot right after the initial reconcile; a
    // wedged actor means it never appears.
    let status = base.join("status.toml");
    let status2 = status.clone();
    tokio::spawn(async move { engine.run(&status2).await });

    let mut ok = false;
    for _ in 0..240 {
        if status.exists() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(
        ok,
        "daemon wedged during initial import (status never written)"
    );
}
