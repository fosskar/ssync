//! iroh node (endpoint, blobs, docs, gossip, router) and peering (DECISIONS §5/§6).
//! Stores ciphertext blobs plus a synced key-value index.

use std::path::Path;

use anyhow::{Context, Result};
use futures_lite::{Stream, StreamExt};
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointId, SecretKey};
use iroh_blobs::store::fs::FsStore;
use iroh_blobs::{BlobsProtocol, Hash};
use iroh_docs::api::protocol::{AddrInfoOptions, ShareMode};
use iroh_docs::api::Doc;
use iroh_docs::engine::LiveEvent;
use iroh_docs::protocol::Docs;
use iroh_docs::store::Query;
use iroh_docs::{AuthorId, DocTicket, NamespaceId};
use iroh_gossip::net::Gossip;
pub use {iroh, iroh_blobs, iroh_docs};

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

impl Node {
    /// No index namespace is active until one is created, opened or joined.
    pub async fn spawn(data_dir: &Path, secret_key: SecretKey) -> Result<Self> {
        tokio::fs::create_dir_all(data_dir)
            .await
            .with_context(|| format!("creating data dir {}", data_dir.display()))?;

        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .bind()
            .await
            .context("binding iroh endpoint")?;

        let blobs = FsStore::load(data_dir.join("blobs"))
            .await
            .context("opening blob store")?;

        let gossip = Gossip::builder().spawn(endpoint.clone());

        let docs_dir = data_dir.join("docs");
        tokio::fs::create_dir_all(&docs_dir)
            .await
            .context("creating docs dir")?;
        let docs = Docs::persistent(docs_dir)
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
    pub async fn subscribe(&self) -> Result<impl Stream<Item = Result<LiveEvent>>> {
        self.doc()?.subscribe().await
    }

    pub async fn add_blob(&self, data: Vec<u8>) -> Result<Hash> {
        let tag = self.blobs.blobs().add_bytes(data).await?;
        Ok(tag.hash)
    }

    pub async fn get_blob(&self, hash: Hash) -> Result<Vec<u8>> {
        let bytes = self.blobs.blobs().get_bytes(hash).await?;
        Ok(bytes.to_vec())
    }

    pub async fn index_set(
        &self,
        key: impl Into<bytes::Bytes>,
        hash: Hash,
        size: u64,
    ) -> Result<()> {
        self.doc()?.set_hash(self.author, key, hash, size).await?;
        Ok(())
    }

    /// Winning entry for `key` (newest across all authors). Not author-scoped, so
    /// a node sees a peer's version even if it never wrote the key (loop-prevention).
    pub async fn index_get(&self, key: impl AsRef<[u8]>) -> Result<Option<Hash>> {
        let query = Query::single_latest_per_key().key_exact(key);
        let entry = self.doc()?.get_one(query).await?;
        Ok(entry.map(|e| e.content_hash()))
    }

    /// Every author's current entry as `(key, timestamp, hash)` (multiple rows
    /// per key when several machines wrote it). Used for content-based conflict
    /// resolution.
    pub async fn index_entries_full(&self) -> Result<Vec<(Vec<u8>, u64, Hash)>> {
        let stream = self.doc()?.get_many(Query::all()).await?;
        let mut stream = std::pin::pin!(stream);
        let mut out = Vec::new();
        while let Some(entry) = stream.next().await {
            let entry = entry?;
            out.push((
                entry.key().to_vec(),
                entry.timestamp(),
                entry.content_hash(),
            ));
        }
        Ok(out)
    }

    /// Delete this node's entry for `key` (append-only tombstone that syncs).
    pub async fn index_delete(&self, key: impl AsRef<[u8]>) -> Result<()> {
        self.doc()?.del(self.author, key.as_ref().to_vec()).await?;
        Ok(())
    }

    /// Winning `(key, hash)` per key, newest wins (DECISIONS §8).
    pub async fn index_latest(&self) -> Result<Vec<(Vec<u8>, Hash)>> {
        let stream = self.doc()?.get_many(Query::single_latest_per_key()).await?;
        let mut stream = std::pin::pin!(stream);
        let mut out = Vec::new();
        while let Some(entry) = stream.next().await {
            let entry = entry?;
            out.push((entry.key().to_vec(), entry.content_hash()));
        }
        Ok(out)
    }

    /// Keys written independently by more than one author (machine) — a conflict.
    /// Both versions are retained as blobs (DECISIONS §8).
    pub async fn conflicts(&self) -> Result<Vec<Vec<u8>>> {
        use std::collections::{BTreeMap, BTreeSet};
        let stream = self.doc()?.get_many(Query::all()).await?;
        let mut stream = std::pin::pin!(stream);
        let mut by_key: BTreeMap<Vec<u8>, BTreeSet<String>> = BTreeMap::new();
        while let Some(entry) = stream.next().await {
            let entry = entry?;
            by_key
                .entry(entry.key().to_vec())
                .or_default()
                .insert(entry.author().to_string());
        }
        Ok(by_key
            .into_iter()
            .filter(|(_, authors)| authors.len() > 1)
            .map(|(key, _)| key)
            .collect())
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

        let data = b"ciphertext".to_vec();
        let hash = node.add_blob(data.clone()).await.unwrap();
        node.index_set("pi/proj/session-1", hash, data.len() as u64)
            .await
            .unwrap();

        let got = node.index_get("pi/proj/session-1").await.unwrap();
        assert_eq!(got, Some(hash));
        assert_eq!(node.index_get("missing").await.unwrap(), None);

        node.shutdown().await.unwrap();
    }

    fn tempdir(tag: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("ssync-net-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }
}
