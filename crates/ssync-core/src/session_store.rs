//! Session bytes at rest: the one place that pairs the age identity with the
//! mesh. Plaintext goes in, plaintext comes out; ciphertext, blob hashes and
//! the publish ordering rule stay inside.
//!
//! Two shared references, so the same store is reachable from `&self` methods
//! and from inside a pass that already holds `&mut Engine` — the split that
//! used to force every read helper to exist twice.

use anyhow::Result;
use ssync_crypto::AgeIdentity;
use ssync_net::Node;
use ssync_net::iroh_blobs::Hash;

#[derive(Clone, Copy)]
pub(crate) struct SessionStore<'a> {
    identity: &'a AgeIdentity,
    node: &'a Node,
}

impl<'a> SessionStore<'a> {
    pub(crate) fn new(identity: &'a AgeIdentity, node: &'a Node) -> Self {
        Self { identity, node }
    }

    /// Encrypt and publish under `key`, returning the blob hash. The only
    /// write path: [`Node::publish`] holds the blob behind a temp tag until
    /// the index entry that protects it from GC exists.
    pub(crate) async fn publish(&self, key: &str, plaintext: &[u8]) -> Result<Hash> {
        let ciphertext = self.identity.encrypt(plaintext).await?;
        self.node.publish(key.to_string(), ciphertext).await
    }

    /// Plaintext for a blob **already held locally**; `None` on a local miss
    /// or undecryptable ciphertext. Divergence depends on the miss: a short
    /// version set reads as incomplete, and an all-or-skip merge is what keeps
    /// a partial union from dropping a fork's lines (DECISIONS §8). Never
    /// swap this for [`fetch_plaintext`](Self::fetch_plaintext).
    pub(crate) async fn local_plaintext(&self, hash: Hash) -> Option<Vec<u8>> {
        let ciphertext = self.node.get_blob(hash).await.ok()?;
        self.identity.decrypt(&ciphertext).await.ok()
    }

    /// Plaintext for a blob, fetching from known peers on a local miss —
    /// iroh-docs never retries a missed content download (iroh-docs#88).
    pub(crate) async fn fetch_plaintext(&self, hash: Hash) -> Result<Vec<u8>> {
        let ciphertext = self.node.blob(hash).await?;
        self.identity.decrypt(&ciphertext).await
    }

    /// Delete this node's entry for `key` (a tombstone that syncs).
    pub(crate) async fn tombstone(&self, key: &str) -> Result<()> {
        self.node.index_delete(key).await
    }

    pub(crate) fn node(&self) -> &'a Node {
        self.node
    }
}

/// Fingerprint of the recipient set blobs are encrypted to. A mismatch with
/// the set recorded in `SyncState` forces a full re-publish: plaintext dedup
/// alone would keep ciphertext readable by a removed key and unreadable by an
/// added one (issue #22).
pub(crate) fn recipients_fingerprint(identity: &AgeIdentity) -> String {
    Hash::new(identity.recipients().join("\n")).to_string()
}
