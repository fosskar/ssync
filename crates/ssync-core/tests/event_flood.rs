//! Regression: a large initial import must not deadlock the daemon.
//!
//! iroh-docs delivers replica events to subscribers over bounded channels and
//! the docs actor *awaits* those sends. The daemon reads no events while
//! `step` runs, so a big enough publish flood (local inserts + remote
//! inserts, ~1100 events) filled every buffer and wedged the actor — the
//! daemon then hung forever mid-initial-import (seen in prod on every boot
//! with ~560 sessions per side). The fix drains the subscription on a
//! dedicated task (now inside `Node::signals`) so the channel never backs up.

mod harness;

use std::time::Duration;

use harness::*;

#[tokio::test(flavor = "multi_thread")]
async fn initial_import_of_many_sessions_does_not_wedge() {
    let sim = Sim::new("event-flood").await;
    let mut node = sim.node("n").await;
    node.create_namespace().await.unwrap();
    let peer = sim.pi_peer("n", "pi", node).await;
    for i in 0..1500u32 {
        peer.write(
            format!(
                "--proj--/2026-01-01T00-00-00-000Z_019e{i:04x}f00d1eee71acbe20flood{i:04x}.jsonl"
            ),
            format!("payload {i}\n"),
        );
    }

    // run() writes the status snapshot right after the initial reconcile; a
    // wedged actor means it never appears.
    let status = peer.dir.join("status.toml");
    peer.run();

    // longer than eventually(): 1500 imports take a while under CI load
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
