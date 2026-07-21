//! sync engine, index model, conflict logic. Session bytes are stored
//! opaque: read → age-encrypt → blob; the index maps identity → hash. All
//! mutation flows through one path: snapshot → [`reconcile`] → execute.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use notify::{RecursiveMode, Watcher};
use ssync_adapters::Adapter;
use ssync_crypto::AgeIdentity;
use ssync_net::Node;

use ssync_net::iroh_blobs::Hash;

pub mod cleanup;
pub mod cluster;
mod config;
mod divergence;
mod exclude;
mod pathmap;
mod reconcile;
pub mod search;
mod status;
mod wiremap;
pub use config::{AgentConfig, Config, Discovery, insert_cluster_path};
use divergence::{Divergence, Verdict};
pub use pathmap::PathMap;
use reconcile::{Action, IndexEntry, IndexHead, LocalFile, SyncState, reconcile};
pub use status::{PeerStatus, StatusReport};
use wiremap::Wiremap;

/// Consecutive state-persist ENOENT failures after which the daemon exits so
/// its supervisor restarts it with a fresh mount namespace.
const PERSIST_WEDGE_THRESHOLD: u32 = 5;

/// The sync engine for one node: one or more agent adapters (pi, omp, ...)
/// sharing a single index namespace, partitioned by the `{agent}/` key prefix.
pub struct Engine {
    wiremap: Wiremap,
    identity: AgeIdentity,
    node: Node,
    /// What we last materialised per key; feeds [`reconcile`].
    state: SyncState,
    /// Where the carried state persists across restarts; `None` = memory-only.
    state_path: Option<PathBuf>,
    /// Merges already announced (log once, not per tick).
    merged_logged: std::collections::HashSet<String>,
    /// Identify failures already announced (log once, not per tick).
    identify_logged: std::collections::HashSet<PathBuf>,
    /// Conflicts already announced; replaced with the current set on every
    /// announcing pass so a resolved-then-returned conflict logs again.
    conflicts_logged: std::collections::HashSet<String>,
    /// Consecutive state-persist ENOENT failures. A system switch can leave
    /// the running unit's mount namespace stale — every data_dir write fails
    /// ENOENT until restart — so [`persist_wedged`](Self::persist_wedged)
    /// escalates instead of retrying forever.
    persist_enoent: u32,
    /// Cached divergence verdicts; skips re-decrypting sessions whose version
    /// set has not changed (e.g. a stale second author entry on every tick).
    divergence: Divergence,
    /// How often the daemon re-initiates sync with the known peers.
    resync_interval: std::time::Duration,
    /// Fingerprint of the configured recipient set (sorted, hashed).
    recipients_fp: String,
    /// Set while a recipient-set rotation is re-publishing (issue #22);
    /// cleared once a pass completes with no import errors.
    rotation_pending: bool,
    /// Import failures in the current pass; a rotation only settles at zero.
    import_errors: usize,
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
        let recipients_fp = Hash::new(identity.recipients().join("\n")).to_string();
        Self {
            wiremap: Wiremap::new(adapters),
            identity,
            node,
            state: SyncState::default(),
            state_path: None,
            merged_logged: Default::default(),
            identify_logged: Default::default(),
            conflicts_logged: Default::default(),
            persist_enoent: 0,
            divergence: Divergence::default(),
            resync_interval: std::time::Duration::from_secs(60),
            recipients_fp,
            rotation_pending: false,
            import_errors: 0,
        }
    }

    /// Load carried state from `path` and keep it persisted there after every
    /// pass. Restart then resumes where the last run settled — unchanged files
    /// import as no-ops without decryption, and a file deleted while the
    /// daemon was down reads as "we materialised it, now it is gone" ⇒
    /// tombstone instead of re-import.
    pub fn persist_state(&mut self, path: &Path) {
        self.state = SyncState::load(path);
        self.state_path = Some(path.to_path_buf());
    }

    /// Override the peer re-sync cadence (config `resync_interval_secs`;
    /// tests use short intervals).
    pub fn set_resync_interval(&mut self, interval: std::time::Duration) {
        self.resync_interval = interval;
    }

    /// Per-agent `exclude` patterns from config (`[[agents]]` tables).
    pub fn set_excludes(&mut self, excludes: std::collections::HashMap<String, Vec<String>>) {
        self.wiremap.set_excludes(excludes);
    }

    /// The `[[path_map]]` + `canonical_home` from config (issue #13).
    pub fn set_path_map(&mut self, map: PathMap, canonical_home: Option<PathBuf>) {
        self.wiremap.set_path_map(map, canonical_home);
    }

    /// Live index keys (wire form) — status/debug surface; the two-node
    /// tests assert wire hygiene through it.
    pub async fn index_keys(&self) -> Result<Vec<String>> {
        self.node
            .index_records()
            .await?
            .into_iter()
            .map(|r| String::from_utf8(r.key).context("index key not utf-8"))
            .collect()
    }

    /// Shut down the underlying node (flushes the blob store).
    pub async fn shutdown(self) -> Result<()> {
        self.node.shutdown().await
    }

    /// Read → encrypt → blob → upsert index. Dedups on *plaintext* (age
    /// ciphertext is randomized), or the exporter's own write-back echoes.
    /// `force` (recipient-set rotation) bypasses the dedup: the plaintext is
    /// unchanged but the encryption no longer matches the configured set.
    async fn import_action(
        &self,
        key: &str,
        path: &Path,
        winner: Option<Hash>,
        force: bool,
    ) -> Result<ImportOutcome> {
        let plaintext = tokio::fs::read(path)
            .await
            .with_context(|| format!("reading session file {}", path.display()))?;
        let plaintext = self.wiremap.canonical_plaintext(key, plaintext)?;
        if !force
            && let Some(w) = winner
            && self.get_plain(w).await.as_deref() == Some(&plaintext)
        {
            return Ok(ImportOutcome::Unchanged(w));
        }
        let ciphertext = self.identity.encrypt(&plaintext)?;
        let hash = self.node.publish(key.to_string(), ciphertext).await?;
        Ok(ImportOutcome::Published(hash))
    }

    /// The divergence verdict for a key, decrypting whatever the cache cannot
    /// answer. Decryption stops at the first unavailable blob; the resulting
    /// short set reads as incomplete in [`Divergence::verdict`].
    async fn verdict_of(&self, key: &str, winner: Hash, versions: &[Hash]) -> Verdict {
        let winner_pt = self.get_plain(winner).await;
        let mut plaintexts = Vec::with_capacity(versions.len());
        if winner_pt.is_some() {
            for h in versions {
                let Some(pt) = self.get_plain(*h).await else {
                    break;
                };
                plaintexts.push(pt);
            }
        }
        self.divergence
            .verdict(key, versions, winner_pt, plaintexts)
    }

    /// One index scan: live session count plus the still-diverged sessions
    /// (union of all authors' lines differs from the winner; cached verdicts).
    /// Artifact files carry their own index keys but belong to a session
    /// (DECISIONS §9), so the count is distinct session identities, not keys.
    pub async fn status_report(&self) -> Result<StatusReport> {
        let mut sessions = std::collections::HashSet::new();
        let mut conflicts = Vec::new();
        for rec in self.node.index_records().await? {
            let Some(winner) = rec.winner else {
                continue; // deleted — never resurrect from stale live entries
            };
            let key = String::from_utf8(rec.key).context("index key not utf-8")?;
            if self.wiremap.excluded(&key) {
                continue; // frozen keys neither count nor report conflicts
            }
            match self.wiremap.session_identity_of_key(&key) {
                Some(id) => sessions.insert((id.agent, id.project_id, id.session_id)),
                // unconfigured agent or unparseable path: the key is the session
                None => sessions.insert((key.clone(), String::new(), String::new())),
            };
            if rec.versions.len() <= 1 {
                continue;
            }
            let Some(rel) = self.wiremap.relative_of(&key).map(str::to_string) else {
                continue;
            };
            let diverged = match self.divergence.cached(&key, &rec.versions) {
                Some(d) => d,
                None => matches!(
                    self.verdict_of(&key, winner, &rec.versions).await,
                    Verdict::Diverged(_)
                ),
            };
            if diverged {
                conflicts.push(rel);
            }
        }
        let peers = self
            .node
            .peer_paths()
            .await
            .into_iter()
            .map(|p| PeerStatus {
                id: p.id.to_string(),
                path: p.kind.to_string(),
            })
            .collect();
        Ok(StatusReport {
            namespace: self.node.namespace().map(|n| n.to_string()),
            sessions: sessions.len(),
            conflicts,
            peers,
        })
    }

    async fn get_plain(&self, hash: Hash) -> Option<Vec<u8>> {
        let ciphertext = self.node.get_blob(hash).await.ok()?;
        self.identity.decrypt(&ciphertext).ok()
    }

    /// Publish the lossless union for a diverged key; the cached verdict gates
    /// it, so a settled key costs one lookup. Returns the relative path when
    /// it published.
    async fn merge_one(&self, key: &str) -> Result<Option<String>> {
        let Some(rel) = self.wiremap.relative_of(key).map(str::to_string) else {
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
        if self.divergence.cached(key, &rec.versions) == Some(false) {
            return Ok(None);
        }
        let Verdict::Diverged(merged) = self.verdict_of(key, winner, &rec.versions).await else {
            return Ok(None);
        };
        let ciphertext = self.identity.encrypt(&merged)?;
        self.node.publish(key.to_string(), ciphertext).await?;
        Ok(Some(rel))
    }

    /// Write the status snapshot; `announce` additionally logs conflicts (off
    /// for the periodic liveness refresh, which would repeat them every tick).
    async fn write_status(&mut self, path: &Path, announce: bool) {
        if let Ok(report) = self.status_report().await {
            if let Ok(text) = toml::to_string_pretty(&report) {
                let _ = tokio::fs::write(path, text).await;
            }
            if announce {
                for c in newly_diverged(&self.conflicts_logged, &report.conflicts) {
                    eprintln!("ssync: conflict on {c} (both versions kept; newest wins)");
                }
                self.conflicts_logged = report.conflicts.into_iter().collect();
            }
        }
    }

    /// Snapshot every configured session dir as `reconcile` input.
    fn local_snapshot(&mut self) -> Vec<LocalFile> {
        self.wiremap.begin_pass();
        let mut out = Vec::new();
        for path in self.wiremap.session_files() {
            let Some(stamp) = file_stamp_micros(&path) else {
                continue;
            };
            let id = match self.wiremap.identify(&path) {
                Ok(id) => id,
                Err(e) => {
                    // per-key errors are logged, never silently dropped
                    if self.identify_logged.insert(path.clone()) {
                        eprintln!("ssync: skipping {}: {e:#}", path.display());
                    }
                    continue;
                }
            };
            let key = match self.wiremap.key_of(&id, &path) {
                Ok(k) => k,
                Err(f) => {
                    // an unresolvable mapping FREEZES the agent's tombstones
                    // this pass (#49), recorded inside the wiremap: a skip
                    // that read as deletion would propagate mesh-wide
                    if f.announce {
                        eprintln!("ssync: skipping {}: {:#}", path.display(), f.error);
                    }
                    continue;
                }
            };
            if self.wiremap.excluded(&key) {
                continue;
            }
            out.push(LocalFile { key, path, stamp });
        }
        out
    }

    /// Snapshot the synced index as `reconcile` input: winning entry plus the
    /// count of distinct live hashes (divergence) per key.
    async fn index_snapshot(&self) -> Result<std::collections::HashMap<String, IndexEntry>> {
        let mut out = std::collections::HashMap::new();
        for rec in self.node.index_records().await? {
            let key = String::from_utf8(rec.key).context("index key not utf-8")?;
            // No adapter = frozen, exactly like an excluded key: a dropped
            // `[[agents]]` entry must never tombstone peers' sessions (the
            // key would read as "materialised here, now gone").
            if self.wiremap.frozen(&key) {
                continue;
            }
            let merge_allowed = self.wiremap.append_only(&key);
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
        // Recipient-set rotation (issue #22): a changed set invalidates every
        // published blob's encryption even though no plaintext changed, so
        // clear the import stamps once and re-publish with the dedup bypassed.
        if !self.rotation_pending
            && self
                .state
                .recipients
                .as_ref()
                .is_some_and(|stored| *stored != self.recipients_fp)
        {
            self.rotation_pending = true;
            for ks in self.state.keys.values_mut() {
                ks.import_stamp = None;
            }
        }
        self.import_errors = 0;

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
            // withhold tombstones for agents whose mapping failed this pass:
            // the "file gone" reading is an artifact of the skip, not a delete
            if let Action::Tombstone { key } = &action
                && self.wiremap.tombstone_withheld(key)
            {
                continue;
            }
            changed |= self.execute(&action).await;
        }
        // the rotation (or a fresh state) settles only after a clean pass, so
        // a failed import keeps forcing until every blob is re-encrypted.
        if self.import_errors == 0 && (self.rotation_pending || self.state.recipients.is_none()) {
            self.state.recipients = Some(self.recipients_fp.clone());
            self.rotation_pending = false;
        }
        // prune state for keys gone from both dir and index
        let live: std::collections::HashSet<&str> = local
            .iter()
            .map(|f| f.key.as_str())
            .chain(index.keys().map(String::as_str))
            .collect();
        self.state.keys.retain(|k, _| live.contains(k.as_str()));
        if let Some(path) = &self.state_path {
            match self.state.save(path) {
                Ok(()) => self.persist_enoent = 0,
                Err(e) => {
                    eprintln!("ssync: persist state {}: {e:#}", path.display());
                    if e.root_cause()
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                    {
                        self.persist_enoent += 1;
                    }
                }
            }
        }
        changed
    }

    /// Whether persisting state has hit ENOENT for so many consecutive passes
    /// that the data dir is presumed gone from this process's view (a stale
    /// mount namespace after a system switch). Retrying is then pointless;
    /// [`run`](Self::run) exits so the supervisor restarts with a fresh one.
    pub fn persist_wedged(&self) -> bool {
        self.persist_enoent >= PERSIST_WEDGE_THRESHOLD
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
            } => match self
                .import_action(key, path, *winner, self.rotation_pending)
                .await
            {
                Ok(outcome) => {
                    self.state.settle_import(key, *stamp, outcome.hash());
                    matches!(outcome, ImportOutcome::Published(_))
                }
                Err(e) => {
                    self.import_errors += 1;
                    eprintln!("ssync: import {}: {e:#}", path.display());
                    false
                }
            },
            Action::WriteFile { key, hash } => {
                // dest + on-disk bytes resolve through the path map after
                // decryption (the header carries the canonical cwd)
                let ciphertext = match self.node.blob(*hash).await {
                    Ok(c) => c,
                    // per-key error, retried next tick like every other arm
                    Err(e) => {
                        eprintln!("ssync: fetch {key}: {e:#}");
                        return false;
                    }
                };
                let plaintext = match self.identity.decrypt(&ciphertext) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("ssync: decrypt {key}: {e:#}");
                        return false;
                    }
                };
                let (dest, plaintext) = match self.wiremap.localize(key, plaintext) {
                    Ok(Some(pair)) => pair,
                    Ok(None) => return false, // unresolved yet — retried
                    Err(e) => {
                        eprintln!("ssync: write {key}: {e:#}");
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
                let Some(dest) = self.wiremap.local_dest_of(key) else {
                    return false;
                };
                let existed = dest.exists();
                let _ = tokio::fs::remove_file(&dest).await;
                // sweep the emptied artifact dir the deletion may leave behind
                if let Some(root) = self.wiremap.session_root_of(key) {
                    cleanup::remove_empty_parents(&dest, root);
                }
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
    /// snapshot's mtime is the liveness signal for `ssync status`). Errors
    /// only when persisting state is wedged (stale mount namespace): the
    /// daemon must exit so its supervisor restarts it with a fresh one.
    async fn step(&mut self, status_path: &Path) -> Result<()> {
        let changed = self.tick_once().await;
        self.write_status(status_path, changed).await;
        ensure!(
            !self.persist_wedged(),
            "state dir unreachable ({PERSIST_WEDGE_THRESHOLD} consecutive ENOENT persisting state); exiting for a clean restart"
        );
        Ok(())
    }

    /// Run the daemon: filesystem events, index events, and a periodic rescan
    /// (fallback for missed events) all funnel into the same debounced step.
    pub async fn run(&mut self, status_path: &Path) -> Result<()> {
        use std::time::Duration;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;
        for root in self.wiremap.roots() {
            watcher
                .watch(root, RecursiveMode::Recursive)
                .with_context(|| format!("watching {}", root.display()))?;
        }

        // Doc events are drained behind the Node seam (Node::signals): only
        // wake pings cross, and peers learned on the live stream are
        // recorded by the node itself.
        let mut erx = self.node.signals().await?;
        let mut events_ended = false;

        // initial reconcile
        self.step(status_path).await?;

        let mut rescan = tokio::time::interval(Duration::from_secs(15));
        rescan.tick().await;
        // Live links can die silently when a peer restarts (one-way sync until
        // reconnect); re-initiating sync is a cheap no-op when already in sync.
        let mut resync = tokio::time::interval(self.resync_interval);
        resync.tick().await;

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
                        && event.paths.iter().any(|p| self.wiremap.is_session_file(p)) {
                            deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
                        }
                }
                ev = erx.recv(), if !events_ended => {
                    match ev {
                        Some(()) => {
                            deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
                        }
                        // ended stream: disarm this select arm, or it is ready
                        // (None) on every loop iteration and spins the CPU at
                        // 100%; the rescan interval still drives ticks.
                        None => events_ended = true,
                    }
                }
                _ = settle, if deadline.is_some() => {
                    deadline = None;
                    self.step(status_path).await?;
                }
                _ = rescan.tick() => {
                    self.step(status_path).await?;
                }
                _ = resync.tick() => {
                    if let Err(e) = self.node.resync().await {
                        eprintln!("ssync: resync: {e:#}");
                    }
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

/// Conflicts to announce this pass: those in `current` not yet logged. The
/// caller replaces its logged set with `current` afterwards, so a conflict
/// that resolved and re-diverged announces again.
fn newly_diverged<'a>(
    logged: &std::collections::HashSet<String>,
    current: &'a [String],
) -> Vec<&'a str> {
    current
        .iter()
        .filter(|c| !logged.contains(*c))
        .map(String::as_str)
        .collect()
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

        let id = engine.wiremap.identify(&session_path).unwrap();
        let key = engine
            .wiremap
            .key_of(&id, &session_path)
            .map_err(|f| f.error)
            .unwrap();
        let rec = engine
            .node
            .index_record(key)
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

        assert_eq!(
            engine.verdict_of("pi/p/s", h1, &[h1, missing]).await,
            Verdict::Incomplete
        );
        assert_eq!(
            engine.divergence.cached("pi/p/s", &[h1, missing]),
            None,
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

        let Verdict::Diverged(union) = engine.verdict_of("k", h2, &[h1, h2]).await else {
            panic!("fork must read as diverged");
        };
        assert_eq!(engine.divergence.cached("k", &[h1, h2]), Some(true));

        // once the union is the winner, the same key settles
        let hu = engine.node.add_blob(enc(&union)).await.unwrap();
        assert_eq!(
            engine.verdict_of("k", hu, &[h1, h2, hu]).await,
            Verdict::Settled
        );
        assert_eq!(engine.divergence.cached("k", &[h1, h2, hu]), Some(false));
    }

    #[tokio::test]
    async fn status_report_lists_known_peers_with_path_kind() {
        let base = std::env::temp_dir().join(format!("ssync-status-peers-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut node = Node::spawn(&base.join("data"), SecretKey::generate())
            .await
            .unwrap();
        node.create_namespace().await.unwrap();
        let bogus = ssync_net::iroh::SecretKey::generate().public();
        node.sync_with(vec![ssync_net::iroh::EndpointAddr::from(bogus)])
            .await
            .unwrap();
        let engine = Engine::new(
            PiAdapter::new("pi", base.join("sessions")),
            AgeIdentity::generate().unwrap(),
            node,
        );
        let report = engine.status_report().await.unwrap();
        assert_eq!(report.peers.len(), 1);
        assert_eq!(report.peers[0].id, bogus.to_string());
        assert_eq!(report.peers[0].path, "unknown");
    }

    #[tokio::test]
    async fn tick_imports_nested_artifact_file() {
        // omp stores subagent transcripts in a per-session artifact dir nested
        // inside the session root: <root>/<enc>/<ts>_<uuid>/<Name>.jsonl. These
        // depth-3 files must import just like the depth-2 main session file. The
        // CamelCase name (no underscore) is the case silently skipped today.
        let base = std::env::temp_dir().join(format!("ssync-core-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sessions_root = base.join("sessions");
        let proj = sessions_root.join("--home-simon-Projects-demo--");
        let sess = "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a";
        std::fs::create_dir_all(&proj).unwrap();
        // main session file (depth 2)
        std::fs::write(
            proj.join(format!("{sess}.jsonl")),
            b"{\"type\":\"session\",\"version\":3}\n",
        )
        .unwrap();
        // nested subagent transcript (depth 3), CamelCase name with no underscore
        let artifact_dir = proj.join(sess);
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::fs::write(
            artifact_dir.join("Tests.jsonl"),
            b"{\"type\":\"session\",\"version\":3}\n{\"agent\":\"Tests\"}\n",
        )
        .unwrap();

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

        // The nested artifact is indexed under its full relative-path key.
        let key = "pi/--home-simon-Projects-demo--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a/Tests.jsonl";
        let rec = engine.node.index_record(key).await.unwrap();
        assert!(
            rec.is_some(),
            "depth-3 artifact file must be indexed under {key}"
        );
    }
    #[tokio::test]
    async fn status_counts_sessions_not_files() {
        // artifact files get their own index keys but are part of their
        // session (DECISIONS §9): status must count distinct sessions.
        let base = std::env::temp_dir().join(format!("ssync-core-count-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sessions_root = base.join("sessions");
        let proj = sessions_root.join("--p--");
        let sess = "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a";
        std::fs::create_dir_all(proj.join(sess)).unwrap();
        std::fs::write(
            proj.join(format!("{sess}.jsonl")),
            b"{\"type\":\"session\",\"version\":3}\n",
        )
        .unwrap();
        std::fs::write(proj.join(sess).join("Tests.jsonl"), b"{\"a\":1}\n").unwrap();
        std::fs::write(proj.join(sess).join("__advisor.jsonl"), b"{\"a\":2}\n").unwrap();

        let mut node = Node::spawn(&base.join("data"), SecretKey::generate())
            .await
            .unwrap();
        node.create_namespace().await.unwrap();
        let mut engine = Engine::new(
            PiAdapter::new("pi", &sessions_root),
            AgeIdentity::generate().unwrap(),
            node,
        );
        assert!(engine.tick_once().await, "tick must import");

        let report = engine.status_report().await.unwrap();
        assert_eq!(report.sessions, 1, "3 files, 1 session");
    }

    #[tokio::test]
    async fn persist_enoent_wedges_after_consecutive_failures() {
        // a system switch can invalidate the running unit's mount namespace:
        // every write under data_dir fails ENOENT until restart (observed as a
        // 17h silent retry loop). the engine must read as wedged so the daemon
        // exits and systemd restarts it with a fresh namespace.
        let base = std::env::temp_dir().join(format!("ssync-core-wedge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut node = Node::spawn(&base.join("data"), SecretKey::generate())
            .await
            .unwrap();
        node.create_namespace().await.unwrap();
        let mut engine = Engine::new(
            PiAdapter::new("pi", base.join("sessions")),
            AgeIdentity::generate().unwrap(),
            node,
        );
        let state_path = base.join("missing-dir/state.toml");
        engine.persist_state(&state_path);

        for _ in 0..PERSIST_WEDGE_THRESHOLD {
            assert!(!engine.persist_wedged(), "must not wedge early");
            engine.tick_once().await;
        }
        assert!(engine.persist_wedged(), "consecutive ENOENT must wedge");

        // dir back (namespace healed / different failure): one good pass resets
        std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
        engine.tick_once().await;
        assert!(!engine.persist_wedged(), "successful persist must reset");
    }

    #[test]
    fn conflicts_announce_once_and_reannounce_on_return() {
        let logged = std::collections::HashSet::new();
        let current = vec!["a".to_string(), "b".to_string()];
        assert_eq!(newly_diverged(&logged, &current), ["a", "b"]);

        // already-announced conflicts stay quiet
        let logged: std::collections::HashSet<String> = current.iter().cloned().collect();
        assert!(newly_diverged(&logged, &current).is_empty());
        let wider = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(newly_diverged(&logged, &wider), ["c"]);

        // a conflict that resolved and re-diverged announces again: the caller
        // replaces its logged set with the current set each announcing pass
        let logged: std::collections::HashSet<String> = ["a".to_string()].into();
        assert_eq!(newly_diverged(&logged, &current), ["b"]);
    }
}
