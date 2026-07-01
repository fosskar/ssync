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
    /// Shared namespace secret (same on every peer). When set, peers auto-join
    /// this one namespace with no ticket exchange (clan provides it).
    #[serde(default)]
    pub namespace_secret_path: Option<PathBuf>,
    /// Override the node key path (default: `data_dir/node.key`).
    #[serde(default)]
    pub node_key_path: Option<PathBuf>,
    /// Peer node-ids to sync with (clan fills this from the other machines).
    #[serde(default)]
    pub peers: Vec<String>,
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
            namespace_secret_path: None,
            node_key_path: None,
            peers: Vec::new(),
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

    /// Sessions still genuinely diverged — the newest version does not already
    /// contain every line of every author's version.
    pub async fn conflict_paths(&self) -> Result<Vec<String>> {
        Ok(self
            .divergent()
            .await?
            .into_iter()
            .map(|(rel, _)| rel)
            .collect())
    }

    pub async fn status_report(&self) -> Result<StatusReport> {
        Ok(StatusReport {
            namespace: self.node.namespace().map(|n| n.to_string()),
            sessions: self.node.index_latest().await?.len(),
            conflicts: self.conflict_paths().await?,
        })
    }

    async fn get_plain(&self, hash: ssync_net::iroh_blobs::Hash) -> Option<Vec<u8>> {
        let ciphertext = self.node.get_blob(hash).await.ok()?;
        self.identity.decrypt(&ciphertext).ok()
    }

    /// Sessions where several authors' versions exist and the newest does not
    /// contain the union of all lines. Since pi is append-only, the union is the
    /// correct lossless resolution. Returns `(relative_path, merged_plaintext)`.
    /// A stale duplicate (one version a subset of the newest) is not divergent.
    async fn divergent(&self) -> Result<Vec<(String, Vec<u8>)>> {
        use std::collections::{BTreeMap, HashSet};
        let prefix = format!("{}/", self.adapter.agent());
        let mut by_key: BTreeMap<Vec<u8>, Vec<(u64, ssync_net::iroh_blobs::Hash)>> =
            BTreeMap::new();
        for (key, ts, hash) in self.node.index_entries_full().await? {
            by_key.entry(key).or_default().push((ts, hash));
        }
        let mut out = Vec::new();
        for (key, mut versions) in by_key {
            let distinct: HashSet<String> = versions.iter().map(|(_, h)| h.to_string()).collect();
            if distinct.len() <= 1 {
                continue;
            }
            let key = String::from_utf8(key).context("index key not utf-8")?;
            let Some(rel) = key.strip_prefix(&prefix).map(str::to_string) else {
                continue;
            };
            versions.sort_by_key(|(ts, _)| *ts);
            let (_, winner) = *versions.last().unwrap();
            let Some(winner_pt) = self.get_plain(winner).await else {
                continue;
            };
            let mut plaintexts = Vec::new();
            let mut seen = HashSet::new();
            for (_, h) in &versions {
                if seen.insert(h.to_string()) {
                    if let Some(p) = self.get_plain(*h).await {
                        plaintexts.push(p);
                    }
                }
            }
            let merged = merge_lines(&plaintexts);
            if merged != winner_pt {
                out.push((rel, merged));
            }
        }
        Ok(out)
    }

    /// Publish the merged version of every genuinely-diverged session as the new
    /// winner (under this node's author), converging all peers. Logs each merge
    /// once. Returns whether anything was merged.
    async fn resolve_divergences(
        &self,
        logged: &mut std::collections::HashSet<String>,
    ) -> Result<bool> {
        let mut merged_any = false;
        for (rel, merged) in self.divergent().await? {
            let key = format!("{}/{}", self.adapter.agent(), rel);
            let ciphertext = self.identity.encrypt(&merged)?;
            let size = ciphertext.len() as u64;
            let hash = self.node.add_blob(ciphertext).await?;
            self.node.index_set(key.clone(), hash, size).await?;
            if logged.insert(key) {
                eprintln!("ssync: merged divergent session {rel} (lossless union, nothing lost)");
            }
            merged_any = true;
        }
        Ok(merged_any)
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

    /// Run the daemon: an initial import, then two concurrent debounced loops —
    /// watch the session dir (import only changed files, with a periodic rescan
    /// fallback) and react to index changes (export only changed sessions). The
    /// loops share one task via `try_join!`, so a slow export never starves
    /// imports, and both are incremental so the idle cost is ~zero.
    pub async fn run(&self, status_path: &Path) -> Result<()> {
        self.import_all().await?;
        self.write_status(status_path).await;
        // Paths the exporter removed (peer deletion), so the import loop doesn't
        // mistake them for a local deletion and echo it back.
        let exporter_deleted: Deleted = Default::default();
        tokio::try_join!(
            self.watch_import_loop(status_path, &exporter_deleted),
            self.export_loop(status_path, &exporter_deleted),
        )?;
        Ok(())
    }

    async fn watch_import_loop(
        &self,
        status_path: &Path,
        exporter_deleted: &Deleted,
    ) -> Result<()> {
        use std::collections::{HashMap, HashSet};
        use std::time::{Duration, SystemTime};

        // Files import_all already handled — don't re-read them.
        let mut seen: HashMap<PathBuf, (SystemTime, u64)> = HashMap::new();
        for path in session_files(self.adapter.session_root(), &self.adapter) {
            if let Some(stamp) = file_stamp(&path) {
                seen.insert(path, stamp);
            }
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        watcher
            .watch(self.adapter.session_root(), RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", self.adapter.session_root().display()))?;

        // Rescan periodically as a robust fallback for any missed fs event.
        let mut rescan = tokio::time::interval(Duration::from_secs(15));
        rescan.tick().await;

        // Debounce: pi appends to the live file, emitting many events per write.
        const DEBOUNCE: Duration = Duration::from_millis(500);
        let mut pending: HashSet<PathBuf> = HashSet::new();
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
                    let mut changed = false;
                    for path in std::mem::take(&mut pending) {
                        changed |= self.import_if_changed(&path, &mut seen).await;
                    }
                    if changed {
                        self.write_status(status_path).await;
                    }
                }
                _ = rescan.tick() => {
                    let files = session_files(self.adapter.session_root(), &self.adapter);
                    let mut changed = false;
                    for path in &files {
                        changed |= self.import_if_changed(path, &mut seen).await;
                    }
                    // Propagate user deletions. Guard: never delete everything at
                    // once (a transiently empty/unmounted dir must not wipe peers).
                    if !files.is_empty() {
                        let present: HashSet<PathBuf> = files.into_iter().collect();
                        let gone: Vec<PathBuf> =
                            seen.keys().filter(|p| !present.contains(*p)).cloned().collect();
                        for path in gone {
                            seen.remove(&path);
                            if exporter_deleted.lock().unwrap().remove(&path) {
                                continue; // our own exporter removed it
                            }
                            if let Ok(id) = self.adapter.identify(&path) {
                                match self.node.index_delete(self.index_key(&id)).await {
                                    Ok(()) => changed = true,
                                    Err(e) => {
                                        eprintln!("ssync: delete {}: {e:#}", path.display())
                                    }
                                }
                            }
                        }
                    }
                    if changed {
                        self.write_status(status_path).await;
                    }
                }
            }
        }
    }

    /// Import `path` only if its mtime/size changed since last seen (so a rescan
    /// or a bounced write-back doesn't re-decrypt). Returns whether it imported.
    async fn import_if_changed(
        &self,
        path: &Path,
        seen: &mut std::collections::HashMap<PathBuf, (std::time::SystemTime, u64)>,
    ) -> bool {
        let Some(stamp) = file_stamp(path) else {
            return false;
        };
        if seen.get(path) == Some(&stamp) {
            return false;
        }
        match self.import_file(path).await {
            Ok(_) => {
                seen.insert(path.to_path_buf(), stamp);
                true
            }
            Err(e) => {
                eprintln!("ssync: import {}: {e:#}", path.display());
                false
            }
        }
    }

    async fn export_loop(&self, status_path: &Path, exporter_deleted: &Deleted) -> Result<()> {
        use std::collections::{HashMap, HashSet};
        use std::time::Duration;

        let mut exported: HashMap<String, String> = HashMap::new();
        let mut merged_logged: HashSet<String> = HashSet::new();
        let merged = self
            .resolve_divergences(&mut merged_logged)
            .await
            .unwrap_or(false);
        if self.export_changed(&mut exported, exporter_deleted).await? || merged {
            self.write_status(status_path).await;
        }

        let events = self.node.subscribe().await?;
        let mut events = std::pin::pin!(events);

        const DEBOUNCE: Duration = Duration::from_millis(300);
        let mut deadline: Option<tokio::time::Instant> = None;

        loop {
            let settle = async {
                match deadline {
                    Some(d) => tokio::time::sleep_until(d).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                event = events.next() => {
                    match event {
                        Some(Ok(LiveEvent::InsertRemote { .. }))
                        | Some(Ok(LiveEvent::ContentReady { .. })) => {
                            deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
                        }
                        Some(_) => {}
                        None => break,
                    }
                }
                _ = settle, if deadline.is_some() => {
                    deadline = None;
                    let merged = self.resolve_divergences(&mut merged_logged).await.unwrap_or(false);
                    if self.export_changed(&mut exported, exporter_deleted).await? || merged {
                        self.write_status(status_path).await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Export only sessions whose winning blob hash differs from what was last
    /// written, decrypting just those. Returns whether anything was written.
    async fn export_changed(
        &self,
        exported: &mut std::collections::HashMap<String, String>,
        exporter_deleted: &Deleted,
    ) -> Result<bool> {
        let prefix = format!("{}/", self.adapter.agent());
        let mut wrote = false;
        let mut current: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (key, hash) in self.node.index_latest().await? {
            let key = String::from_utf8(key).context("index key not utf-8")?;
            current.insert(key.clone());
            let hash_str = hash.to_string();
            if exported.get(&key) == Some(&hash_str) {
                continue;
            }
            let Some(relative) = key.strip_prefix(&prefix).map(str::to_string) else {
                continue;
            };
            let ciphertext = match self.node.get_blob(hash).await {
                Ok(ct) => ct,
                // content not downloaded yet; retry on a later event.
                Err(_) => continue,
            };
            let plaintext = self.identity.decrypt(&ciphertext)?;
            let dest = self.adapter.session_root().join(&relative);
            atomic_write(&dest, &plaintext).await?;
            exported.insert(key, hash_str);
            wrote = true;
        }
        // Sessions removed from the index (deleted on a peer) -> remove locally.
        let removed: Vec<String> = exported
            .keys()
            .filter(|k| !current.contains(*k))
            .cloned()
            .collect();
        for key in removed {
            exported.remove(&key);
            if let Some(relative) = key.strip_prefix(&prefix) {
                let dest = self.adapter.session_root().join(relative);
                exporter_deleted.lock().unwrap().insert(dest.clone());
                let _ = tokio::fs::remove_file(&dest).await;
                wrote = true;
            }
        }
        Ok(wrote)
    }
}

/// Shared set of paths the exporter removed, so the importer won't re-delete.
type Deleted = std::sync::Arc<std::sync::Mutex<std::collections::HashSet<PathBuf>>>;

/// Merge append-only session versions by unioning their lines: longest version
/// first (so a superset stays intact), then any unique lines from the others.
/// Lossless — safe for pi, whose sessions only ever grow (compaction is an
/// appended marker, never a rewrite; see docs/pi-format-notes.md).
fn merge_lines(versions: &[Vec<u8>]) -> Vec<u8> {
    let mut order: Vec<&Vec<u8>> = versions.iter().collect();
    order.sort_by_key(|b| std::cmp::Reverse(b.len()));
    let mut seen: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    let mut lines: Vec<&[u8]> = Vec::new();
    for v in order {
        for line in v.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if seen.insert(line) {
                lines.push(line);
            }
        }
    }
    let mut out = Vec::new();
    for line in &lines {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

/// Metadata stamp (mtime, len) used to detect whether a file changed.
fn file_stamp(path: &Path) -> Option<(std::time::SystemTime, u64)> {
    let m = std::fs::metadata(path).ok()?;
    if !m.is_file() {
        return None;
    }
    Some((m.modified().ok()?, m.len()))
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
    use std::path::Path;

    #[test]
    fn config_round_trips_with_peers() {
        // mirrors the daemon config the nix modules render, including the
        // shared-namespace fields and a multi-element peers array.
        let toml_str = r#"
            agent = "pi"
            session_dir = "/home/x/.pi/agent/sessions"
            age_identity_path = "/run/secrets/age/key"
            data_dir = "/var/lib/ssync"
            namespace_secret_path = "/run/secrets/ns/secret"
            node_key_path = "/run/secrets/node/key"
            peers = [ "aaa", "bbb" ]
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.peers, vec!["aaa".to_string(), "bbb".to_string()]);
        assert_eq!(
            cfg.namespace_secret_path.as_deref(),
            Some(Path::new("/run/secrets/ns/secret"))
        );
        assert_eq!(
            cfg.node_key_path.as_deref(),
            Some(Path::new("/run/secrets/node/key"))
        );
        // render back out and reparse: the fields survive a full round-trip.
        let rendered = toml::to_string(&cfg).unwrap();
        let cfg2: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(cfg.peers, cfg2.peers);
        assert_eq!(cfg.namespace_secret_path, cfg2.namespace_secret_path);
    }

    #[test]
    fn config_defaults_when_shared_fields_absent() {
        // a pre-shared-namespace config (ticket flow) still parses.
        let toml_str = r#"
            agent = "pi"
            session_dir = "/s"
            age_identity_path = "/a"
            data_dir = "/d"
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(cfg.peers.is_empty());
        assert_eq!(cfg.namespace_secret_path, None);
        assert_eq!(cfg.node_key_path, None);
    }

    #[test]
    fn merge_superset_is_the_superset() {
        let short = b"h\na\nb\n".to_vec();
        let long = b"h\na\nb\nc\n".to_vec();
        assert_eq!(merge_lines(&[short, long.clone()]), long);
    }

    #[test]
    fn merge_fork_unions_all_lines_losslessly() {
        let a = b"h\na1\na2\n".to_vec();
        let b = b"h\na1\nb2\n".to_vec();
        let m = merge_lines(&[a, b]);
        let s = String::from_utf8(m).unwrap();
        for line in ["h", "a1", "a2", "b2"] {
            assert!(s.lines().any(|l| l == line), "missing {line} in {s:?}");
        }
        assert_eq!(
            s.lines().filter(|l| *l == "h").count(),
            1,
            "header duplicated"
        );
    }

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
