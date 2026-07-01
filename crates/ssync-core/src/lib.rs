//! config, importer, exporter, index model, conflict logic. Session bytes are
//! stored opaque: read → age-encrypt → blob; the index maps identity → hash.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use futures_lite::StreamExt;
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use ssync_adapters::{Adapter, SessionIdentity};
use ssync_crypto::AgeIdentity;
use ssync_net::iroh_docs::engine::LiveEvent;
use ssync_net::iroh_docs::{DocTicket, NamespaceId};
use ssync_net::Node;

/// On-disk daemon configuration (`$XDG_CONFIG_HOME/ssync/config.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Agent to sync (v1: `"pi"`).
    pub agent: String,
    pub session_dir: PathBuf,
    /// Shared age identity file (same key on every machine).
    pub age_identity_path: PathBuf,
    pub data_dir: PathBuf,
}

impl Config {
    /// Default config path: `$XDG_CONFIG_HOME/ssync/config.toml`.
    pub fn default_path() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .ok_or_else(|| anyhow!("no config dir"))?
            .join("ssync/config.toml"))
    }

    /// Built-in defaults (pi at `~/.pi/agent/sessions`, XDG data/config dirs).
    pub fn defaults() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
        let config = dirs::config_dir().ok_or_else(|| anyhow!("no config dir"))?;
        let data = dirs::data_dir().ok_or_else(|| anyhow!("no data dir"))?;
        Ok(Self {
            agent: "pi".to_string(),
            session_dir: home.join(".pi/agent/sessions"),
            age_identity_path: config.join("ssync/age.key"),
            data_dir: data.join("ssync"),
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing config {}", path.display()))
    }
}

/// A snapshot the daemon writes to `data_dir/status.toml` so `ssync status` /
/// `ssync conflicts` can report without opening the (single-process) store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub namespace: Option<String>,
    pub sessions: usize,
    pub conflicts: Vec<String>,
}

/// The sync engine for a single agent adapter on one node.
pub struct Engine<A: Adapter> {
    adapter: A,
    identity: AgeIdentity,
    node: Node,
}

impl<A: Adapter> Engine<A> {
    pub fn new(adapter: A, identity: AgeIdentity, node: Node) -> Self {
        Self {
            adapter,
            identity,
            node,
        }
    }

    pub async fn create_namespace(&mut self) -> Result<NamespaceId> {
        self.node.create_namespace().await
    }

    pub async fn open_namespace(&mut self, id: NamespaceId) -> Result<()> {
        self.node.open_namespace(id).await
    }

    pub async fn join(&mut self, ticket: DocTicket) -> Result<NamespaceId> {
        self.node.join(ticket).await
    }

    pub async fn share(&self) -> Result<DocTicket> {
        self.node.share().await
    }

    /// The iroh-docs index key for a session: `{agent}/{relative_path}`. The
    /// relative path is machine-independent and carries the write-back location,
    /// so the exporter can reconstruct where the file belongs on any peer.
    fn index_key(&self, id: &SessionIdentity) -> String {
        format!("{}/{}", self.adapter.agent(), id.relative_path.display())
    }

    /// Read → age-encrypt → blob → upsert index entry.
    ///
    /// Loop prevention: age ciphertext is randomized, so dedup on *plaintext* —
    /// skip if the file already matches the indexed version, otherwise the
    /// exporter's own write-back bounces back as a false-conflict entry.
    pub async fn import_file(&self, path: &Path) -> Result<SessionIdentity> {
        let id = self.adapter.identify(path)?;
        let plaintext = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading session file {}", path.display()))?;
        if let Ok(Some(existing)) = self.read_session(&id).await {
            if existing == plaintext {
                return Ok(id);
            }
        }
        let ciphertext = self.identity.encrypt(&plaintext)?;
        let size = ciphertext.len() as u64;
        let hash = self.node.add_blob(ciphertext).await?;
        self.node.index_set(self.index_key(&id), hash, size).await?;
        Ok(id)
    }

    pub async fn import_all(&self) -> Result<usize> {
        let mut n = 0;
        for path in session_files(self.adapter.session_root(), &self.adapter) {
            if let Err(e) = self.import_file(&path).await {
                eprintln!("ssync: import {}: {e:#}", path.display());
            } else {
                n += 1;
            }
        }
        Ok(n)
    }

    pub async fn read_session(&self, id: &SessionIdentity) -> Result<Option<Vec<u8>>> {
        let Some(hash) = self.node.index_get(self.index_key(id)).await? else {
            return Ok(None);
        };
        let ciphertext = self.node.get_blob(hash).await?;
        let plaintext = self.identity.decrypt(&ciphertext)?;
        Ok(Some(plaintext))
    }

    /// Write every indexed session into the session root, decrypted, atomically.
    pub async fn export_all(&self) -> Result<usize> {
        let prefix = format!("{}/", self.adapter.agent());
        let mut written = 0;
        for (key, hash) in self.node.index_latest().await? {
            let key = String::from_utf8(key).context("index key not utf-8")?;
            let relative = key
                .strip_prefix(&prefix)
                .ok_or_else(|| anyhow!("index key {key} missing agent prefix"))?;
            let ciphertext = match self.node.get_blob(hash).await {
                Ok(ct) => ct,
                // content may not have downloaded yet; skip, a later event retries.
                Err(_) => continue,
            };
            let plaintext = self.identity.decrypt(&ciphertext)?;
            let dest = self.adapter.session_root().join(relative);
            atomic_write(&dest, &plaintext).await?;
            written += 1;
        }
        Ok(written)
    }

    pub async fn conflict_paths(&self) -> Result<Vec<String>> {
        let prefix = format!("{}/", self.adapter.agent());
        let mut out = Vec::new();
        for key in self.node.conflicts().await? {
            let key = String::from_utf8(key).context("index key not utf-8")?;
            out.push(key.strip_prefix(&prefix).unwrap_or(&key).to_string());
        }
        Ok(out)
    }

    pub async fn status_report(&self) -> Result<StatusReport> {
        Ok(StatusReport {
            namespace: self.node.namespace().map(|n| n.to_string()),
            sessions: self.node.index_latest().await?.len(),
            conflicts: self.conflict_paths().await?,
        })
    }

    async fn write_status(&self, path: &Path) {
        if let Ok(report) = self.status_report().await {
            if let Ok(text) = toml::to_string_pretty(&report) {
                let _ = tokio::fs::write(path, text).await;
            }
            for c in &report.conflicts {
                eprintln!("ssync: conflict on {c} (both versions kept; newest wins)");
            }
        }
    }

    /// Import + export once, then watch the session dir (import on change) and the
    /// index (export on remote update), writing a status snapshot each pass.
    pub async fn run(&self, status_path: &Path) -> Result<()> {
        self.import_all().await?;
        self.export_all().await?;
        self.write_status(status_path).await;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        watcher
            .watch(self.adapter.session_root(), RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", self.adapter.session_root().display()))?;

        let events = self.node.subscribe().await?;
        let mut events = std::pin::pin!(events);

        // Debounce filesystem events: pi appends to the live session file, so a
        // single logical write emits many events. Collect changed paths and only
        // import once they have been quiet for `DEBOUNCE`, so we never read a
        // half-written file and don't import on every append.
        const DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(500);
        let mut pending: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut deadline: Option<tokio::time::Instant> = None;

        loop {
            let settle = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                Some(res) = rx.recv() => {
                    if let Ok(event) = res {
                        for path in event.paths {
                            if self.adapter.is_session_file(&path) {
                                pending.insert(path);
                            }
                        }
                        if !pending.is_empty() {
                            deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
                        }
                    }
                }
                _ = settle, if deadline.is_some() => {
                    deadline = None;
                    for path in pending.drain() {
                        if path.is_file() {
                            if let Err(e) = self.import_file(&path).await {
                                eprintln!("ssync: import {}: {e:#}", path.display());
                            }
                        }
                    }
                    self.write_status(status_path).await;
                }
                Some(event) = events.next() => {
                    if matches!(
                        event,
                        Ok(LiveEvent::InsertRemote { .. }) | Ok(LiveEvent::ContentReady { .. })
                    ) {
                        if let Err(e) = self.export_all().await {
                            eprintln!("ssync: export: {e:#}");
                        }
                        self.write_status(status_path).await;
                    }
                }
                else => break,
            }
        }
        Ok(())
    }
}

/// Recursively collect session files under `root` accepted by `adapter`.
fn session_files(root: &Path, adapter: &impl Adapter) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if adapter.is_session_file(&path) {
                out.push(path);
            }
        }
    }
    out
}

/// Write `data` to `dest` atomically: temp file in the same dir, then rename, so
/// a reader (the agent) never observes a partial file (DECISIONS §10).
async fn atomic_write(dest: &Path, data: &[u8]) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent dir", dest.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;
    let tmp: PathBuf = dest.with_extension("ssync-tmp");
    tokio::fs::write(&tmp, data)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    tokio::fs::rename(&tmp, dest)
        .await
        .with_context(|| format!("renaming into {}", dest.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssync_adapters::pi::PiAdapter;
    use ssync_net::iroh::SecretKey;

    #[tokio::test]
    async fn imported_session_round_trips_decrypted() {
        let base = std::env::temp_dir().join(format!("ssync-core-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sessions_root = base.join("sessions");
        let proj = sessions_root.join("--home-simon-Projects-demo--");
        std::fs::create_dir_all(&proj).unwrap();
        let session_path =
            proj.join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl");
        let contents = b"{\"type\":\"session\",\"version\":3}\n{\"secret\":\"sk-live-abc\"}\n";
        std::fs::write(&session_path, contents).unwrap();

        let node = Node::spawn(&base.join("data"), SecretKey::generate())
            .await
            .unwrap();
        let mut engine = Engine::new(
            PiAdapter::new(&sessions_root),
            AgeIdentity::generate().unwrap(),
            node,
        );
        engine.create_namespace().await.unwrap();

        let id = engine.import_file(&session_path).await.unwrap();
        assert_eq!(id.session_id, "019e539d-f6ab-71ac-be20-d3ae2b23ea4a");

        let got = engine.read_session(&id).await.unwrap();
        assert_eq!(got.as_deref(), Some(&contents[..]));
    }
}
