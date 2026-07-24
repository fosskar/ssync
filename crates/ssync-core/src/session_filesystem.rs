//! Session filesystem policy: bounded discovery, wire-key ↔ local-path
//! translation, path-map resolution, freeze verdicts, and contained mutation.
//! Owns every adapter and every filesystem invariant around its session root.

mod pathmap;

pub use pathmap::PathMap;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::reconcile::{IndexEntry, IndexHead, LocalFile};
use anyhow::{Context, Result, anyhow, ensure};
use ssync_adapters::{Adapter, SessionIdentity};
use ssync_net::IndexRecord;
use ssync_net::iroh_blobs::Hash;
use tokio::io::AsyncWriteExt;

use crate::exclude;
use pathmap::Resolver;

#[derive(Clone)]
struct ObservedFile {
    path: PathBuf,
    stamp: (u64, u64),
}
pub(crate) struct FsSignals {
    _watcher: notify::RecommendedWatcher,
    receiver: tokio::sync::mpsc::Receiver<()>,
}

impl FsSignals {
    pub async fn recv(&mut self) -> Option<()> {
        self.receiver.recv().await
    }
}

pub(crate) enum LocalAttempt<T> {
    Completed(T),
    Retry,
}

pub(crate) struct StatusIndexRecord {
    pub key: String,
    pub winner: Option<Hash>,
    pub versions: Vec<Hash>,
}

pub(crate) struct PassViews<'a> {
    pub local: &'a [LocalFile],
    pub index: &'a HashMap<String, IndexEntry>,
}

pub(crate) struct SessionPass<'a> {
    filesystem: &'a mut SessionFilesystem,
    local: Vec<LocalFile>,
    index: HashMap<String, IndexEntry>,
    records: Vec<StatusIndexRecord>,
}

impl SessionPass<'_> {
    pub fn views(&self) -> PassViews<'_> {
        PassViews {
            local: &self.local,
            index: &self.index,
        }
    }

    pub fn replace_records(&mut self, records: Vec<IndexRecord>) {
        let (records, index) = self.filesystem.project_pass_records(records);
        self.index = index;
        self.records = records;
    }

    pub fn into_records(self) -> Vec<StatusIndexRecord> {
        self.records
    }

    pub async fn read(&mut self, key: &str, stamp: (u64, u64)) -> LocalAttempt<Vec<u8>> {
        let result = self.filesystem.read(key, stamp).await;
        self.filesystem.local_attempt("read", key, result)
    }

    pub async fn write(&mut self, key: &str, plaintext: Vec<u8>) -> LocalAttempt<(u64, u64)> {
        let result = self
            .filesystem
            .write(key, plaintext)
            .await
            .and_then(|stamp| stamp.ok_or_else(|| anyhow!("{key}: destination unresolved")));
        self.filesystem.local_attempt("write", key, result)
    }

    pub async fn delete(&mut self, key: &str) -> LocalAttempt<bool> {
        let result = self.filesystem.delete(key).await;
        self.filesystem.local_attempt("delete", key, result)
    }

    pub fn tombstone_withheld(&self, key: &str) -> bool {
        self.filesystem.tombstone_withheld(key)
    }

    pub fn relative_of<'a>(&self, key: &'a str) -> Option<&'a str> {
        self.filesystem.relative_of(key)
    }

    pub fn pass_complete(&self) -> bool {
        self.filesystem.pass_complete()
    }

    pub fn state_retained(&self, key: &str) -> bool {
        self.filesystem.state_retained(key)
    }
}

/// Session discovery, translation, policy, and mutation for one engine.
pub struct SessionFsConfig {
    pub adapters: Vec<Box<dyn Adapter>>,
    pub excludes: HashMap<String, Vec<String>>,
    pub path_map: PathMap,
    pub canonical_home: Option<PathBuf>,
}

pub struct SessionFilesystem {
    adapters: Arc<[Box<dyn Adapter>]>,
    /// Per-agent session exclusion patterns (issue #14); a matching key is
    /// invisible to reconcile from both sides, freezing it everywhere.
    excludes: HashMap<String, Vec<String>>,
    /// Wire↔local path-map translation (issue #13, map #42); default = inert.
    resolver: Resolver,
    identify_logged: HashSet<PathBuf>,
    incomplete_agents: HashSet<String>,
    observed: HashMap<String, ObservedFile>,
    operation_logged: HashSet<(String, String)>,
    malformed_index_logged: std::sync::Mutex<HashSet<Vec<u8>>>,
}

impl SessionFilesystem {
    #[cfg(test)]
    fn for_tests(adapters: Vec<Box<dyn Adapter>>) -> Self {
        Self {
            adapters: adapters.into(),
            excludes: HashMap::new(),
            resolver: Resolver::default(),
            identify_logged: HashSet::new(),
            incomplete_agents: HashSet::new(),
            observed: HashMap::new(),
            operation_logged: HashSet::new(),
            malformed_index_logged: std::sync::Mutex::new(HashSet::new()),
        }
    }

    pub fn new(config: SessionFsConfig) -> Result<Self> {
        Self::configured(
            config.adapters,
            config.excludes,
            config.path_map,
            config.canonical_home,
        )
    }

    fn configured(
        adapters: Vec<Box<dyn Adapter>>,
        excludes: HashMap<String, Vec<String>>,
        map: PathMap,
        canonical_home: Option<PathBuf>,
    ) -> Result<Self> {
        validate_adapters(&adapters)?;
        Ok(Self {
            adapters: adapters.into(),
            excludes,
            resolver: Resolver::new(map, canonical_home),
            identify_logged: HashSet::new(),
            incomplete_agents: HashSet::new(),
            observed: HashMap::new(),
            operation_logged: HashSet::new(),
            malformed_index_logged: std::sync::Mutex::new(HashSet::new()),
        })
    }

    #[cfg(test)]
    fn set_excludes(&mut self, excludes: HashMap<String, Vec<String>>) {
        self.excludes = excludes;
    }

    #[cfg(test)]
    fn set_path_map(&mut self, map: PathMap, canonical_home: Option<PathBuf>) {
        self.resolver = Resolver::new(map, canonical_home);
    }

    fn local_attempt<T>(
        &mut self,
        operation: &str,
        key: &str,
        result: Result<T>,
    ) -> LocalAttempt<T> {
        let diagnostic = (operation.to_string(), key.to_string());
        match result {
            Ok(value) => {
                self.operation_logged.remove(&diagnostic);
                LocalAttempt::Completed(value)
            }
            Err(error) => {
                if self.operation_logged.insert(diagnostic) {
                    eprintln!("ssync: {operation} {key}: {error:#}");
                }
                LocalAttempt::Retry
            }
        }
    }

    /// Every configured session root (watch targets).
    fn roots(&self) -> impl Iterator<Item = &Path> {
        self.adapters.iter().map(|a| a.session_root())
    }
    pub(crate) fn signals(&mut self) -> Result<FsSignals> {
        use notify::Watcher;

        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        let adapters = Arc::clone(&self.adapters);
        let mut watcher =
            notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
                if result.is_ok_and(|event| {
                    event.paths.iter().any(|path| {
                        adapters.iter().any(|adapter| {
                            path.starts_with(adapter.session_root())
                                && adapter.is_session_file(path)
                        })
                    })
                }) {
                    let _ = sender.try_send(());
                }
            })?;
        for root in self.roots() {
            watcher
                .watch(root, notify::RecursiveMode::Recursive)
                .with_context(|| format!("watching {}", root.display()))?;
        }
        Ok(FsSignals {
            _watcher: watcher,
            receiver,
        })
    }

    /// Whether `path` is a session file of the adapter whose root contains it.
    #[cfg(test)]
    fn is_session_file(&self, path: &Path) -> bool {
        self.adapter_of_path(path)
            .is_some_and(|a| a.is_session_file(path))
    }

    /// Decode an index key back to its session-root-relative path (the
    /// inverse of the key encoding): strip the `{agent}/` prefix of a
    /// configured adapter.
    pub(crate) fn relative_of<'a>(&self, key: &'a str) -> Option<&'a str> {
        self.key_parts(key).map(|(_, rel)| rel)
    }

    /// The session identity an index key resolves to on this machine, if a
    /// configured adapter can parse its destination path.
    pub(crate) fn session_identity_of_key(&self, key: &str) -> Option<SessionIdentity> {
        let (idx, rel) = self.key_parts(key)?;
        validate_relative(rel).ok()?;
        let adapter = self.adapters[idx].as_ref();
        adapter.identify(&adapter.session_root().join(rel)).ok()
    }

    /// Whether a key falls under its agent's `exclude` patterns (issue #14).
    /// Filtered out of BOTH reconcile inputs, so the key is frozen: never
    /// imported, exported, tombstoned, or merged — here or on write-back.
    pub(crate) fn excluded(&self, key: &str) -> bool {
        let Some((idx, rel)) = self.key_parts(key) else {
            return false;
        };
        self.excluded_parts(self.adapters[idx].agent(), rel)
    }

    /// Whether a key is invisible to reconcile: excluded (#14) or owned by no
    /// configured adapter (dropped-agent guard — a removed `[[agents]]` entry
    /// must never tombstone peers' sessions).
    fn frozen(&self, key: &str) -> bool {
        let Some((idx, rel)) = self.key_parts(key) else {
            return true;
        };
        let agent = self.adapters[idx].agent();
        self.incomplete_agents.contains(agent)
            || self.resolver.agent_failed(agent)
            || self.excluded_parts(agent, rel)
    }

    /// Whether a tombstone for this key must be withheld this pass: its
    /// agent's mapping failed (#49), so "file gone" is an artifact of the
    /// skip, not a delete.
    fn tombstone_withheld(&self, key: &str) -> bool {
        self.adapter_of_key(key).is_some_and(|adapter| {
            self.incomplete_agents.contains(adapter.agent())
                || self.resolver.agent_failed(adapter.agent())
        })
    }

    fn state_retained(&self, key: &str) -> bool {
        self.adapter_of_key(key).is_some_and(|adapter| {
            self.incomplete_agents.contains(adapter.agent())
                || self.resolver.agent_failed(adapter.agent())
        })
    }

    fn pass_complete(&self) -> bool {
        self.incomplete_agents.is_empty() && !self.resolver.any_agent_failed()
    }
    /// Whether the key's format merges (append-only line union) rather than
    /// newest-wins. `false` for keys of unconfigured agents.
    fn append_only(&self, key: &str) -> bool {
        self.adapter_of_key(key).is_some_and(|a| a.append_only())
    }

    /// The adapter owning an index key (matching `{agent}/` prefix), if any —
    /// peers may sync agents this node does not have configured.
    fn adapter_of_key(&self, key: &str) -> Option<&dyn Adapter> {
        let (idx, _) = self.key_parts(key)?;
        Some(self.adapters[idx].as_ref())
    }

    /// The adapter whose session root contains `path`.
    #[cfg(test)]
    fn adapter_of_path(&self, path: &Path) -> Option<&dyn Adapter> {
        self.adapter_index_of_path(path)
            .map(|index| self.adapters[index].as_ref())
    }

    #[cfg(test)]
    fn adapter_index_of_path(&self, path: &Path) -> Option<usize> {
        self.adapters
            .iter()
            .position(|adapter| path.starts_with(adapter.session_root()))
    }

    fn key_parts<'a>(&self, key: &'a str) -> Option<(usize, &'a str)> {
        self.adapters
            .iter()
            .enumerate()
            .find_map(|(index, adapter)| {
                key.strip_prefix(adapter.agent())
                    .and_then(|rest| rest.strip_prefix('/'))
                    .map(|relative| (index, relative))
            })
    }

    pub(crate) fn status_key(&self, bytes: &[u8]) -> Option<String> {
        let key = match std::str::from_utf8(bytes) {
            Ok(key) => key,
            Err(error) => {
                self.note_malformed_index(bytes, &format!("invalid UTF-8: {error}"));
                return None;
            }
        };
        if structural_key(key).is_none() {
            self.note_malformed_index(bytes, "invalid path structure");
            return None;
        }
        Some(key.to_string())
    }

    fn note_malformed_index(&self, bytes: &[u8], reason: &str) {
        if self
            .malformed_index_logged
            .lock()
            .unwrap()
            .insert(bytes.to_vec())
        {
            eprintln!("ssync: skipping malformed index key ({reason})");
        }
    }
    fn excluded_parts(&self, agent: &str, rel: &str) -> bool {
        self.excludes
            .get(agent)
            .is_some_and(|patterns| exclude::is_excluded(patterns, rel))
    }

    pub(crate) fn begin_pass(&mut self, records: Vec<IndexRecord>) -> SessionPass<'_> {
        let local = self.snapshot();
        let (records, index) = self.project_pass_records(records);
        SessionPass {
            filesystem: self,
            local,
            index,
            records,
        }
    }

    pub(crate) fn project_status_records(
        &self,
        records: Vec<IndexRecord>,
    ) -> Vec<StatusIndexRecord> {
        self.project_records(records, false).0
    }

    fn project_pass_records(
        &self,
        records: Vec<IndexRecord>,
    ) -> (Vec<StatusIndexRecord>, HashMap<String, IndexEntry>) {
        self.project_records(records, true)
    }

    fn project_records(
        &self,
        records: Vec<IndexRecord>,
        reconcile: bool,
    ) -> (Vec<StatusIndexRecord>, HashMap<String, IndexEntry>) {
        let mut status = Vec::with_capacity(records.len());
        let mut index = HashMap::new();
        let mut malformed = HashSet::new();
        for record in records {
            let Some(key) = self.status_key(&record.key) else {
                malformed.insert(record.key);
                continue;
            };
            if reconcile && self.key_parts(&key).is_some() && !self.frozen(&key) {
                index.insert(
                    key.clone(),
                    IndexEntry {
                        head: IndexHead {
                            timestamp: record.winner_ts,
                            hash: record.winner,
                        },
                        distinct_live: record.versions.len(),
                        merge_allowed: self.append_only(&key),
                    },
                );
            }
            status.push(StatusIndexRecord {
                key,
                winner: record.winner,
                versions: record.versions,
            });
        }
        self.malformed_index_logged
            .lock()
            .unwrap()
            .retain(|key| malformed.contains(key));
        (status, index)
    }

    /// Snapshot every configured session root directly into reconcile input.
    fn snapshot(&mut self) -> Vec<LocalFile> {
        self.resolver.begin_pass();
        let mut out = Vec::new();
        let resolver = &mut self.resolver;
        let mut seen_projects = HashSet::new();
        let mut complete_roots = Vec::new();
        let mut identify_failures = HashSet::new();
        self.observed.clear();
        self.incomplete_agents.clear();
        let identify_logged = &mut self.identify_logged;
        let excludes = &self.excludes;
        for adapter in self.adapters.iter() {
            let output_start = out.len();
            let mut agent_incomplete = false;
            let mut paths = Vec::new();
            let mut complete = false;
            for _ in 0..2 {
                paths.clear();
                complete =
                    for_each_session_file(adapter.session_root(), adapter.as_ref(), |path| {
                        paths.push(path)
                    });
                if complete {
                    break;
                }
            }
            if !complete {
                self.incomplete_agents.insert(adapter.agent().to_string());
                continue;
            }
            complete_roots.push(adapter.session_root().to_path_buf());
            let mut project_files: HashMap<std::ffi::OsString, Vec<PathBuf>> = HashMap::new();
            for path in &paths {
                let Ok(relative) = path.strip_prefix(adapter.session_root()) else {
                    continue;
                };
                let Some(project) = relative.components().next() else {
                    continue;
                };
                seen_projects.insert(adapter.session_root().join(project.as_os_str()));
                project_files
                    .entry(project.as_os_str().to_owned())
                    .or_default()
                    .push(path.clone());
            }
            for path in paths {
                let Some(stamp) = file_stamp_micros(&path) else {
                    agent_incomplete = true;
                    continue;
                };
                let id = match adapter.identify(&path) {
                    Ok(id) => id,
                    Err(error) => {
                        agent_incomplete = true;
                        identify_failures.insert(path.clone());
                        if identify_logged.insert(path.clone()) {
                            eprintln!("ssync: skipping {}: {error:#}", path.display());
                        }
                        continue;
                    }
                };
                let rel = id.relative_path.to_string_lossy();
                let project = id.relative_path.components().next();
                let candidates = project
                    .and_then(|component| project_files.get(component.as_os_str()))
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let key = match resolver.wire_rel(adapter.as_ref(), &rel, candidates) {
                    Ok(None) => index_key(&id),
                    Ok(Some(mapped)) => format!("{}/{mapped}", id.agent),
                    Err(error) => {
                        let dir = path.parent().unwrap_or(&path).to_path_buf();
                        if resolver.note_failure(&id.agent, dir) {
                            eprintln!("ssync: skipping {}: {error:#}", path.display());
                        }
                        continue;
                    }
                };
                let wire_rel = key
                    .split_once('/')
                    .map(|(_, relative)| relative)
                    .unwrap_or_default();
                if excludes
                    .get(adapter.agent())
                    .is_some_and(|patterns| exclude::is_excluded(patterns, wire_rel))
                {
                    continue;
                }
                self.observed
                    .insert(key.clone(), ObservedFile { path, stamp });
                out.push(LocalFile { key, stamp });
            }
            if agent_incomplete || resolver.agent_failed(adapter.agent()) {
                self.incomplete_agents.insert(adapter.agent().to_string());
                for file in out.drain(output_start..) {
                    self.observed.remove(&file.key);
                }
            }
        }
        self.identify_logged.retain(|path| {
            !complete_roots.iter().any(|root| path.starts_with(root))
                || identify_failures.contains(path)
        });
        resolver.finish_pass(&seen_projects, &complete_roots);
        out
    }

    async fn read(&mut self, key: &str, expected: (u64, u64)) -> Result<Vec<u8>> {
        let (idx, _) = self
            .key_parts(key)
            .ok_or_else(|| anyhow!("{key}: no configured agent"))?;
        let observed = self
            .observed
            .get(key)
            .filter(|observed| observed.stamp == expected)
            .cloned()
            .ok_or_else(|| anyhow!("{key}: not present in the current filesystem pass"))?;
        ensure!(
            file_stamp_micros(&observed.path) == Some(expected),
            "{key}: session changed before import"
        );
        let bytes = tokio::fs::read(&observed.path)
            .await
            .with_context(|| format!("reading session file {}", observed.path.display()))?;
        ensure!(
            file_stamp_micros(&observed.path) == Some(expected),
            "{key}: session changed during import"
        );
        self.resolver
            .canonical_plaintext(self.adapters[idx].as_ref(), bytes)
    }

    /// Localize and atomically materialize one decrypted wire value.
    async fn write(&mut self, key: &str, plaintext: Vec<u8>) -> Result<Option<(u64, u64)>> {
        let Some((idx, rel)) = self.key_parts(key) else {
            return Ok(None);
        };
        validate_relative(rel)?;
        let adapter = self.adapters[idx].as_ref();
        let Some((dest, plaintext)) = self.resolver.localize(adapter, rel, plaintext)? else {
            return Ok(None);
        };
        let dest = contained_destination(adapter.session_root(), &dest)?;
        self.ensure_unchanged(key, &dest)?;
        atomic_write(&dest, &plaintext).await?;
        Ok(file_stamp_micros(&dest))
    }

    /// Delete one local wire value, preserving the configured symlink policy.
    async fn delete(&mut self, key: &str) -> Result<bool> {
        let Some((idx, rel)) = self.key_parts(key) else {
            return Ok(false);
        };
        validate_relative(rel)?;
        let adapter = self.adapters[idx].as_ref();
        let Some(dest) = self.resolver.local_dest_of(adapter, rel) else {
            return Ok(false);
        };
        let dest = contained_destination(adapter.session_root(), &dest)?;
        if tokio::fs::symlink_metadata(&dest)
            .await
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            self.observed.remove(key);
            return Ok(false);
        }
        self.ensure_unchanged(key, &dest)?;
        let metadata = tokio::fs::symlink_metadata(&dest)
            .await
            .with_context(|| format!("reading {}", dest.display()))?;
        ensure!(
            metadata.file_type().is_file() || metadata.file_type().is_symlink(),
            "{} is not a session file",
            dest.display()
        );
        let removed = remove_file_if_present(&dest).await?;
        self.observed.remove(key);
        crate::cleanup::remove_empty_parents(&dest, adapter.session_root());
        Ok(removed)
    }

    fn ensure_unchanged(&self, key: &str, dest: &Path) -> Result<()> {
        match self.observed.get(key) {
            Some(observed) => {
                ensure!(
                    observed.path == dest,
                    "{key}: destination changed since the filesystem pass"
                );
                ensure!(
                    file_stamp_micros(dest) == Some(observed.stamp),
                    "{key}: session changed since the filesystem pass"
                );
            }
            None => ensure!(
                std::fs::symlink_metadata(dest)
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
                "{key}: destination appeared since the filesystem pass"
            ),
        }
        Ok(())
    }
}
async fn remove_file_if_present(path: &Path) -> Result<bool> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}
fn validate_adapters(adapters: &[Box<dyn Adapter>]) -> Result<()> {
    let mut agents = HashSet::new();
    for adapter in adapters {
        ensure!(
            agents.insert(adapter.agent()),
            "duplicate configured agent {}",
            adapter.agent()
        );
    }
    for (index, adapter) in adapters.iter().enumerate() {
        for other in adapters.iter().skip(index + 1) {
            let root = adapter.session_root();
            let other_root = other.session_root();
            ensure!(
                !root.starts_with(other_root) && !other_root.starts_with(root),
                "overlapping session roots {} and {}",
                root.display(),
                other_root.display()
            );
            if let (Ok(resolved), Ok(other_resolved)) =
                (root.canonicalize(), other_root.canonicalize())
            {
                ensure!(
                    !resolved.starts_with(&other_resolved)
                        && !other_resolved.starts_with(&resolved),
                    "session roots {} and {} overlap after resolution",
                    root.display(),
                    other_root.display()
                );
            }
        }
    }
    Ok(())
}

/// The iroh-docs index key for a session: `{agent}/{relative_path}`. The
/// relative path is machine-independent and carries the write-back location,
/// so the exporter can reconstruct where the file belongs on any peer.
fn index_key(id: &SessionIdentity) -> String {
    format!("{}/{}", id.agent, id.relative_path.display())
}

/// Recursively collect session files under `root` accepted by `adapter`.
pub(crate) fn session_files(root: &Path, adapter: &dyn Adapter) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let _ = for_each_session_file(root, adapter, |path| out.push(path));
    out
}
fn for_each_session_file(
    root: &Path,
    adapter: &dyn Adapter,
    mut visit: impl FnMut(PathBuf),
) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(root_metadata) = std::fs::metadata(root) else {
        return false;
    };
    let mut complete = true;
    let mut root_chain = HashSet::new();
    root_chain.insert((root_metadata.dev(), root_metadata.ino()));
    let mut stack = vec![(root.to_path_buf(), root_chain)];
    while let Some((dir, chain)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            let path = entry.path();
            let metadata = match std::fs::metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            if metadata.is_dir() {
                let identity = (metadata.dev(), metadata.ino());
                if chain.contains(&identity) {
                    continue;
                }
                let mut child_chain = chain.clone();
                child_chain.insert(identity);
                stack.push((path, child_chain));
            } else if metadata.is_file() && adapter.is_session_file(&path) {
                visit(path);
            }
        }
    }
    complete
}

fn structural_key(key: &str) -> Option<(&str, &str)> {
    let (agent, relative) = key.split_once('/')?;
    if agent.is_empty() || validate_relative(relative).is_err() {
        return None;
    }
    Some((agent, relative))
}

fn validate_relative(rel: &str) -> Result<&Path> {
    let path = Path::new(rel);
    let mut has_component = false;
    for component in path.components() {
        ensure!(
            matches!(component, std::path::Component::Normal(_)),
            "{rel}: session path must be lexical and relative"
        );
        has_component = true;
    }
    ensure!(has_component, "session path is empty");
    Ok(path)
}

fn contained_destination(configured_root: &Path, dest: &Path) -> Result<PathBuf> {
    let relative = dest.strip_prefix(configured_root).with_context(|| {
        format!(
            "{} escapes session root {}",
            dest.display(),
            configured_root.display()
        )
    })?;
    for component in relative.components() {
        ensure!(
            matches!(component, std::path::Component::Normal(_)),
            "{} is not a lexical child of {}",
            dest.display(),
            configured_root.display()
        );
    }
    Ok(dest.to_path_buf())
}

fn file_stamp_micros(path: &Path) -> Option<(u64, u64)> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    let mtime = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_micros() as u64;
    Some((mtime, metadata.len()))
}

struct TempGuard {
    path: Option<PathBuf>,
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

async fn atomic_write(dest: &Path, data: &[u8]) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let parent = dest
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent dir", dest.display()))?;
    let mut missing = Vec::new();
    let mut cursor = parent;
    while !cursor.exists() {
        missing.push(cursor.to_path_buf());
        cursor = cursor
            .parent()
            .ok_or_else(|| anyhow!("{} has no existing ancestor", parent.display()))?;
    }
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;
    for directory in missing {
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting permissions on {}", directory.display()))?;
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut attempt = 0_u64;
    let (tmp, mut file) = loop {
        let mut name = dest.as_os_str().to_os_string();
        name.push(format!(
            ".ssync-tmp-{}-{nonce}-{attempt}",
            std::process::id()
        ));
        let tmp = PathBuf::from(name);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .await
        {
            Ok(file) => break (tmp, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                attempt = attempt
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("exhausted temporary names for {}", dest.display()))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", tmp.display()));
            }
        }
    };
    let mut guard = TempGuard {
        path: Some(tmp.clone()),
    };
    file.write_all(data)
        .await
        .with_context(|| format!("writing {}", tmp.display()))?;
    drop(file);
    tokio::fs::rename(&tmp, dest)
        .await
        .with_context(|| format!("renaming into {}", dest.display()))?;
    guard.path = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssync_adapters::blob_store::BlobStoreAdapter;
    use ssync_adapters::pi::PiAdapter;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ssync-filesystem-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn pi_filesystem(root: &Path) -> SessionFilesystem {
        SessionFilesystem::for_tests(vec![Box::new(PiAdapter::new("pi", root))])
    }

    #[test]
    fn configured_rejects_duplicate_agents_and_overlapping_roots() {
        use std::os::unix::fs::symlink;

        let base = scratch("invalid-roots");
        let nested = base.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let alias = scratch("invalid-roots-alias").join("sessions");
        symlink(&base, &alias).unwrap();
        let configured = |adapters| {
            SessionFilesystem::configured(adapters, HashMap::new(), PathMap::default(), None)
        };

        assert!(
            configured(vec![
                Box::new(PiAdapter::new("pi", &base)) as Box<dyn Adapter>,
                Box::new(PiAdapter::new("pi", scratch("other-agent-root"))),
            ])
            .is_err()
        );
        assert!(
            configured(vec![
                Box::new(PiAdapter::new("pi", &base)) as Box<dyn Adapter>,
                Box::new(PiAdapter::new("omp", &nested)),
            ])
            .is_err()
        );
        assert!(
            configured(vec![
                Box::new(PiAdapter::new("pi", &base)) as Box<dyn Adapter>,
                Box::new(PiAdapter::new("omp", &alias)),
            ])
            .is_err()
        );
    }

    #[tokio::test]
    async fn watcher_burst_coalesces_to_one_pending_wakeup() {
        let root = scratch("watcher-coalesce");
        let mut filesystem = pi_filesystem(&root);
        let mut signals = filesystem.signals().unwrap();
        let project = root.join("--project--");
        std::fs::create_dir_all(&project).unwrap();
        let path =
            project.join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl");
        for index in 0..100 {
            std::fs::write(&path, format!("{index}\n")).unwrap();
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        signals.recv().await.unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), signals.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn watcher_ignores_files_rejected_by_adapter() {
        let root = scratch("watcher-filter");
        let project = root.join("--project--");
        std::fs::create_dir_all(&project).unwrap();
        let mut filesystem = pi_filesystem(&root);
        let mut signals = filesystem.signals().unwrap();

        std::fs::write(project.join("notes.txt"), b"ignored").unwrap();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), signals.recv())
                .await
                .is_err()
        );

        std::fs::write(
            project.join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl"),
            b"session",
        )
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), signals.recv())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn snapshot_and_write_round_trip_without_map() {
        let root = scratch("roundtrip");
        let mut filesystem = pi_filesystem(&root);
        let rel = "--proj--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        let key = format!("pi/{rel}");

        filesystem.write(&key, b"session".to_vec()).await.unwrap();
        let snapshot = filesystem.snapshot();

        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].key, key);
        assert_eq!(filesystem.relative_of(&key), Some(rel));
    }

    #[test]
    fn excluded_keys_are_frozen_but_foreign_agents_are_not_excluded() {
        let root = scratch("exclude");
        let mut filesystem = pi_filesystem(&root);
        filesystem.set_excludes(HashMap::from([(
            "pi".to_string(),
            vec!["*secret*".to_string()],
        )]));
        assert!(filesystem.excluded("pi/--proj--/secret.jsonl"));
        assert!(filesystem.frozen("pi/--proj--/secret.jsonl"));
        assert!(!filesystem.excluded("pi/--proj--/s.jsonl"));
        // patterns bind per agent; a key of an unconfigured agent is not
        // excluded (it freezes via the dropped-agent guard instead)
        assert!(!filesystem.excluded("ghost/--proj--/secret.jsonl"));
    }

    #[test]
    fn dropped_agent_keys_are_frozen() {
        let root = scratch("dropped");
        let filesystem = pi_filesystem(&root);
        assert!(filesystem.frozen("ghost/--proj--/s.jsonl"));
        assert!(!filesystem.frozen("pi/--proj--/s.jsonl"));
    }

    #[test]
    fn mapping_failure_withholds_tombstones_for_the_snapshot_pass() {
        let root = scratch("freeze");
        let mut filesystem = pi_filesystem(&root);
        filesystem.set_path_map(
            PathMap::new(vec![("/data".into(), "/canon".into())]).unwrap(),
            None,
        );
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl");
        std::fs::write(&path, b"{\"type\":\"session\",\"version\":3}\n").unwrap();

        assert!(filesystem.snapshot().is_empty());
        assert!(filesystem.tombstone_withheld("pi/--proj--/other.jsonl"));
        std::fs::remove_file(path).unwrap();
        assert!(filesystem.snapshot().is_empty());
        assert!(!filesystem.tombstone_withheld("pi/--proj--/other.jsonl"));
    }

    #[tokio::test]
    async fn mapped_session_round_trips_through_the_filesystem_interface() {
        let root = scratch("mapped-roundtrip");
        let dir = root.join("--data-Projects-x--");
        std::fs::create_dir_all(&dir).unwrap();
        let name = "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        let path = dir.join(name);
        std::fs::write(
            &path,
            b"{\"type\":\"session\",\"version\":3,\"cwd\":\"/data/Projects/x\"}\n",
        )
        .unwrap();
        let mut filesystem = pi_filesystem(&root);
        filesystem.set_path_map(
            PathMap::new(vec![("/data/Projects".into(), "/canon/Projects".into())]).unwrap(),
            None,
        );

        let snapshot = filesystem.snapshot();
        assert_eq!(snapshot.len(), 1);
        let key = format!("pi/--canon-Projects-x--/{name}");
        assert_eq!(snapshot[0].key, key);
        let wire_bytes = filesystem.read(&key, snapshot[0].stamp).await.unwrap();
        assert!(String::from_utf8_lossy(&wire_bytes).contains("\"cwd\":\"/canon/Projects/x\""));

        std::fs::remove_file(&path).unwrap();
        filesystem.snapshot();
        filesystem.write(&key, wire_bytes).await.unwrap();
        let local_bytes = std::fs::read(path).unwrap();
        assert!(String::from_utf8_lossy(&local_bytes).contains("\"cwd\":\"/data/Projects/x\""));
    }

    #[tokio::test]
    async fn pass_operations_retry_after_concurrent_local_changes() {
        let root = scratch("concurrent-change");
        let project = root.join("--project--");
        std::fs::create_dir_all(&project).unwrap();
        let path =
            project.join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl");
        std::fs::write(&path, b"old").unwrap();
        let mut filesystem = pi_filesystem(&root);
        let snapshot = filesystem.snapshot();
        let key = snapshot[0].key.clone();

        std::fs::write(&path, b"new-content").unwrap();

        assert!(filesystem.read(&key, snapshot[0].stamp).await.is_err());
        assert!(filesystem.delete(&key).await.is_err());

        std::fs::remove_file(&path).unwrap();
        filesystem.snapshot();
        std::fs::write(&path, b"appeared").unwrap();
        assert!(filesystem.write(&key, b"remote".to_vec()).await.is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"appeared");
    }

    #[tokio::test]
    async fn remove_file_if_present_treats_not_found_as_success() {
        let path = scratch("idempotent-remove").join("session.jsonl");
        assert!(!remove_file_if_present(&path).await.unwrap());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"session").unwrap();
        assert!(remove_file_if_present(&path).await.unwrap());
    }

    #[test]
    fn missing_omp_canonical_home_freezes_the_snapshot_pass() {
        let root = scratch("omp-home");
        let dir = root.join("-Projects-x");
        std::fs::create_dir_all(&dir).unwrap();
        let name = "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        std::fs::write(
            dir.join(name),
            b"{\"type\":\"session\",\"version\":3,\"cwd\":\"/data/Projects/x\"}\n",
        )
        .unwrap();
        let mut filesystem =
            SessionFilesystem::for_tests(vec![Box::new(PiAdapter::new("omp", &root))]);
        filesystem.set_path_map(
            PathMap::new(vec![("/data".into(), "/canon-home".into())]).unwrap(),
            None,
        );

        assert!(filesystem.snapshot().is_empty());
        assert!(filesystem.tombstone_withheld("omp/-Projects-x/other.jsonl"));
    }

    #[test]
    fn one_mapping_failure_discards_the_agents_complete_staged_inventory() {
        let root = scratch("mapping-freeze");
        let valid_dir = root.join("--data-Projects-valid--");
        let invalid_dir = root.join("--data-Projects-invalid--");
        std::fs::create_dir_all(&valid_dir).unwrap();
        std::fs::create_dir_all(&invalid_dir).unwrap();
        let valid_name = "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        let invalid_name = "2026-05-23T06-55-21-772Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl";
        std::fs::write(
            valid_dir.join(valid_name),
            b"{\"type\":\"session\",\"version\":3,\"cwd\":\"/data/Projects/valid\"}\n",
        )
        .unwrap();
        std::fs::write(invalid_dir.join(invalid_name), b"not a session header\n").unwrap();
        let mut filesystem = pi_filesystem(&root);
        filesystem.set_path_map(
            PathMap::new(vec![("/data/Projects".into(), "/canon/Projects".into())]).unwrap(),
            None,
        );

        let snapshot = filesystem.snapshot();

        assert!(snapshot.is_empty());
        assert!(filesystem.frozen(&format!("pi/--canon-Projects-valid--/{valid_name}")));
        assert!(filesystem.state_retained(&format!("pi/--canon-Projects-valid--/{valid_name}")));
    }

    #[test]
    fn unreadable_entry_freezes_the_agent() {
        let root = scratch("scan-metadata-failure");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(root.join("missing"), root.join("broken.jsonl")).unwrap();
        let mut filesystem = pi_filesystem(&root);

        assert!(filesystem.snapshot().is_empty());
        assert!(!filesystem.pass_complete());
    }

    #[test]
    fn unidentified_session_file_freezes_the_agent() {
        let root = scratch("scan-identify-failure");
        let project = root.join("--project--");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("missing-session-id.jsonl"), b"session").unwrap();
        let mut filesystem = pi_filesystem(&root);

        assert!(filesystem.snapshot().is_empty());
        assert!(!filesystem.pass_complete());
    }

    #[test]
    fn resolver_diagnostics_and_mappings_follow_visible_projects() {
        let root = scratch("resolver-cache");
        let project = root.join("--data-Projects-x--");
        std::fs::create_dir_all(&project).unwrap();
        let name = "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        let path = project.join(name);
        std::fs::write(&path, b"not a session header\n").unwrap();
        let mut filesystem = pi_filesystem(&root);
        filesystem.set_path_map(
            PathMap::new(vec![("/data/Projects".into(), "/canon/Projects".into())]).unwrap(),
            None,
        );

        assert!(filesystem.snapshot().is_empty());
        assert_eq!(filesystem.resolver.cache_sizes(), (0, 1));

        std::fs::write(
            &path,
            b"{\"type\":\"session\",\"version\":3,\"cwd\":\"/data/Projects/x\"}\n",
        )
        .unwrap();
        assert_eq!(filesystem.snapshot().len(), 1);
        assert_eq!(filesystem.resolver.cache_sizes(), (1, 0));

        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(project).unwrap();
        assert!(filesystem.snapshot().is_empty());
        assert_eq!(filesystem.resolver.cache_sizes(), (0, 0));
    }

    #[test]
    fn mapped_exclude_filters_the_wire_key_from_the_local_snapshot() {
        let root = scratch("mapped-exclude");
        let dir = root.join("--data-Projects-x--");
        std::fs::create_dir_all(&dir).unwrap();
        let name = "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        std::fs::write(
            dir.join(name),
            b"{\"type\":\"session\",\"version\":3,\"cwd\":\"/data/Projects/x\"}\n",
        )
        .unwrap();
        let mut filesystem = pi_filesystem(&root);
        filesystem.set_path_map(
            PathMap::new(vec![("/data/Projects".into(), "/canon/Projects".into())]).unwrap(),
            None,
        );
        filesystem.set_excludes(HashMap::from([(
            "pi".to_string(),
            vec!["*canon-Projects-x*".to_string()],
        )]));

        assert!(filesystem.snapshot().is_empty());
        assert!(filesystem.frozen(&format!("pi/--canon-Projects-x--/{name}")));
    }

    #[test]
    fn append_only_follows_the_owning_adapter() {
        let root = scratch("append");
        let blob_root = scratch("append-blobs");
        let filesystem = SessionFilesystem::for_tests(vec![
            Box::new(PiAdapter::new("pi", &root)),
            Box::new(BlobStoreAdapter::new("omp-blobs", &blob_root)),
        ]);
        assert!(filesystem.append_only("pi/--proj--/s.jsonl"));
        assert!(!filesystem.append_only("omp-blobs/abc123"));
        assert!(!filesystem.append_only("ghost/x"));
    }

    #[test]
    fn is_session_file_requires_an_owning_root() {
        let root = scratch("owning");
        let filesystem = pi_filesystem(&root);
        assert!(filesystem.is_session_file(&root.join("--proj--/s.jsonl")));
        assert!(!filesystem.is_session_file(Path::new("/elsewhere/--proj--/s.jsonl")));
    }

    #[test]
    fn scan_preserves_symlink_aliases_and_terminates_cycles() {
        use std::os::unix::fs::symlink;

        let base = scratch("symlinks");
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(root.join("--project--")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(root.join("--project--/one_session.jsonl"), b"one").unwrap();
        std::fs::write(outside.join("outside_session.jsonl"), b"outside").unwrap();
        symlink(&outside, root.join("--project--/linked-dir")).unwrap();
        symlink(
            outside.join("outside_session.jsonl"),
            root.join("--project--/linked-file.jsonl"),
        )
        .unwrap();
        symlink(&root, outside.join("cycle")).unwrap();
        symlink(
            outside.join("missing.jsonl"),
            root.join("--project--/dangling.jsonl"),
        )
        .unwrap();

        let adapter = PiAdapter::new("pi", &root);
        let files: HashSet<_> = session_files(&root, &adapter).into_iter().collect();

        assert_eq!(files.len(), 3);
        assert!(files.contains(&root.join("--project--/one_session.jsonl")));
        assert!(files.contains(&root.join("--project--/linked-file.jsonl")));
        assert!(files.contains(&root.join("--project--/linked-dir/outside_session.jsonl")));
    }

    #[tokio::test]
    async fn mapped_artifact_retries_until_main_session_learns_its_project_dir() {
        let root = scratch("mapped-artifact");
        let mut filesystem = pi_filesystem(&root);
        filesystem.set_path_map(
            PathMap::new(vec![("/data".into(), "/canon".into())]).unwrap(),
            None,
        );
        let main_key = "pi/--canon-Projects-x--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        let artifact_key = "pi/--canon-Projects-x--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a/advisor.jsonl";

        assert!(
            filesystem
                .write(artifact_key, b"{\"type\":\"advisor\"}\n".to_vec())
                .await
                .unwrap()
                .is_none()
        );
        filesystem
            .write(
                main_key,
                b"{\"type\":\"session\",\"version\":3,\"cwd\":\"/canon/Projects/x\"}\n".to_vec(),
            )
            .await
            .unwrap()
            .expect("main session must establish the project translation");
        assert!(
            filesystem
                .write(artifact_key, b"{\"type\":\"advisor\"}\n".to_vec())
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            root.join(
                "--data-Projects-x--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a/advisor.jsonl"
            )
            .exists()
        );
    }

    #[test]
    fn oversized_header_is_a_retriable_mapping_skip() {
        let root = scratch("header-budget");
        let project = root.join("--project--");
        std::fs::create_dir_all(&project).unwrap();
        let name = "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        let mut bytes = vec![b'x'; 64 * 1024];
        bytes.extend_from_slice(b"\n{\"type\":\"session\",\"cwd\":\"/work/project\"}\n");
        std::fs::write(project.join(name), bytes).unwrap();
        let mut filesystem = pi_filesystem(&root);
        filesystem.set_path_map(
            PathMap::new(vec![("/work".into(), "/canonical".into())]).unwrap(),
            None,
        );

        assert!(filesystem.snapshot().is_empty());
        assert!(filesystem.tombstone_withheld("pi/--project--/other.jsonl"));
    }

    #[tokio::test]
    async fn access_rejects_parent_components_but_preserves_symlinked_ancestors() {
        use std::os::unix::fs::symlink;

        let root = scratch("contained-write");
        let outside = scratch("contained-write-outside");
        let mut filesystem = pi_filesystem(&root);

        assert!(
            filesystem
                .write("pi/../escape.jsonl", b"escape".to_vec())
                .await
                .is_err()
        );
        symlink(&outside, root.join("--project--")).unwrap();
        filesystem
            .write("pi/--project--/session_id.jsonl", b"written".to_vec())
            .await
            .unwrap();
        let snapshot = filesystem.snapshot();
        assert_eq!(
            filesystem
                .read("pi/--project--/session_id.jsonl", snapshot[0].stamp,)
                .await
                .unwrap(),
            b"written"
        );
        assert_eq!(
            std::fs::read(outside.join("session_id.jsonl")).unwrap(),
            b"written"
        );
    }

    #[tokio::test]
    async fn write_does_not_follow_existing_temporary_symlink() {
        use std::os::unix::fs::symlink;

        let root = scratch("temporary-symlink");
        let outside = scratch("temporary-symlink-outside");
        let project = root.join("--project--");
        std::fs::create_dir_all(&project).unwrap();
        let outside_file = outside.join("user-file");
        std::fs::write(&outside_file, b"outside").unwrap();
        symlink(&outside_file, project.join("session_id.ssync-tmp")).unwrap();
        let mut filesystem = pi_filesystem(&root);

        filesystem
            .write("pi/--project--/session_id.jsonl", b"session".to_vec())
            .await
            .unwrap()
            .expect("write must resolve");

        assert_eq!(std::fs::read(outside_file).unwrap(), b"outside");
        assert_eq!(
            std::fs::read(project.join("session_id.jsonl")).unwrap(),
            b"session"
        );
    }

    #[tokio::test]
    async fn write_and_delete_are_atomic_contained_mutations() {
        let root = scratch("atomic-mutation");
        let mut filesystem = pi_filesystem(&root);
        let key = "pi/--project--/session_id.jsonl";

        let stamp = filesystem
            .write(key, b"session".to_vec())
            .await
            .unwrap()
            .expect("write must resolve");
        assert_eq!(
            std::fs::read(root.join("--project--/session_id.jsonl")).unwrap(),
            b"session"
        );
        assert_eq!(stamp.1, 7);
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        assert_eq!(
            std::fs::metadata(root.join("--project--"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(root.join("--project--/session_id.jsonl"))
                .unwrap()
                .mode()
                & 0o777,
            0o600
        );
        filesystem.snapshot();
        assert!(filesystem.delete(key).await.unwrap());
        assert!(!root.join("--project--").exists());
    }
}
