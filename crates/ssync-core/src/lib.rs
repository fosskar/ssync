//! sync engine, index model, conflict logic. Session bytes are stored
//! opaque: read → age-encrypt → blob; the index maps identity → hash. All
//! mutation flows through one path: snapshot → [`reconcile`] → execute.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use ssync_adapters::Adapter;
use ssync_crypto::AgeIdentity;
use ssync_net::Node;

use ssync_net::iroh_blobs::Hash;

pub mod cleanup;
pub mod cluster;
mod config;
mod divergence;
mod exclude;
mod reconcile;
pub mod search;
mod session_filesystem;
mod status;
pub use config::{AgentConfig, Config, Discovery, insert_cluster_path};
use divergence::{Divergence, Verdict};
use reconcile::{Action, SyncState, reconcile};
use session_filesystem::{LocalAttempt, SessionPass, StatusIndexRecord};
pub use session_filesystem::{PathMap, SessionFilesystem, SessionFsConfig};
pub use status::{PeerStatus, StatusReport};

/// Consecutive state-persist ENOENT failures after which the daemon exits so
/// its supervisor restarts it with a fresh mount namespace.
const PERSIST_WEDGE_THRESHOLD: u32 = 5;

/// The sync engine for one node: one or more agent adapters (pi, omp, ...)
/// sharing a single index namespace, partitioned by the `{agent}/` key prefix.
pub struct Engine {
    filesystem: SessionFilesystem,
    identity: AgeIdentity,
    node: Node,
    /// What we last materialised per key; feeds [`reconcile`].
    state: SyncState,
    /// Where the carried state persists across restarts; `None` = memory-only.
    state_path: Option<PathBuf>,
    /// Merges already announced (log once, not per tick).
    merged_logged: std::collections::HashSet<String>,
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
    status_records: Option<Vec<StatusIndexRecord>>,
}
struct Execution<'a, 'filesystem> {
    pass: &'a mut SessionPass<'filesystem>,
    identity: &'a AgeIdentity,
    node: &'a Node,
    state: &'a mut SyncState,
    divergence: &'a Divergence,
    merged_logged: &'a mut std::collections::HashSet<String>,
    rotation_pending: bool,
    import_errors: &'a mut usize,
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
        let filesystem = SessionFilesystem::new(SessionFsConfig {
            adapters,
            excludes: Default::default(),
            path_map: PathMap::default(),
            canonical_home: None,
        })
        .expect("Engine::with_adapters requires unique agents and non-overlapping roots");
        Self::with_filesystem(filesystem, identity, node)
    }

    pub fn with_filesystem(
        filesystem: SessionFilesystem,
        identity: AgeIdentity,
        node: Node,
    ) -> Self {
        let recipients_fp = Hash::new(identity.recipients().join("\n")).to_string();
        Self {
            filesystem,
            identity,
            node,
            state: SyncState::default(),
            state_path: None,
            merged_logged: Default::default(),
            conflicts_logged: Default::default(),
            persist_enoent: 0,
            divergence: Divergence::default(),
            resync_interval: std::time::Duration::from_secs(60),
            status_records: None,
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
        let records = self.node.index_records().await?;
        let records = self.filesystem.project_status_records(records);
        self.status_from_records(&records).await
    }

    async fn status_from_records(&self, records: &[StatusIndexRecord]) -> Result<StatusReport> {
        let mut sessions = std::collections::HashSet::new();
        let mut conflicts = Vec::new();
        for rec in records {
            let Some(winner) = rec.winner else {
                continue;
            };
            let key = &rec.key;
            if self.filesystem.excluded(key) {
                continue;
            }
            match self.filesystem.session_identity_of_key(key) {
                Some(id) => sessions.insert((id.agent, id.project_id, id.session_id)),
                None => sessions.insert((key.clone(), String::new(), String::new())),
            };
            if rec.versions.len() <= 1 {
                continue;
            }
            let Some(rel) = self.filesystem.relative_of(key).map(str::to_string) else {
                continue;
            };
            let diverged = match self.divergence.cached(key, &rec.versions) {
                Some(diverged) => diverged,
                None => matches!(
                    self.verdict_of(key, winner, &rec.versions).await,
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
            .map(|peer| PeerStatus {
                id: peer.id.to_string(),
                path: peer.kind.to_string(),
            })
            .collect();
        Ok(StatusReport {
            namespace: self.node.namespace().map(|namespace| namespace.to_string()),
            sessions: sessions.len(),
            conflicts,
            peers,
        })
    }

    async fn get_plain(&self, hash: Hash) -> Option<Vec<u8>> {
        let ciphertext = self.node.get_blob(hash).await.ok()?;
        self.identity.decrypt(&ciphertext).await.ok()
    }

    async fn get_plain_with(identity: &AgeIdentity, node: &Node, hash: Hash) -> Option<Vec<u8>> {
        let ciphertext = node.get_blob(hash).await.ok()?;
        identity.decrypt(&ciphertext).await.ok()
    }

    async fn verdict_with(
        identity: &AgeIdentity,
        node: &Node,
        divergence: &Divergence,
        key: &str,
        winner: Hash,
        versions: &[Hash],
    ) -> Verdict {
        let winner_pt = Self::get_plain_with(identity, node, winner).await;
        let mut plaintexts = Vec::with_capacity(versions.len());
        if winner_pt.is_some() {
            for hash in versions {
                let Some(plaintext) = Self::get_plain_with(identity, node, *hash).await else {
                    break;
                };
                plaintexts.push(plaintext);
            }
        }
        divergence.verdict(key, versions, winner_pt, plaintexts)
    }

    async fn merge_with(
        pass: &SessionPass<'_>,
        identity: &AgeIdentity,
        node: &Node,
        divergence: &Divergence,
        key: &str,
    ) -> Result<Option<String>> {
        let Some(relative) = pass.relative_of(key).map(str::to_string) else {
            return Ok(None);
        };
        let Some(record) = node.index_record(key).await? else {
            return Ok(None);
        };
        // A winning tombstone means the session is deleted; never merge it back.
        let Some(winner) = record.winner else {
            return Ok(None);
        };
        if record.versions.len() <= 1 || divergence.cached(key, &record.versions) == Some(false) {
            return Ok(None);
        }
        let Verdict::Diverged(merged) =
            Self::verdict_with(identity, node, divergence, key, winner, &record.versions).await
        else {
            return Ok(None);
        };
        let ciphertext = identity.encrypt(&merged).await?;
        node.publish(key.to_string(), ciphertext).await?;
        Ok(Some(relative))
    }

    /// Write the status snapshot; `announce` additionally logs conflicts (off
    /// for the periodic liveness refresh, which would repeat them every tick).
    async fn write_status(&mut self, path: &Path, announce: bool) {
        let report = match self.status_records.take() {
            Some(records) => self.status_from_records(&records).await,
            None => self.status_report().await,
        };
        if let Ok(report) = report {
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

        let records = match self.node.index_records().await {
            Ok(records) => records,
            Err(error) => {
                eprintln!("ssync: index snapshot: {error:#}");
                return false;
            }
        };
        let mut pass = self.filesystem.begin_pass(records);
        let actions = reconcile(&self.state, pass.views().local, pass.views().index);
        let local_keys: std::collections::HashSet<String> = pass
            .views()
            .local
            .iter()
            .map(|file| file.key.clone())
            .collect();
        let mut changed = false;
        let mut index_changed = false;
        for action in actions {
            if let Action::Tombstone { key } = &action
                && pass.tombstone_withheld(key)
            {
                continue;
            }
            let action_changed = Self::execute(
                &action,
                Execution {
                    pass: &mut pass,
                    identity: &self.identity,
                    node: &self.node,
                    state: &mut self.state,
                    divergence: &self.divergence,
                    merged_logged: &mut self.merged_logged,
                    rotation_pending: self.rotation_pending,
                    import_errors: &mut self.import_errors,
                },
            )
            .await;
            changed |= action_changed;
            index_changed |= action_changed
                && matches!(
                    action,
                    Action::Import { .. } | Action::Tombstone { .. } | Action::Merge { .. }
                );
        }
        if index_changed {
            match self.node.index_records().await {
                Ok(fresh) => pass.replace_records(fresh),
                Err(error) => eprintln!("ssync: fresh status index snapshot: {error:#}"),
            }
        }
        // Rotation settles only after a complete clean pass; otherwise the
        // next pass must retry every key not confirmed under the new recipients.
        if self.import_errors == 0
            && pass.pass_complete()
            && (self.rotation_pending || self.state.recipients.is_none())
        {
            self.state.recipients = Some(self.recipients_fp.clone());
            self.rotation_pending = false;
        }
        self.state.keys.retain(|key, _| {
            local_keys.contains(key)
                || pass.views().index.contains_key(key)
                || pass.state_retained(key)
        });
        self.status_records = Some(pass.into_records());
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
    async fn execute(action: &Action, execution: Execution<'_, '_>) -> bool {
        let Execution {
            pass,
            identity,
            node,
            state,
            divergence,
            merged_logged,
            rotation_pending,
            import_errors,
        } = execution;
        match action {
            Action::Import { key, stamp, winner } => {
                let plaintext = match pass.read(key, *stamp).await {
                    LocalAttempt::Completed(plaintext) => plaintext,
                    LocalAttempt::Retry => {
                        *import_errors += 1;
                        return false;
                    }
                };
                let outcome = async {
                    // Age ciphertext is randomized, so dedup the plaintext unless
                    // recipient rotation requires fresh encryption.
                    if !rotation_pending
                        && let Some(winner) = winner
                        && Self::get_plain_with(identity, node, *winner)
                            .await
                            .as_deref()
                            == Some(plaintext.as_slice())
                    {
                        return Ok(ImportOutcome::Unchanged(*winner));
                    }
                    let ciphertext = identity.encrypt(&plaintext).await?;
                    let hash = node.publish(key.to_string(), ciphertext).await?;
                    Ok::<_, anyhow::Error>(ImportOutcome::Published(hash))
                }
                .await;
                match outcome {
                    Ok(outcome) => {
                        state.settle_import(key, *stamp, outcome.hash());
                        matches!(outcome, ImportOutcome::Published(_))
                    }
                    Err(error) => {
                        *import_errors += 1;
                        eprintln!("ssync: import {key}: {error:#}");
                        false
                    }
                }
            }
            Action::WriteFile { key, hash } => {
                let ciphertext = match node.blob(*hash).await {
                    Ok(ciphertext) => ciphertext,
                    Err(error) => {
                        *import_errors += 1;
                        eprintln!("ssync: fetch {key}: {error:#}");
                        return false;
                    }
                };
                let plaintext = match identity.decrypt(&ciphertext).await {
                    Ok(plaintext) => plaintext,
                    Err(error) => {
                        *import_errors += 1;
                        eprintln!("ssync: decrypt {key}: {error:#}");
                        return false;
                    }
                };
                match pass.write(key, plaintext).await {
                    LocalAttempt::Completed(stamp) => {
                        state.settle_write(key, *hash, Some(stamp));
                        if rotation_pending || state.recipients.is_none() {
                            state.keys.get_mut(key).expect("settled above").import_stamp = None;
                            *import_errors += 1;
                        }
                        true
                    }
                    LocalAttempt::Retry => {
                        *import_errors += 1;
                        false
                    }
                }
            }
            Action::DeleteLocal { key } => match pass.delete(key).await {
                LocalAttempt::Completed(existed) => {
                    state.settle_delete(key);
                    existed
                }
                LocalAttempt::Retry => {
                    *import_errors += 1;
                    false
                }
            },
            Action::Tombstone { key } => match node.index_delete(key).await {
                Ok(()) => {
                    state.settle_delete(key);
                    true
                }
                Err(error) => {
                    *import_errors += 1;
                    eprintln!("ssync: delete {key}: {error:#}");
                    false
                }
            },
            Action::Merge { key } => {
                match Self::merge_with(pass, identity, node, divergence, key).await {
                    Ok(Some(relative)) => {
                        if merged_logged.insert(key.clone()) {
                            eprintln!(
                                "ssync: merged divergent session {relative} (lossless union, nothing lost)"
                            );
                        }
                        true
                    }
                    Ok(None) => false,
                    Err(error) => {
                        *import_errors += 1;
                        eprintln!("ssync: merge {key}: {error:#}");
                        false
                    }
                }
            }
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

        let mut fs_signals = self.filesystem.signals()?;

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
                Some(()) = fs_signals.recv() => {
                    deadline = Some(tokio::time::Instant::now() + DEBOUNCE);
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
            AgeIdentity::generate().await.unwrap(),
            node,
        );

        assert!(engine.tick_once().await, "first tick must import");
        assert!(!engine.tick_once().await, "second tick must be a no-op");

        let key = format!(
            "pi/{}",
            session_path.strip_prefix(&sessions_root).unwrap().display()
        );
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
    async fn fresh_state_write_back_remains_unsettled_until_republished() {
        let base = std::env::temp_dir().join(format!("ssync-fresh-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let sessions_root = base.join("sessions");
        std::fs::create_dir_all(&sessions_root).unwrap();
        let node = Node::spawn(&base.join("data"), SecretKey::generate())
            .await
            .unwrap();
        let identity = AgeIdentity::generate().await.unwrap();
        let ciphertext = identity.encrypt(b"session").await.unwrap();
        let hash = node.add_blob(ciphertext).await.unwrap();
        let mut engine = Engine::new(PiAdapter::new("pi", &sessions_root), identity, node);
        let key = "pi/--project--/session.jsonl";

        let mut pass = engine.filesystem.begin_pass(Vec::new());
        assert!(
            Engine::execute(
                &Action::WriteFile {
                    key: key.to_string(),
                    hash,
                },
                Execution {
                    pass: &mut pass,
                    identity: &engine.identity,
                    node: &engine.node,
                    state: &mut engine.state,
                    divergence: &engine.divergence,
                    merged_logged: &mut engine.merged_logged,
                    rotation_pending: engine.rotation_pending,
                    import_errors: &mut engine.import_errors,
                },
            )
            .await
        );
        drop(pass);

        assert_eq!(engine.import_errors, 1);
        assert_eq!(engine.state.keys[key].import_stamp, None);
        assert!(engine.state.recipients.is_none());
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
            AgeIdentity::generate().await.unwrap(),
            node,
        );
        let v1 = engine.identity.encrypt(b"h\na\n").await.unwrap();
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
            AgeIdentity::generate().await.unwrap(),
            node,
        );
        let h1 = engine
            .node
            .add_blob(engine.identity.encrypt(b"h\na\n").await.unwrap())
            .await
            .unwrap();
        let h2 = engine
            .node
            .add_blob(engine.identity.encrypt(b"h\nb\n").await.unwrap())
            .await
            .unwrap();

        let Verdict::Diverged(union) = engine.verdict_of("k", h2, &[h1, h2]).await else {
            panic!("fork must read as diverged");
        };
        assert_eq!(engine.divergence.cached("k", &[h1, h2]), Some(true));

        // once the union is the winner, the same key settles
        let hu = engine
            .node
            .add_blob(engine.identity.encrypt(&union).await.unwrap())
            .await
            .unwrap();
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
            AgeIdentity::generate().await.unwrap(),
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
            AgeIdentity::generate().await.unwrap(),
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
            AgeIdentity::generate().await.unwrap(),
            node,
        );
        assert!(engine.tick_once().await, "tick must import");

        let report = engine.status_report().await.unwrap();
        assert_eq!(report.sessions, 1, "3 files, 1 session");
    }

    #[tokio::test]
    async fn standalone_status_skips_hostile_paths_without_hiding_unconfigured_agents() {
        let base = std::env::temp_dir().join(format!("ssync-status-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut node = Node::spawn(&base.join("data"), SecretKey::generate())
            .await
            .unwrap();
        node.create_namespace().await.unwrap();
        node.publish("pi/../escape".to_string(), b"bad".to_vec())
            .await
            .unwrap();
        node.publish("ghost/project/session".to_string(), b"good".to_vec())
            .await
            .unwrap();
        let engine = Engine::new(
            PiAdapter::new("pi", base.join("unreadable-sessions")),
            AgeIdentity::generate().await.unwrap(),
            node,
        );

        let report = engine.status_report().await.unwrap();

        assert_eq!(report.sessions, 1);
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
            AgeIdentity::generate().await.unwrap(),
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
