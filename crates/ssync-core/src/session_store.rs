//! Session bytes at rest: the one place that pairs the age identity with the
//! mesh. Plaintext goes in, plaintext comes out; ciphertext, blob hashes and
//! the publish ordering rule stay inside.
//!
//! Two shared references, so the same store is reachable from `&self` methods
//! and from inside a pass that already holds `&mut Engine` — the split that
//! used to force every read helper to exist twice.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Result, bail};
use ssync_crypto::AgeIdentity;
use ssync_net::Node;
use ssync_net::iroh_blobs::Hash;

/// Blobs this node's identity cannot decrypt. Decryption is a pure function of
/// (ciphertext, identity) and the identity is fixed for the process, so a
/// failure is permanent: without this set the daemon re-spawns `age` for the
/// same blob on every tick, forever (issue #109).
#[derive(Default)]
pub(crate) struct DecryptFailures {
    hashes: Mutex<HashSet<Hash>>,
    attempts: AtomicUsize,
}

impl DecryptFailures {
    fn known(&self, hash: Hash) -> bool {
        self.hashes.lock().unwrap().contains(&hash)
    }

    fn record(&self, hash: Hash, error: &anyhow::Error) {
        if self.hashes.lock().unwrap().insert(hash) {
            eprintln!("ssync: blob {hash} stays undecryptable until restart: {error:#}");
        }
    }

    fn attempt(&self) {
        self.attempts.fetch_add(1, Ordering::Relaxed);
    }

    /// Decryptions actually handed to `age`; the suppression this type exists
    /// for is only observable as this count staying flat.
    #[cfg(test)]
    pub(crate) fn attempts(&self) -> usize {
        self.attempts.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct SessionStore<'a> {
    identity: &'a AgeIdentity,
    node: &'a Node,
    failures: &'a DecryptFailures,
}

impl<'a> SessionStore<'a> {
    pub(crate) fn new(
        identity: &'a AgeIdentity,
        node: &'a Node,
        failures: &'a DecryptFailures,
    ) -> Self {
        Self {
            identity,
            node,
            failures,
        }
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
        if self.failures.known(hash) {
            return None;
        }
        let ciphertext = self.node.get_blob(hash).await.ok()?;
        self.failures.attempt();
        match self.identity.decrypt(&ciphertext).await {
            Ok(plaintext) => Some(plaintext),
            Err(error) => {
                self.failures.record(hash, &error);
                None
            }
        }
    }

    /// Plaintext for a blob, fetching from known peers on a local miss —
    /// iroh-docs never retries a missed content download (iroh-docs#88).
    pub(crate) async fn fetch_plaintext(&self, hash: Hash) -> Result<Vec<u8>> {
        if self.failures.known(hash) {
            bail!("blob {hash} is not decryptable with this identity");
        }
        let ciphertext = self.node.blob(hash).await?;
        self.failures.attempt();
        match self.identity.decrypt(&ciphertext).await {
            Ok(plaintext) => Ok(plaintext),
            Err(error) => {
                self.failures.record(hash, &error);
                Err(error)
            }
        }
    }

    /// Whether `hash` is known to be undecryptable with this identity: a
    /// permanent condition, unlike a blob merely not fetched yet.
    pub(crate) fn undecryptable(&self, hash: Hash) -> bool {
        self.failures.known(hash)
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
