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

use ssync_net::iroh_blobs::Hash;

mod reconcile;
use reconcile::{reconcile, Action, IndexEntry, IndexHead, LocalFile, SyncState};

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
    /// Divergence verdict per key, keyed by the distinct-hash fingerprint of
    /// its author entries. Skips re-decrypting sessions whose version set has
    /// not changed — without it every status write decrypts every session that
    /// still carries a stale second author entry.
    divergence_cache: std::sync::Mutex<std::collections::HashMap<String, (String, bool)>>,
}

impl<A: Adapter> Engine<A> {
    pub fn new(adapter: A, identity: AgeIdentity, node: Node) -> Self {
        Self {
            adapter,
            identity,
            node,
            divergence_cache: Default::default(),
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
        // Resurrection guard (for direct/one-shot callers; the daemon decides
        // this in `reconcile`): a peer deleted this session after the file was
        // last written (tombstone newer than mtime) — don't import it back.
        if let Some((ts, None)) = self.node.index_head(self.index_key(&id)).await? {
            if file_mtime_micros(path).is_some_and(|mtime| ts >= mtime) {
                return Ok(id);
            }
        }
        self.import_action(path).await?;
        Ok(id)
    }

    /// Read → dedup on plaintext → age-encrypt → blob → upsert index. Returns
    /// the new blob hash, or `None` when the file already matched the indexed
    /// version (loop-prevention: the exporter's own write-back is randomized
    /// ciphertext, so dedup on plaintext or it bounces back as a false change).
    async fn import_action(&self, path: &Path) -> Result<Option<Hash>> {
        let id = self.adapter.identify(path)?;
        let plaintext = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading session file {}", path.display()))?;
        if let Ok(Some(existing)) = self.read_session(&id).await {
            if existing == plaintext {
                return Ok(None);
            }
        }
        let ciphertext = self.identity.encrypt(&plaintext)?;
        let size = ciphertext.len() as u64;
        let hash = self.node.add_blob(ciphertext).await?;
        self.node.index_set(self.index_key(&id), hash, size).await?;
        Ok(Some(hash))
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
            let Some(hash) = hash else {
                continue; // deleted
            };
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
        let live = self
            .node
            .index_latest()
            .await?
            .into_iter()
            .filter(|(_, h)| h.is_some())
            .count();
        Ok(StatusReport {
            namespace: self.node.namespace().map(|n| n.to_string()),
            sessions: live,
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
        let mut by_key: BTreeMap<Vec<u8>, Vec<(u64, Option<ssync_net::iroh_blobs::Hash>)>> =
            BTreeMap::new();
        for (key, ts, hash) in self.node.index_entries_full().await? {
            by_key.entry(key).or_default().push((ts, hash));
        }
        let mut out = Vec::new();
        for (key, mut versions) in by_key {
            versions.sort_by_key(|(ts, _)| *ts);
            // a winning tombstone means the session is deleted — never merge it
            // back to life from stale live entries.
            let Some((_, Some(winner))) = versions.last().copied() else {
                continue;
            };
            let distinct: HashSet<String> = versions
                .iter()
                .filter_map(|(_, h)| h.map(|h| h.to_string()))
                .collect();
            if distinct.len() <= 1 {
                continue;
            }
            let key = String::from_utf8(key).context("index key not utf-8")?;
            let Some(rel) = key.strip_prefix(&prefix).map(str::to_string) else {
                continue;
            };
            // fingerprint the version set; an unchanged set needs no re-decrypt.
            let mut fingerprint: Vec<String> = distinct.iter().cloned().collect();
            fingerprint.sort();
            let fingerprint = fingerprint.join(",");
            let cached = self
                .divergence_cache
                .lock()
                .unwrap()
                .get(&key)
                .filter(|(fp, _)| *fp == fingerprint)
                .map(|(_, verdict)| *verdict);
            if cached == Some(false) {
                continue;
            }
            let Some(winner_pt) = self.get_plain(winner).await else {
                continue;
            };
            let mut plaintexts = Vec::new();
            let mut seen = HashSet::new();
            for h in versions.iter().filter_map(|(_, h)| *h) {
                if seen.insert(h.to_string()) {
                    if let Some(p) = self.get_plain(h).await {
                        plaintexts.push(p);
                    }
                }
            }
            let merged = merge_lines(&plaintexts);
            let divergent = merged != winner_pt;
            // only cache complete verdicts: with a blob still undownloaded the
            // union is partial and would pin a wrong "not divergent" forever.
            if plaintexts.len() == distinct.len() {
                self.divergence_cache
                    .lock()
                    .unwrap()
                    .insert(key, (fingerprint, divergent));
            }
            if divergent {
                out.push((rel, merged));
            }
        }
        Ok(out)
    }

    /// Recompute the lossless union for one diverged key and publish it as the
    /// new winner (under this node's author) only if it differs from the current
    /// winner. Returns the relative path when it published, so the caller logs
    /// the merge once.
    async fn merge_one(&self, key: &str) -> Result<Option<String>> {
        use std::collections::HashSet;
        let Some(rel) = key.strip_prefix(&self.prefix()).map(str::to_string) else {
            return Ok(None);
        };
        let mut versions: Vec<(u64, Option<Hash>)> = self
            .node
            .index_entries_full()
            .await?
            .into_iter()
            .filter(|(k, _, _)| k.as_slice() == key.as_bytes())
            .map(|(_, ts, h)| (ts, h))
            .collect();
        versions.sort_by_key(|(ts, _)| *ts);
        // a winning tombstone means the session is deleted — never merge it back.
        let Some((_, Some(winner))) = versions.last().copied() else {
            return Ok(None);
        };
        let Some(winner_pt) = self.get_plain(winner).await else {
            return Ok(None);
        };
        let mut seen = HashSet::new();
        let mut plaintexts = Vec::new();
        for h in versions.iter().filter_map(|(_, h)| *h) {
            if seen.insert(h.to_string()) {
                if let Some(p) = self.get_plain(h).await {
                    plaintexts.push(p);
                }
            }
        }
        let merged = merge_lines(&plaintexts);
        if merged == winner_pt {
            return Ok(None);
        }
        let ciphertext = self.identity.encrypt(&merged)?;
        let size = ciphertext.len() as u64;
        let hash = self.node.add_blob(ciphertext).await?;
        self.node.index_set(key.to_string(), hash, size).await?;
        Ok(Some(rel))
    }

    /// The `{agent}/` index-key prefix.
    fn prefix(&self) -> String {
        format!("{}/", self.adapter.agent())
    }

    /// Write the status snapshot; `announce` additionally logs conflicts (off
    /// for the periodic liveness refresh, which would repeat them every tick).
    async fn write_status(&self, path: &Path, announce: bool) {
        if let Ok(report) = self.status_report().await {
            if let Ok(text) = toml::to_string_pretty(&report) {
                let _ = tokio::fs::write(path, text).await;
            }
            if announce {
                for c in &report.conflicts {
                    eprintln!("ssync: conflict on {c} (both versions kept; newest wins)");
                }
            }
        }
    }

    /// Snapshot the session dir as `reconcile` input.
    fn local_snapshot(&self) -> Vec<LocalFile> {
        let mut out = Vec::new();
        for path in session_files(self.adapter.session_root(), &self.adapter) {
            let Some(stamp) = file_stamp_micros(&path) else {
                continue;
            };
            let Ok(id) = self.adapter.identify(&path) else {
                continue;
            };
            out.push(LocalFile {
                key: self.index_key(&id),
                path,
                stamp,
            });
        }
        out
    }

    /// Snapshot the synced index as `reconcile` input: winning entry plus the
    /// count of distinct live hashes (divergence) per key.
    async fn index_snapshot(&self) -> Result<std::collections::HashMap<String, IndexEntry>> {
        use std::collections::{HashMap, HashSet};
        let mut ts_by_key: HashMap<String, u64> = HashMap::new();
        let mut distinct: HashMap<String, HashSet<String>> = HashMap::new();
        for (key, ts, hash) in self.node.index_entries_full().await? {
            let key = String::from_utf8(key).context("index key not utf-8")?;
            ts_by_key
                .entry(key.clone())
                .and_modify(|t| *t = (*t).max(ts))
                .or_insert(ts);
            if let Some(h) = hash {
                distinct.entry(key).or_default().insert(h.to_string());
            }
        }
        let mut out = HashMap::new();
        for (key, hash) in self.node.index_latest().await? {
            let key = String::from_utf8(key).context("index key not utf-8")?;
            let timestamp = ts_by_key.get(&key).copied().unwrap_or(0);
            let distinct_live = distinct.get(&key).map(HashSet::len).unwrap_or(0);
            out.insert(
                key,
                IndexEntry {
                    head: IndexHead { timestamp, hash },
                    distinct_live,
                },
            );
        }
        Ok(out)
    }

    /// One reconcile pass: snapshot both sides, decide, execute, refresh status.
    async fn tick(
        &self,
        state: &mut SyncState,
        merged_logged: &mut std::collections::HashSet<String>,
        status_path: &Path,
    ) {
        let local = self.local_snapshot();
        let index = match self.index_snapshot().await {
            Ok(i) => i,
            Err(e) => {
                eprintln!("ssync: index snapshot: {e:#}");
                return;
            }
        };
        let mut changed = false;
        for action in reconcile(state, &local, &index) {
            changed |= self.execute(&action, state, merged_logged).await;
        }
        // Refresh unconditionally: the snapshot's mtime doubles as the daemon's
        // liveness signal for `ssync status`. Announce conflicts only on change.
        self.write_status(status_path, changed).await;
    }

    /// Perform one action and record what we did back into `state` (so a
    /// self-write never echoes on the next pass). Returns whether it changed
    /// anything, which drives conflict announcement.
    async fn execute(
        &self,
        action: &Action,
        state: &mut SyncState,
        merged_logged: &mut std::collections::HashSet<String>,
    ) -> bool {
        match action {
            Action::Import {
                key,
                path,
                stamp,
                winner,
            } => match self.import_action(path).await {
                Ok(new_hash) => {
                    let ks = state.keys.entry(key.clone()).or_default();
                    ks.import_stamp = Some(*stamp);
                    // settled content is the freshly written blob, else the
                    // winner we deduped against.
                    ks.export_hash = Some(new_hash.or(*winner));
                    new_hash.is_some()
                }
                Err(e) => {
                    eprintln!("ssync: import {}: {e:#}", path.display());
                    false
                }
            },
            Action::WriteFile { key, hash } => {
                let Some(relative) = key.strip_prefix(&self.prefix()).map(str::to_string) else {
                    return false;
                };
                // content may not have downloaded yet; skip, a later tick retries.
                let Ok(ciphertext) = self.node.get_blob(*hash).await else {
                    return false;
                };
                let plaintext = match self.identity.decrypt(&ciphertext) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("ssync: decrypt {key}: {e:#}");
                        return false;
                    }
                };
                let dest = self.adapter.session_root().join(&relative);
                if let Err(e) = atomic_write(&dest, &plaintext).await {
                    eprintln!("ssync: write {}: {e:#}", dest.display());
                    return false;
                }
                let ks = state.keys.entry(key.clone()).or_default();
                ks.export_hash = Some(Some(*hash));
                ks.import_stamp = file_stamp_micros(&dest);
                true
            }
            Action::DeleteLocal { key } => {
                let ks = state.keys.entry(key.clone()).or_default();
                ks.export_hash = Some(None);
                ks.import_stamp = None;
                let Some(relative) = key.strip_prefix(&self.prefix()) else {
                    return false;
                };
                let dest = self.adapter.session_root().join(relative);
                let existed = dest.exists();
                let _ = tokio::fs::remove_file(&dest).await;
                existed
            }
            Action::Tombstone { key } => match self.node.index_delete(key).await {
                Ok(()) => {
                    let ks = state.keys.entry(key.clone()).or_default();
                    ks.export_hash = Some(None);
                    ks.import_stamp = None;
                    true
                }
                Err(e) => {
                    eprintln!("ssync: delete {key}: {e:#}");
                    false
                }
            },
            Action::Merge { key } => match self.merge_one(key).await {
                Ok(Some(rel)) => {
                    if merged_logged.insert(key.clone()) {
                        eprintln!(
                            "ssync: merged divergent session {rel} (lossless union, nothing lost)"
                        );
                    }
                    true
                }
                Ok(None) => false,
                Err(e) => {
                    eprintln!("ssync: merge {key}: {e:#}");
                    false
                }
            },
        }
    }

    /// Run the daemon: one reconcile loop fed by three triggers — filesystem
    /// events, index events, and a periodic rescan (a robust fallback for any
    /// missed fs/index event, and the liveness heartbeat). Every trigger funnels
    /// into the same debounced [`tick`], so all decisions run through one pure
    /// [`reconcile`] over a single owned [`SyncState`] — no cross-task shared
    /// state, no `seen`/`exported`/`Deleted` bookkeeping.
    pub async fn run(&self, status_path: &Path) -> Result<()> {
        use std::time::Duration;

        let mut state = SyncState::default();
        let mut merged_logged: std::collections::HashSet<String> = Default::default();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        watcher
            .watch(self.adapter.session_root(), RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", self.adapter.session_root().display()))?;

        let events = self.node.subscribe().await?;
        let mut events = std::pin::pin!(events);

        // initial reconcile (replaces the old up-front import_all)
        self.tick(&mut state, &mut merged_logged, status_path).await;

        let mut rescan = tokio::time::interval(Duration::from_secs(15));
        rescan.tick().await;

        // Debounce: pi appends to the live file, emitting many events per write.
        const DEBOUNCE: Duration = Duration::from_millis(400);
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
                        if event.paths.iter().any(|p| self.adapter.is_session_file(p)) {
                            deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
                        }
                    }
                }
                ev = events.next() => {
                    match ev {
                        Some(Ok(LiveEvent::InsertRemote { .. }))
                        | Some(Ok(LiveEvent::ContentReady { .. })) => {
                            deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
                        }
                        Some(_) => {}
                        None => {} // index stream ended; rescan still drives ticks
                    }
                }
                _ = settle, if deadline.is_some() => {
                    deadline = None;
                    self.tick(&mut state, &mut merged_logged, status_path).await;
                }
                _ = rescan.tick() => {
                    self.tick(&mut state, &mut merged_logged, status_path).await;
                }
            }
        }
    }
}

/// Merge append-only session versions: the common prefix (shared chronological
/// history, duplicates intact) followed by each fork's remaining lines, deduped
/// only across versions (a line both forks appended stays single; a duplicate
/// within one version survives). Version order is content-derived, so every
/// peer computes the identical merge. Lossless — safe for pi, whose sessions
/// only ever grow (compaction is an appended marker, never a rewrite; see
/// docs/pi-format-notes.md).
fn merge_lines(versions: &[Vec<u8>]) -> Vec<u8> {
    let mut split: Vec<Vec<&[u8]>> = versions
        .iter()
        .map(|v| v.split(|&b| b == b'\n').filter(|l| !l.is_empty()).collect())
        .collect();
    split.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));

    let mut prefix = split.first().map_or(0, |v| v.len());
    for v in &split[1..] {
        prefix = (0..prefix.min(v.len()))
            .take_while(|&i| v[i] == split[0][i])
            .count();
    }

    let mut lines: Vec<&[u8]> = split.first().map_or(Vec::new(), |v| v[..prefix].to_vec());
    let mut emitted: std::collections::HashSet<&[u8]> = std::collections::HashSet::new();
    for v in &split {
        let suffix = &v[prefix..];
        lines.extend(suffix.iter().filter(|l| !emitted.contains(*l)));
        emitted.extend(suffix.iter().copied());
    }

    let mut out = Vec::new();
    for line in &lines {
        out.extend_from_slice(line);
        out.push(b'\n');
    }
    out
}

/// File mtime as microseconds since the epoch (iroh-docs timestamp scale).
fn file_mtime_micros(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_micros() as u64)
}

/// Metadata stamp `(mtime_micros, len)` used to detect whether a file changed;
/// mtime is on the iroh-docs microsecond scale so it compares against index
/// timestamps directly (used for the resurrection guard in `reconcile`).
fn file_stamp_micros(path: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(path).ok()?;
    if !m.is_file() {
        return None;
    }
    let mtime = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_micros() as u64;
    Some((mtime, m.len()))
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
    fn merge_preserves_duplicate_lines_within_a_version() {
        // two identical entries (same bytes) in one version must both survive.
        let a = b"h\nx\nx\na1\n".to_vec();
        let b = b"h\nx\nx\nb1\n".to_vec();
        let m = String::from_utf8(merge_lines(&[a, b])).unwrap();
        assert_eq!(
            m.lines().filter(|l| *l == "x").count(),
            2,
            "duplicate entry collapsed: {m:?}"
        );
    }

    #[test]
    fn merge_is_deterministic_regardless_of_input_order() {
        let a = b"h\na1\n".to_vec();
        let b = b"h\nb1\n".to_vec();
        assert_eq!(
            merge_lines(&[a.clone(), b.clone()]),
            merge_lines(&[b, a]),
            "peers feeding versions in different order must converge"
        );
    }

    #[test]
    fn merge_keeps_common_history_in_order() {
        // shared prefix must stay chronological, fork suffixes appended after.
        let a = b"h\nc1\nc2\na1\n".to_vec();
        let b = b"h\nc1\nc2\nb1\nb2\n".to_vec();
        let m = String::from_utf8(merge_lines(&[a, b])).unwrap();
        let lines: Vec<&str> = m.lines().collect();
        assert_eq!(&lines[..3], &["h", "c1", "c2"], "prefix reordered: {m:?}");
        assert_eq!(lines.len(), 6);
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
