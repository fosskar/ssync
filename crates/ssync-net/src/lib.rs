//! iroh node (endpoint, blobs, docs, gossip, router) and peering (DECISIONS §5/§6).
//! Stores ciphertext blobs plus a synced key-value index.

use std::path::Path;

use anyhow::{Context, Result};
use futures_lite::{Stream, StreamExt};
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use iroh_blobs::store::fs::{FsStore, options::Options as FsStoreOptions};
use iroh_blobs::store::{GcConfig, ProtectCb};
use iroh_blobs::{BlobsProtocol, Hash};
use iroh_docs::api::Doc;
use iroh_docs::api::protocol::{AddrInfoOptions, ShareMode};
use iroh_docs::engine::LiveEvent;
use iroh_docs::engine::ProtectCallbackHandler;
use iroh_docs::protocol::Docs;
use iroh_docs::store::Query;
use iroh_docs::{AuthorId, DocTicket, NamespaceId};
use iroh_docs::{Capability, NamespaceSecret};
use iroh_gossip::net::Gossip;
pub use {iroh, iroh_blobs, iroh_docs};

/// Map iroh-docs' tombstone sentinel (`Hash::EMPTY` content) to `None`.
fn live_hash(hash: Hash) -> Option<Hash> {
    (hash != Hash::EMPTY).then_some(hash)
}

/// The full synced state of one index key, computed once behind the seam: the
/// winner (newest across all authors) plus every distinct live version. A
/// winning tombstone reads as `winner = None`; `versions` holds the
/// non-tombstone content hashes (len > 1 means the key has genuinely diverged).
#[derive(Debug, Clone)]
pub struct IndexRecord {
    pub key: Vec<u8>,
    pub winner_ts: u64,
    pub winner: Option<Hash>,
    pub versions: Vec<Hash>,
}

/// A fresh random 32-byte key seed (usable as a node key or namespace secret).
pub fn generate_key_bytes() -> [u8; 32] {
    SecretKey::generate().to_bytes()
}

/// The iroh node-id (public key) for a 32-byte node key.
pub fn node_id_of(key_bytes: &[u8; 32]) -> String {
    SecretKey::from_bytes(key_bytes).public().to_string()
}

/// Parse peer node-id strings into addresses, tolerating surrounding whitespace
/// (e.g. a trailing newline from a generated file) and skipping blank or
/// malformed entries rather than failing the whole daemon.
fn parse_peer_addrs(node_ids: &[String]) -> Vec<EndpointAddr> {
    let mut addrs = Vec::new();
    for s in node_ids {
        let t = s.trim();
        if t.is_empty() {
            continue;
        }
        match t.parse::<EndpointId>() {
            Ok(id) => addrs.push(EndpointAddr::from(id)),
            Err(e) => eprintln!("ssync: bad peer node-id {t:?}: {e}"),
        }
    }
    addrs
}

/// Load the iroh secret key, generating and persisting one on first run so the
/// node's public identity is stable across restarts.
pub async fn load_or_create_secret_key(path: &Path) -> Result<SecretKey> {
    if let Ok(bytes) = tokio::fs::read(path).await {
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("node key at {} must be 32 bytes", path.display()))?;
        Ok(SecretKey::from_bytes(&arr))
    } else {
        let key = SecretKey::generate();
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::write(path, key.to_bytes())
            .await
            .with_context(|| format!("writing node key {}", path.display()))?;
        Ok(key)
    }
}

/// A running iroh node: blob store plus a synced key-value index (one iroh-docs
/// namespace). Blobs hold age-ciphertext.
pub struct Node {
    endpoint: Endpoint,
    blobs: FsStore,
    docs: Docs,
    author: AuthorId,
    doc: Option<Doc>,
    _router: Router,
}

/// Default blob GC interval. Blobs referenced by any author's current index
/// entry are protected (via iroh-docs' protect callback); superseded ciphertext
/// is swept. Content-addressed, so nothing referenced is ever lost.
const GC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

impl Node {
    /// No index namespace is active until one is created, opened or joined.
    pub async fn spawn(data_dir: &Path, secret_key: SecretKey) -> Result<Self> {
        Self::spawn_with_gc(data_dir, secret_key, GC_INTERVAL).await
    }

    /// [`spawn`](Self::spawn) with a custom blob-GC interval (tests).
    pub async fn spawn_with_gc(
        data_dir: &Path,
        secret_key: SecretKey,
        gc_interval: std::time::Duration,
    ) -> Result<Self> {
        tokio::fs::create_dir_all(data_dir)
            .await
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;

        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .bind()
            .await
            .context("binding iroh endpoint")?;

        // GC: iroh-docs feeds the protect callback every hash referenced by a
        // current doc entry (any author, any replica); everything else sweeps.
        let (protect_handler, protect_cb): (ProtectCallbackHandler, ProtectCb) =
            ProtectCallbackHandler::new();
        let blobs_dir = data_dir.join("blobs");
        tokio::fs::create_dir_all(&blobs_dir)
            .await
            .context("creating blobs dir")?;
        let mut store_opts = FsStoreOptions::new(&blobs_dir);
        store_opts.gc = Some(GcConfig {
            interval: gc_interval,
            add_protected: Some(protect_cb),
        });
        let blobs = FsStore::load_with_opts(blobs_dir.join("blobs.db"), store_opts)
            .await
            .context("opening blob store")?;

        // Pre-GC versions of ssync tagged every published blob permanently,
        // pinning superseded ciphertext forever. The index entries are the
        // real references now; drop the legacy tags so old blobs can sweep.
        blobs.tags().delete_all().await.context("clearing tags")?;

        let gossip = Gossip::builder().spawn(endpoint.clone());

        let docs_dir = data_dir.join("docs");
        tokio::fs::create_dir_all(&docs_dir)
            .await
            .context("creating docs dir")?;
        let docs = Docs::persistent(docs_dir)
            .protect_handler(protect_handler)
            .spawn(endpoint.clone(), (*blobs).clone(), gossip.clone())
            .await
            .context("spawning docs")?;

        let router = Router::builder(endpoint.clone())
            .accept(iroh_blobs::ALPN, BlobsProtocol::new(&blobs, None))
            .accept(iroh_gossip::net::GOSSIP_ALPN, gossip)
            .accept(iroh_docs::ALPN, docs.clone())
            .spawn();

        let author = docs
            .api()
            .author_default()
            .await
            .context("default author")?;

        Ok(Self {
            endpoint,
            blobs,
            docs,
            author,
            doc: None,
            _router: router,
        })
    }

    fn doc(&self) -> Result<&Doc> {
        self.doc
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no index namespace active"))
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    pub fn namespace(&self) -> Option<NamespaceId> {
        self.doc.as_ref().map(|d| d.id())
    }

    /// Returns the new namespace id; persist it to reopen on restart.
    pub async fn create_namespace(&mut self) -> Result<NamespaceId> {
        let doc = self
            .docs
            .api()
            .create()
            .await
            .context("creating namespace")?;
        let id = doc.id();
        self.doc = Some(doc);
        Ok(id)
    }

    pub async fn open_namespace(&mut self, id: NamespaceId) -> Result<()> {
        let doc = self
            .docs
            .api()
            .open(id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("namespace {id} not found in store"))?;
        self.doc = Some(doc);
        Ok(())
    }

    /// Open the deterministic namespace derived from a shared 32-byte secret
    /// (the same secret on every peer yields the same namespace — no ticket
    /// exchange). Distributed by clan.vars.
    pub async fn open_shared_namespace(&mut self, secret: [u8; 32]) -> Result<NamespaceId> {
        let ns = NamespaceSecret::from_bytes(&secret);
        let doc = self
            .docs
            .api()
            .import_namespace(Capability::Write(ns))
            .await
            .context("importing shared namespace")?;
        let id = doc.id();
        self.doc = Some(doc);
        Ok(id)
    }

    /// This node's dialable address (node-id plus known transport addresses).
    pub fn endpoint_addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Start syncing the active namespace with the given peer addresses.
    pub async fn sync_with(&self, addrs: Vec<EndpointAddr>) -> Result<()> {
        if !addrs.is_empty() {
            self.doc()?.start_sync(addrs).await?;
        }
        Ok(())
    }

    /// Start syncing with the given peer node-ids. Addresses are resolved via
    /// iroh discovery, so only the node-ids are needed.
    pub async fn sync_with_peers(&self, node_ids: &[String]) -> Result<()> {
        self.sync_with(parse_peer_addrs(node_ids)).await
    }

    /// Import `ticket`'s namespace, start syncing, make it active.
    pub async fn join(&mut self, ticket: DocTicket) -> Result<NamespaceId> {
        let (doc, _events) = self.docs.api().import_and_subscribe(ticket).await?;
        let id = doc.id();
        self.doc = Some(doc);
        Ok(id)
    }

    /// Write-capable ticket to the active namespace, with direct addresses.
    pub async fn share(&self) -> Result<DocTicket> {
        self.doc()?
            .share(ShareMode::Write, AddrInfoOptions::RelayAndAddresses)
            .await
    }

    /// Live event stream for the active namespace; drives the exporter.
    pub async fn subscribe(&self) -> Result<impl Stream<Item = Result<LiveEvent>> + use<>> {
        self.doc()?.subscribe().await
    }

    /// Store `data` as a blob and set it as this author's entry for `key` —
    /// the only write path. The blob is held by a temp tag until the index
    /// entry exists (its reference is what protects the blob from GC), so the
    /// blob is never sweepable in between.
    pub async fn publish(&self, key: impl Into<bytes::Bytes>, data: Vec<u8>) -> Result<Hash> {
        let size = data.len() as u64;
        let tag = self.blobs.blobs().add_bytes(data).temp_tag().await?;
        let hash = tag.hash();
        self.doc()?.set_hash(self.author, key, hash, size).await?;
        drop(tag); // entry written; the doc reference now protects the blob
        Ok(hash)
    }

    /// Store a blob without an index reference (tests). Unreferenced blobs are
    /// swept by the next GC run.
    pub async fn add_blob(&self, data: Vec<u8>) -> Result<Hash> {
        let tag = self.blobs.blobs().add_bytes(data).temp_tag().await?;
        Ok(tag.hash())
    }

    pub async fn get_blob(&self, hash: Hash) -> Result<Vec<u8>> {
        let bytes = self.blobs.blobs().get_bytes(hash).await?;
        Ok(bytes.to_vec())
    }

    /// Delete this node's entry for `key` (append-only tombstone that syncs).
    pub async fn index_delete(&self, key: impl AsRef<[u8]>) -> Result<()> {
        self.doc()?.del(self.author, key.as_ref().to_vec()).await?;
        Ok(())
    }

    /// Collect every author's live hashes per key (for divergence/merge). Keyed
    /// by the raw index key; only non-tombstone content is recorded.
    async fn live_versions(
        &self,
        query: impl Into<Query>,
    ) -> Result<std::collections::HashMap<Vec<u8>, Vec<Hash>>> {
        let stream = self.doc()?.get_many(query).await?;
        let mut stream = std::pin::pin!(stream);
        let mut out: std::collections::HashMap<Vec<u8>, Vec<Hash>> =
            std::collections::HashMap::new();
        while let Some(entry) = stream.next().await {
            let entry = entry?;
            if let Some(h) = live_hash(entry.content_hash()) {
                let versions = out.entry(entry.key().to_vec()).or_default();
                if !versions.contains(&h) {
                    versions.push(h);
                }
            }
        }
        Ok(out)
    }

    /// The full synced state of one key: the winner (newest across all authors)
    /// plus every distinct live version. `None` when the key was never written.
    pub async fn index_record(&self, key: impl AsRef<[u8]>) -> Result<Option<IndexRecord>> {
        let key = key.as_ref();
        let winner = self
            .doc()?
            .get_one(
                Query::single_latest_per_key()
                    .key_exact(key)
                    .include_empty(),
            )
            .await?;
        let Some(winner) = winner else {
            return Ok(None);
        };
        let mut versions = self
            .live_versions(Query::all().key_exact(key).include_empty())
            .await?;
        Ok(Some(IndexRecord {
            key: key.to_vec(),
            winner_ts: winner.timestamp(),
            winner: live_hash(winner.content_hash()),
            versions: versions.remove(key).unwrap_or_default(),
        }))
    }

    /// One [`IndexRecord`] per key. Winner selection and version grouping happen
    /// here, once, so callers never re-derive them from raw entries (DECISIONS §8).
    pub async fn index_records(&self) -> Result<Vec<IndexRecord>> {
        let mut versions = self.live_versions(Query::all().include_empty()).await?;
        let stream = self
            .doc()?
            .get_many(Query::single_latest_per_key().include_empty())
            .await?;
        let mut stream = std::pin::pin!(stream);
        let mut out = Vec::new();
        while let Some(entry) = stream.next().await {
            let entry = entry?;
            let key = entry.key().to_vec();
            let versions = versions.remove(&key).unwrap_or_default();
            out.push(IndexRecord {
                key,
                winner_ts: entry.timestamp(),
                winner: live_hash(entry.content_hash()),
                versions,
            });
        }
        Ok(out)
    }

    pub async fn shutdown(self) -> Result<()> {
        self.blobs.shutdown().await?;
        self.endpoint.close().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_peer_addrs_trims_and_skips_junk() {
        let id1 = node_id_of(&generate_key_bytes());
        let id2 = node_id_of(&generate_key_bytes());
        let input = vec![
            format!("{id1}\n"),          // trailing newline (the deploy bug)
            format!("  {id2}  "),        // surrounding whitespace
            String::new(),               // blank
            "not-a-node-id".to_string(), // garbage
        ];
        let addrs = parse_peer_addrs(&input);
        assert_eq!(
            addrs.len(),
            2,
            "keep the two valid ids, skip blank + garbage"
        );
        let got: Vec<String> = addrs.iter().map(|a| a.id.to_string()).collect();
        assert!(got.contains(&id1));
        assert!(got.contains(&id2));
    }

    #[tokio::test]
    async fn blob_round_trips_through_store() {
        let dir = tempdir("blob");
        let node = Node::spawn(&dir, SecretKey::generate()).await.unwrap();

        let data = b"encrypted-session-bytes".to_vec();
        let hash = node.add_blob(data.clone()).await.unwrap();
        let got = node.get_blob(hash).await.unwrap();
        assert_eq!(got, data);

        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn index_maps_key_to_blob_hash() {
        let dir = tempdir("index");
        let mut node = Node::spawn(&dir, SecretKey::generate()).await.unwrap();
        node.create_namespace().await.unwrap();

        let hash = node
            .publish("pi/proj/session-1", b"ciphertext".to_vec())
            .await
            .unwrap();

        let rec = node
            .index_record("pi/proj/session-1")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rec.winner, Some(hash));
        assert_eq!(rec.versions, vec![hash]);
        assert!(node.index_record("missing").await.unwrap().is_none());

        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn gc_sweeps_superseded_blob_and_keeps_live_one() {
        let dir = tempdir("gc");
        let mut node = Node::spawn_with_gc(&dir, SecretKey::generate(), GC_TEST_INTERVAL)
            .await
            .unwrap();
        node.create_namespace().await.unwrap();

        let old = node.publish("pi/p/s", b"version-1".to_vec()).await.unwrap();
        let new = node.publish("pi/p/s", b"version-2".to_vec()).await.unwrap();
        assert_ne!(old, new);

        // superseded blob becomes unreferenced (same author, same key) and is
        // swept; the live winner must survive every run.
        let mut swept = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            if node.get_blob(old).await.is_err() {
                swept = true;
                break;
            }
        }
        assert!(swept, "superseded blob was never garbage-collected");
        assert_eq!(node.get_blob(new).await.unwrap(), b"version-2".to_vec());

        node.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn gc_never_sweeps_indexed_blob() {
        let dir = tempdir("gc-keep");
        let mut node = Node::spawn_with_gc(&dir, SecretKey::generate(), GC_TEST_INTERVAL)
            .await
            .unwrap();
        node.create_namespace().await.unwrap();

        let hash = node.publish("pi/p/s", b"keep-me".to_vec()).await.unwrap();
        // wait through several GC runs
        tokio::time::sleep(GC_TEST_INTERVAL * 4).await;
        assert_eq!(node.get_blob(hash).await.unwrap(), b"keep-me".to_vec());

        node.shutdown().await.unwrap();
    }

    const GC_TEST_INTERVAL: std::time::Duration = std::time::Duration::from_millis(300);

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("ssync-net-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }
}
