//! config, sync engine, index model, conflict logic. Session bytes are stored
//! opaque: read → age-encrypt → blob; the index maps identity → hash. All
//! mutation flows through one path: snapshot → [`reconcile`] → execute.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use futures_lite::StreamExt;
use notify::{RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use ssync_adapters::{Adapter, SessionIdentity};
use ssync_crypto::AgeIdentity;
use ssync_net::Node;
use ssync_net::iroh_docs::engine::LiveEvent;

use ssync_net::iroh_blobs::Hash;

mod reconcile;
use reconcile::{Action, IndexEntry, IndexHead, LocalFile, SyncState, reconcile};

/// One agent to sync: its name and the session directory to watch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Agent name (`"pi"` or `"omp"`; see `ssync_adapters::adapter_for`).
    pub agent: String,
    pub session_dir: PathBuf,
}

/// On-disk daemon configuration (`$XDG_CONFIG_HOME/ssync/config.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Agents to sync side by side (`[[agents]]` tables).
    pub agents: Vec<AgentConfig>,
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

    /// Built-in defaults: every known agent whose session dir exists on this
    /// machine (pi at `~/.pi/agent/sessions`, omp at `~/.omp/agent/sessions`),
    /// falling back to pi alone on a fresh machine.
    pub fn defaults() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
        let config = dirs::config_dir().ok_or_else(|| anyhow!("no config dir"))?;
        let data = dirs::data_dir().ok_or_else(|| anyhow!("no data dir"))?;
        let known = [
            ("pi", home.join(".pi/agent/sessions")),
            ("omp", home.join(".omp/agent/sessions")),
        ];
        let mut agents: Vec<AgentConfig> = known
            .iter()
            .filter(|(_, dir)| dir.is_dir())
            .map(|(agent, dir)| AgentConfig {
                agent: agent.to_string(),
                session_dir: dir.clone(),
            })
            .collect();
        if agents.is_empty() {
            agents.push(AgentConfig {
                agent: "pi".to_string(),
                session_dir: home.join(".pi/agent/sessions"),
            });
        }
        Ok(Self {
            agents,
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
        Self::parse(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    /// Parse config TOML, expanding a leading `~/` in every path so one config
    /// file works across machines with different home directories.
    pub fn parse(text: &str) -> Result<Self> {
        let mut cfg: Self = toml::from_str(text)?;
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
        let expand = |p: &mut PathBuf| {
            if let Ok(rest) = p.strip_prefix("~") {
                *p = home.join(rest);
            }
        };
        for a in &mut cfg.agents {
            expand(&mut a.session_dir);
        }
        expand(&mut cfg.age_identity_path);
        expand(&mut cfg.data_dir);
        if let Some(p) = &mut cfg.namespace_secret_path {
            expand(p);
        }
        if let Some(p) = &mut cfg.node_key_path {
            expand(p);
        }
        Ok(cfg)
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

/// The sync engine for one node: one or more agent adapters (pi, omp, ...)
/// sharing a single index namespace, partitioned by the `{agent}/` key prefix.
pub struct Engine {
    adapters: Vec<Box<dyn Adapter>>,
    identity: AgeIdentity,
    node: Node,
    /// What we last materialised per key; feeds [`reconcile`].
    state: SyncState,
    /// Merges already announced (log once, not per tick).
    merged_logged: std::collections::HashSet<String>,
    /// Divergence verdict per key, keyed by the distinct-hash fingerprint of
    /// its author entries. Skips re-decrypting sessions whose version set has
    /// not changed — without it every status write decrypts every session that
    /// still carries a stale second author entry.
    divergence_cache: std::sync::Mutex<std::collections::HashMap<String, (String, bool)>>,
}

impl Engine {
    pub fn new(adapter: impl Adapter + 'static, identity: AgeIdentity, node: Node) -> Self {
        Self::with_adapters(vec![Box::new(adapter)], identity, node)
    }

    pub fn with_adapters(
        adapters: Vec<Box<dyn Adapter>>,
        identity: AgeIdentity,
        node: Node,
    ) -> Self {
        Self {
            adapters,
            identity,
            node,
            state: SyncState::default(),
            merged_logged: Default::default(),
            divergence_cache: Default::default(),
        }
    }

    /// The adapter owning an index key (matching `{agent}/` prefix), if any —
    /// peers may sync agents this node does not have configured.
    fn adapter_of_key(&self, key: &str) -> Option<&dyn Adapter> {
        self.adapters.iter().map(|a| a.as_ref()).find(|a| {
            key.strip_prefix(a.agent())
                .is_some_and(|r| r.starts_with('/'))
        })
    }

    /// The adapter whose session root contains `path`.
    fn adapter_of_path(&self, path: &Path) -> Option<&dyn Adapter> {
        self.adapters
            .iter()
            .map(|a| a.as_ref())
            .find(|a| path.starts_with(a.session_root()))
    }

    /// The iroh-docs index key for a session: `{agent}/{relative_path}`. The
    /// relative path is machine-independent and carries the write-back location,
    /// so the exporter can reconstruct where the file belongs on any peer.
    fn index_key(&self, id: &SessionIdentity) -> String {
        format!("{}/{}", id.agent, id.relative_path.display())
    }

    /// Read → encrypt → blob → upsert index. Dedups on *plaintext* (age
    /// ciphertext is randomized), or the exporter's own write-back echoes.
    async fn import_action(
        &self,
        key: &str,
        path: &Path,
        winner: Option<Hash>,
    ) -> Result<ImportOutcome> {
        let plaintext = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading session file {}", path.display()))?;
        if let Some(w) = winner
            && self.get_plain(w).await.as_deref() == Some(&plaintext)
        {
            return Ok(ImportOutcome::Unchanged(w));
        }
        let ciphertext = self.identity.encrypt(&plaintext)?;
        let size = ciphertext.len() as u64;
        let hash = self.node.add_blob(ciphertext).await?;
        self.node.index_set(key.to_string(), hash, size).await?;
        Ok(ImportOutcome::Published(hash))
    }

    /// Identify a path via the adapter whose session root contains it.
    fn identify(&self, path: &Path) -> Result<SessionIdentity> {
        self.adapter_of_path(path)
            .ok_or_else(|| anyhow!("{} is under no configured session root", path.display()))?
            .identify(path)
    }

    /// Sessions still genuinely diverged — the newest version does not already
    /// contain every line of every author's version. Verdicts are cached by the
    /// version-set fingerprint so an unchanged set is never re-decrypted.
    async fn conflict_paths(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for rec in self.node.index_records().await? {
            let Some(winner) = rec.winner else {
                continue; // deleted — never resurrect from stale live entries
            };
            if rec.versions.len() <= 1 {
                continue;
            }
            let key = String::from_utf8(rec.key).context("index key not utf-8")?;
            let Some(rel) = self.relative_of(&key).map(str::to_string) else {
                continue;
            };
            if self.is_diverged(&key, winner, &rec.versions).await == Some(true) {
                out.push(rel);
            }
        }
        Ok(out)
    }

    /// The one divergence verdict: does the union of `versions` differ from
    /// `winner`? `None` (uncached) while any blob is missing — a partial union
    /// would be transiently lossy. Cached by version-set fingerprint.
    async fn is_diverged(&self, key: &str, winner: Hash, versions: &[Hash]) -> Option<bool> {
        let mut fingerprint: Vec<String> = versions.iter().map(|h| h.to_string()).collect();
        fingerprint.sort();
        let fingerprint = fingerprint.join(",");
        if let Some((fp, verdict)) = self.divergence_cache.lock().unwrap().get(key)
            && *fp == fingerprint
        {
            return Some(*verdict);
        }
        let winner_pt = self.get_plain(winner).await?;
        let plaintexts = self.all_plaintexts(versions).await?;
        let divergent = merge_lines(&plaintexts) != winner_pt;
        self.divergence_cache
            .lock()
            .unwrap()
            .insert(key.to_string(), (fingerprint, divergent));
        Some(divergent)
    }

    /// Decrypt every version blob, or `None` while any is still downloading.
    async fn all_plaintexts(&self, versions: &[Hash]) -> Option<Vec<Vec<u8>>> {
        let mut plaintexts = Vec::with_capacity(versions.len());
        for h in versions {
            plaintexts.push(self.get_plain(*h).await?);
        }
        Some(plaintexts)
    }

    pub async fn status_report(&self) -> Result<StatusReport> {
        let live = self
            .node
            .index_records()
            .await?
            .into_iter()
            .filter(|r| r.winner.is_some())
            .count();
        Ok(StatusReport {
            namespace: self.node.namespace().map(|n| n.to_string()),
            sessions: live,
            conflicts: self.conflict_paths().await?,
        })
    }

    async fn get_plain(&self, hash: Hash) -> Option<Vec<u8>> {
        let ciphertext = self.node.get_blob(hash).await.ok()?;
        self.identity.decrypt(&ciphertext).ok()
    }

    /// Publish the lossless union for a diverged key; [`is_diverged`]
    /// (Self::is_diverged) gates it, so a settled key costs one cache lookup.
    /// Returns the relative path when it published.
    async fn merge_one(&self, key: &str) -> Result<Option<String>> {
        let Some(rel) = self.relative_of(key).map(str::to_string) else {
            return Ok(None);
        };
        let Some(rec) = self.node.index_record(key).await? else {
            return Ok(None);
        };
        // a winning tombstone means the session is deleted — never merge it back.
        let Some(winner) = rec.winner else {
            return Ok(None);
        };
        if rec.versions.len() <= 1 {
            return Ok(None);
        }
        if self.is_diverged(key, winner, &rec.versions).await != Some(true) {
            return Ok(None);
        }
        let Some(plaintexts) = self.all_plaintexts(&rec.versions).await else {
            return Ok(None);
        };
        let merged = merge_lines(&plaintexts);
        let ciphertext = self.identity.encrypt(&merged)?;
        let size = ciphertext.len() as u64;
        let hash = self.node.add_blob(ciphertext).await?;
        self.node.index_set(key.to_string(), hash, size).await?;
        Ok(Some(rel))
    }

    /// Decode an index key back to its session-root-relative path (the inverse of
    /// `index_key`): strip the `{agent}/` prefix of a configured adapter.
    fn relative_of<'a>(&self, key: &'a str) -> Option<&'a str> {
        let adapter = self.adapter_of_key(key)?;
        key.strip_prefix(adapter.agent())?.strip_prefix('/')
    }

    /// The absolute session-dir path an index key maps to on this machine, or
    /// `None` when no configured adapter owns the key.
    fn dest_of(&self, key: &str) -> Option<PathBuf> {
        let adapter = self.adapter_of_key(key)?;
        let rel = key.strip_prefix(adapter.agent())?.strip_prefix('/')?;
        Some(adapter.session_root().join(rel))
    }

    /// Session files under every configured adapter's root.
    fn all_session_files(&self) -> Vec<PathBuf> {
        self.adapters
            .iter()
            .flat_map(|a| session_files(a.session_root(), a.as_ref()))
            .collect()
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

    /// Snapshot every configured session dir as `reconcile` input.
    fn local_snapshot(&self) -> Vec<LocalFile> {
        let mut out = Vec::new();
        for path in self.all_session_files() {
            let Some(stamp) = file_stamp_micros(&path) else {
                continue;
            };
            let Ok(id) = self.identify(&path) else {
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
        let mut out = std::collections::HashMap::new();
        for rec in self.node.index_records().await? {
            let key = String::from_utf8(rec.key).context("index key not utf-8")?;
            let merge_allowed = self.adapter_of_key(&key).is_some_and(|a| a.append_only());
            out.insert(
                key,
                IndexEntry {
                    head: IndexHead {
                        timestamp: rec.winner_ts,
                        hash: rec.winner,
                    },
                    distinct_live: rec.versions.len(),
                    merge_allowed,
                },
            );
        }
        Ok(out)
    }

    /// One pass: snapshot both sides, [`reconcile`], execute. The daemon loop
    /// and tests drive this same path. Returns whether anything changed.
    pub async fn tick_once(&mut self) -> bool {
        let local = self.local_snapshot();
        let index = match self.index_snapshot().await {
            Ok(i) => i,
            Err(e) => {
                eprintln!("ssync: index snapshot: {e:#}");
                return false;
            }
        };
        let mut changed = false;
        for action in reconcile(&self.state, &local, &index) {
            changed |= self.execute(&action).await;
        }
        // prune state for keys gone from both dir and index
        let live: std::collections::HashSet<&str> = local
            .iter()
            .map(|f| f.key.as_str())
            .chain(index.keys().map(String::as_str))
            .collect();
        self.state.keys.retain(|k, _| live.contains(k.as_str()));
        changed
    }

    /// Execute one action and settle it into the carried state, so a
    /// self-write never echoes on the next pass.
    async fn execute(&mut self, action: &Action) -> bool {
        match action {
            Action::Import {
                key,
                path,
                stamp,
                winner,
            } => match self.import_action(key, path, *winner).await {
                Ok(outcome) => {
                    self.state.settle_import(key, *stamp, outcome.hash());
                    matches!(outcome, ImportOutcome::Published(_))
                }
                Err(e) => {
                    eprintln!("ssync: import {}: {e:#}", path.display());
                    false
                }
            },
            Action::WriteFile { key, hash } => {
                let Some(dest) = self.dest_of(key) else {
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
                if let Err(e) = atomic_write(&dest, &plaintext).await {
                    eprintln!("ssync: write {}: {e:#}", dest.display());
                    return false;
                }
                self.state
                    .settle_write(key, *hash, file_stamp_micros(&dest));
                true
            }
            Action::DeleteLocal { key } => {
                self.state.settle_delete(key);
                let Some(dest) = self.dest_of(key) else {
                    return false;
                };
                let existed = dest.exists();
                let _ = tokio::fs::remove_file(&dest).await;
                existed
            }
            Action::Tombstone { key } => match self.node.index_delete(key).await {
                Ok(()) => {
                    self.state.settle_delete(key);
                    true
                }
                Err(e) => {
                    eprintln!("ssync: delete {key}: {e:#}");
                    false
                }
            },
            Action::Merge { key } => match self.merge_one(key).await {
                Ok(Some(rel)) => {
                    if self.merged_logged.insert(key.clone()) {
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

    /// A [`tick_once`](Self::tick_once) pass plus the status refresh (the
    /// snapshot's mtime is the liveness signal for `ssync status`).
    async fn step(&mut self, status_path: &Path) {
        let changed = self.tick_once().await;
        self.write_status(status_path, changed).await;
    }

    /// Run the daemon: filesystem events, index events, and a periodic rescan
    /// (fallback for missed events) all funnel into the same debounced step.
    pub async fn run(&mut self, status_path: &Path) -> Result<()> {
        use std::time::Duration;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        for adapter in &self.adapters {
            watcher
                .watch(adapter.session_root(), RecursiveMode::Recursive)
                .with_context(|| format!("watching {}", adapter.session_root().display()))?;
        }

        let events = self.node.subscribe().await?;
        let mut events = std::pin::pin!(events);

        // initial reconcile
        self.step(status_path).await;

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
                    if let Ok(event) = res
                        && event.paths.iter().any(|p| {
                            self.adapter_of_path(p).is_some_and(|a| a.is_session_file(p))
                        }) {
                            deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
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
                    self.step(status_path).await;
                }
                _ = rescan.tick() => {
                    self.step(status_path).await;
                }
            }
        }
    }
}

/// What the file now matches: a fresh blob, or the winner it deduped against.
#[derive(Debug, Clone, Copy)]
enum ImportOutcome {
    Published(Hash),
    Unchanged(Hash),
}

impl ImportOutcome {
    fn hash(self) -> Hash {
        match self {
            Self::Published(h) | Self::Unchanged(h) => h,
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
fn session_files(root: &Path, adapter: &dyn Adapter) -> Vec<PathBuf> {
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
            age_identity_path = "/run/secrets/age/key"
            data_dir = "/var/lib/ssync"
            namespace_secret_path = "/run/secrets/ns/secret"
            node_key_path = "/run/secrets/node/key"
            peers = [ "aaa", "bbb" ]

            [[agents]]
            agent = "pi"
            session_dir = "/home/x/.pi/agent/sessions"

            [[agents]]
            agent = "omp"
            session_dir = "/home/x/.omp/agent/sessions"
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.agents.len(), 2);
        assert_eq!(cfg.agents[1].agent, "omp");
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
        assert_eq!(cfg.agents.len(), cfg2.agents.len());
    }

    #[test]
    fn config_expands_leading_tilde_in_paths() {
        let toml_str = r#"
            age_identity_path = "~/.config/ssync/age.key"
            data_dir = "~/.local/share/ssync"
            namespace_secret_path = "/run/secrets/ns"

            [[agents]]
            agent = "pi"
            session_dir = "~/.pi/agent/sessions"
        "#;
        let cfg = Config::parse(toml_str).unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(cfg.agents[0].session_dir, home.join(".pi/agent/sessions"));
        assert_eq!(cfg.age_identity_path, home.join(".config/ssync/age.key"));
        assert_eq!(cfg.data_dir, home.join(".local/share/ssync"));
        // absolute paths stay untouched
        assert_eq!(
            cfg.namespace_secret_path.as_deref(),
            Some(Path::new("/run/secrets/ns"))
        );
    }

    #[test]
    fn config_defaults_when_shared_fields_absent() {
        // a pre-shared-namespace config (ticket flow) still parses.
        let toml_str = r#"
            age_identity_path = "/a"
            data_dir = "/d"

            [[agents]]
            agent = "pi"
            session_dir = "/s"
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
    async fn tick_imports_encrypted_and_round_trips_decrypted() {
        let base = std::env::temp_dir().join(format!("ssync-core-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sessions_root = base.join("sessions");
        let proj = sessions_root.join("--home-simon-Projects-demo--");
        std::fs::create_dir_all(&proj).unwrap();
        let session_path =
            proj.join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl");
        let contents = b"{\"type\":\"session\",\"version\":3}\n{\"secret\":\"sk-live-abc\"}\n";
        std::fs::write(&session_path, contents).unwrap();

        let mut node = Node::spawn(&base.join("data"), SecretKey::generate())
            .await
            .unwrap();
        node.create_namespace().await.unwrap();
        let mut engine = Engine::new(
            PiAdapter::new("pi", &sessions_root),
            AgeIdentity::generate().unwrap(),
            node,
        );

        assert!(engine.tick_once().await, "first tick must import");
        assert!(!engine.tick_once().await, "second tick must be a no-op");

        let rec = engine
            .node
            .index_record(engine.index_key(&engine.identify(&session_path).unwrap()))
            .await
            .unwrap()
            .expect("session indexed");
        let hash = rec.winner.expect("live winner");
        let ciphertext = engine.node.get_blob(hash).await.unwrap();
        assert_ne!(&ciphertext[..], &contents[..], "blob must not be plaintext");
        assert_eq!(engine.get_plain(hash).await.as_deref(), Some(&contents[..]));
    }

    #[tokio::test]
    async fn divergence_waits_for_all_version_blobs() {
        let base = std::env::temp_dir().join(format!("ssync-div-wait-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let node = Node::spawn(&base.join("data"), SecretKey::generate())
            .await
            .unwrap();
        let engine = Engine::new(
            PiAdapter::new("pi", base.join("sessions")),
            AgeIdentity::generate().unwrap(),
            node,
        );
        let v1 = engine.identity.encrypt(b"h\na\n").unwrap();
        let h1 = engine.node.add_blob(v1).await.unwrap();
        let missing = Hash::new(b"never-added");

        assert_eq!(engine.is_diverged("pi/p/s", h1, &[h1, missing]).await, None);
        assert!(
            engine.divergence_cache.lock().unwrap().is_empty(),
            "partial verdict must not be cached"
        );
    }

    #[tokio::test]
    async fn divergence_verdict_is_cached_by_version_set() {
        let base = std::env::temp_dir().join(format!("ssync-div-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let node = Node::spawn(&base.join("data"), SecretKey::generate())
            .await
            .unwrap();
        let engine = Engine::new(
            PiAdapter::new("pi", base.join("sessions")),
            AgeIdentity::generate().unwrap(),
            node,
        );
        let enc = |b: &[u8]| engine.identity.encrypt(b).unwrap();
        let h1 = engine.node.add_blob(enc(b"h\na\n")).await.unwrap();
        let h2 = engine.node.add_blob(enc(b"h\nb\n")).await.unwrap();

        assert_eq!(engine.is_diverged("k", h2, &[h1, h2]).await, Some(true));
        assert_eq!(
            engine
                .divergence_cache
                .lock()
                .unwrap()
                .get("k")
                .map(|(_, v)| *v),
            Some(true)
        );

        // once the union is the winner, the same key settles
        let union = merge_lines(&[b"h\na\n".to_vec(), b"h\nb\n".to_vec()]);
        let hu = engine.node.add_blob(enc(&union)).await.unwrap();
        assert_eq!(
            engine.is_diverged("k", hu, &[h1, h2, hu]).await,
            Some(false)
        );
        assert_eq!(
            engine
                .divergence_cache
                .lock()
                .unwrap()
                .get("k")
                .map(|(_, v)| *v),
            Some(false)
        );
    }
}
