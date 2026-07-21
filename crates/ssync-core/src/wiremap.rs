//! The wire-map: every translation between wire keys (`{agent}/{relative_path}`
//! index keys) and local session paths, plus the freeze verdicts that keep a
//! skipped key from ever reading as a deletion. Owns the adapters, the
//! per-agent excludes (#14), and the path-map [`Resolver`] (#13/#49); the
//! engine holds one `Wiremap` and never touches adapters directly.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use ssync_adapters::{Adapter, SessionIdentity};

use crate::exclude;
use crate::pathmap::{PathMap, Resolver};

/// Result of resolving a local session to its wire key.
pub(crate) enum KeyLookup {
    Key(String),
    /// Retriable per-key skip (#49); `Some` is the first announcement.
    Skipped(Option<anyhow::Error>),
}

/// Wire key ↔ local path translation and freeze state for one engine.
pub(crate) struct Wiremap {
    adapters: Vec<Box<dyn Adapter>>,
    /// Per-agent session exclusion patterns (issue #14); a matching key is
    /// invisible to reconcile from both sides, freezing it everywhere.
    excludes: HashMap<String, Vec<String>>,
    /// Wire↔local path-map translation (issue #13, map #42); default = inert.
    resolver: Resolver,
}

impl Wiremap {
    pub fn new(adapters: Vec<Box<dyn Adapter>>) -> Self {
        Self {
            adapters,
            excludes: HashMap::new(),
            resolver: Resolver::default(),
        }
    }

    /// Per-agent `exclude` patterns from config (`[[agents]]` tables).
    pub fn set_excludes(&mut self, excludes: HashMap<String, Vec<String>>) {
        self.excludes = excludes;
    }

    /// The `[[path_map]]` + `canonical_home` from config (issue #13).
    pub fn set_path_map(&mut self, map: PathMap, canonical_home: Option<PathBuf>) {
        self.resolver = Resolver::new(map, canonical_home);
    }

    /// Start a snapshot pass: last pass's mapping failures no longer freeze.
    pub fn begin_pass(&mut self) {
        self.resolver.begin_pass();
    }

    /// Every configured session root (watch targets).
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.adapters.iter().map(|a| a.session_root())
    }

    /// Whether `path` is a session file of the adapter whose root contains it.
    pub fn is_session_file(&self, path: &Path) -> bool {
        self.adapter_of_path(path)
            .is_some_and(|a| a.is_session_file(path))
    }

    /// Session files under every configured adapter's root.
    pub fn session_files(&self) -> Vec<PathBuf> {
        self.adapters
            .iter()
            .flat_map(|a| session_files(a.session_root(), a.as_ref()))
            .collect()
    }

    /// Identify a path via the adapter whose session root contains it.
    pub fn identify(&self, path: &Path) -> Result<SessionIdentity> {
        self.adapter_of_path(path)
            .ok_or_else(|| anyhow!("{} is under no configured session root", path.display()))?
            .identify(path)
    }

    /// The wire key for a local file (issue #13): translated through the path
    /// map when its project dir is mapped, today's relative path otherwise.
    /// Mapping failures are retriable per-key skips (#49).
    pub fn key_of(&mut self, id: &SessionIdentity, path: &Path) -> KeyLookup {
        let Some(idx) = self.adapter_index_of_path(path) else {
            return KeyLookup::Key(index_key(id));
        };
        let rel = id.relative_path.to_string_lossy();
        match self.resolver.wire_rel(self.adapters[idx].as_ref(), &rel) {
            Ok(None) => KeyLookup::Key(index_key(id)),
            Ok(Some(mapped)) => KeyLookup::Key(format!("{}/{mapped}", id.agent)),
            Err(error) => {
                let dir = path.parent().unwrap_or(path).to_path_buf();
                let announcement = self.resolver.note_failure(&id.agent, dir).then_some(error);
                KeyLookup::Skipped(announcement)
            }
        }
    }

    /// Local destination + on-disk bytes for a wire key's plaintext, through
    /// the path map. `Ok(None)` = not resolvable yet — the caller returns
    /// false and a later tick retries.
    pub fn localize(
        &mut self,
        key: &str,
        plaintext: Vec<u8>,
    ) -> Result<Option<(PathBuf, Vec<u8>)>> {
        let Some((idx, rel)) = self.key_parts(key) else {
            return Ok(None);
        };
        let adapter = self.adapters[idx].as_ref();
        self.resolver.localize(adapter, rel, plaintext)
    }

    /// Local destination through the path map for actions without plaintext
    /// in hand (DeleteLocal). `None` = nothing local to touch.
    pub fn local_dest_of(&self, key: &str) -> Option<PathBuf> {
        let (idx, rel) = self.key_parts(key)?;
        self.resolver
            .local_dest_of(self.adapters[idx].as_ref(), rel)
    }

    /// The wire form of a local file's bytes: header cwd mapped to canonical
    /// (issue #13). A machine-local path must never reach the index — an
    /// unmappable header is a per-key error, not a pass-through (#49).
    pub fn canonical_plaintext(&self, key: &str, bytes: Vec<u8>) -> Result<Vec<u8>> {
        let Some(adapter) = self.adapter_of_key(key) else {
            return Ok(bytes);
        };
        self.resolver.canonical_plaintext(adapter, bytes)
    }

    /// The session root of the adapter owning `key` (empty-parent sweep after
    /// DeleteLocal).
    pub fn session_root_of(&self, key: &str) -> Option<&Path> {
        self.adapter_of_key(key).map(|a| a.session_root())
    }

    /// Decode an index key back to its session-root-relative path (the
    /// inverse of the key encoding): strip the `{agent}/` prefix of a
    /// configured adapter.
    pub fn relative_of<'a>(&self, key: &'a str) -> Option<&'a str> {
        self.key_parts(key).map(|(_, rel)| rel)
    }

    /// The session identity an index key resolves to on this machine, if a
    /// configured adapter can parse its destination path.
    pub fn session_identity_of_key(&self, key: &str) -> Option<SessionIdentity> {
        let (idx, rel) = self.key_parts(key)?;
        let adapter = self.adapters[idx].as_ref();
        adapter.identify(&adapter.session_root().join(rel)).ok()
    }

    /// Whether a key falls under its agent's `exclude` patterns (issue #14).
    /// Filtered out of BOTH reconcile inputs, so the key is frozen: never
    /// imported, exported, tombstoned, or merged — here or on write-back.
    pub fn excluded(&self, key: &str) -> bool {
        let Some((idx, rel)) = self.key_parts(key) else {
            return false;
        };
        self.excluded_parts(self.adapters[idx].agent(), rel)
    }

    /// Whether a key is invisible to reconcile: excluded (#14) or owned by no
    /// configured adapter (dropped-agent guard — a removed `[[agents]]` entry
    /// must never tombstone peers' sessions).
    pub fn frozen(&self, key: &str) -> bool {
        let Some((idx, rel)) = self.key_parts(key) else {
            return true;
        };
        self.excluded_parts(self.adapters[idx].agent(), rel)
    }

    /// Whether a tombstone for this key must be withheld this pass: its
    /// agent's mapping failed (#49), so "file gone" is an artifact of the
    /// skip, not a delete.
    pub fn tombstone_withheld(&self, key: &str) -> bool {
        self.adapter_of_key(key)
            .is_some_and(|a| self.resolver.agent_failed(a.agent()))
    }

    /// Whether the key's format merges (append-only line union) rather than
    /// newest-wins. `false` for keys of unconfigured agents.
    pub fn append_only(&self, key: &str) -> bool {
        self.adapter_of_key(key).is_some_and(|a| a.append_only())
    }

    /// The adapter owning an index key (matching `{agent}/` prefix), if any —
    /// peers may sync agents this node does not have configured.
    fn adapter_of_key(&self, key: &str) -> Option<&dyn Adapter> {
        let (idx, _) = self.key_parts(key)?;
        Some(self.adapters[idx].as_ref())
    }

    /// The adapter whose session root contains `path`.
    fn adapter_of_path(&self, path: &Path) -> Option<&dyn Adapter> {
        self.adapter_index_of_path(path)
            .map(|i| self.adapters[i].as_ref())
    }

    fn adapter_index_of_path(&self, path: &Path) -> Option<usize> {
        self.adapters
            .iter()
            .position(|a| path.starts_with(a.session_root()))
    }

    fn key_parts<'a>(&self, key: &'a str) -> Option<(usize, &'a str)> {
        self.adapters.iter().enumerate().find_map(|(idx, adapter)| {
            key.strip_prefix(adapter.agent())
                .and_then(|rest| rest.strip_prefix('/'))
                .map(|rel| (idx, rel))
        })
    }

    fn excluded_parts(&self, agent: &str, rel: &str) -> bool {
        self.excludes
            .get(agent)
            .is_some_and(|patterns| exclude::is_excluded(patterns, rel))
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use ssync_adapters::blob_store::BlobStoreAdapter;
    use ssync_adapters::pi::PiAdapter;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ssync-wiremap-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn pi_map(root: &Path) -> Wiremap {
        Wiremap::new(vec![Box::new(PiAdapter::new("pi", root))])
    }

    fn id(agent: &str, rel: &str) -> SessionIdentity {
        SessionIdentity {
            agent: agent.into(),
            session_id: "s".into(),
            project_id: "p".into(),
            relative_path: rel.into(),
        }
    }

    #[test]
    fn key_and_dest_round_trip_without_map() {
        let root = scratch("roundtrip");
        let mut wm = pi_map(&root);
        let rel = "--proj--/s.jsonl";
        let KeyLookup::Key(key) = wm.key_of(&id("pi", rel), &root.join(rel)) else {
            panic!("key lookup skipped");
        };
        assert_eq!(key, format!("pi/{rel}"));
        assert_eq!(wm.local_dest_of(&key), Some(root.join(rel)));
        assert_eq!(wm.relative_of(&key), Some(rel));
    }

    #[test]
    fn excluded_keys_are_frozen_but_foreign_agents_are_not_excluded() {
        let root = scratch("exclude");
        let mut wm = pi_map(&root);
        wm.set_excludes(HashMap::from([(
            "pi".to_string(),
            vec!["*secret*".to_string()],
        )]));
        assert!(wm.excluded("pi/--proj--/secret.jsonl"));
        assert!(wm.frozen("pi/--proj--/secret.jsonl"));
        assert!(!wm.excluded("pi/--proj--/s.jsonl"));
        // patterns bind per agent; a key of an unconfigured agent is not
        // excluded (it freezes via the dropped-agent guard instead)
        assert!(!wm.excluded("ghost/--proj--/secret.jsonl"));
    }

    #[test]
    fn dropped_agent_keys_are_frozen() {
        let root = scratch("dropped");
        let wm = pi_map(&root);
        assert!(wm.frozen("ghost/--proj--/s.jsonl"));
        assert!(!wm.frozen("pi/--proj--/s.jsonl"));
        assert_eq!(wm.local_dest_of("ghost/--proj--/s.jsonl"), None);
    }

    #[test]
    fn mapping_failure_withholds_tombstones_and_announces_once() {
        let root = scratch("freeze");
        let mut wm = pi_map(&root);
        wm.set_path_map(
            PathMap::new(vec![("/data".into(), "/canon".into())]).unwrap(),
            None,
        );
        // project dir exists but holds no session header to resolve the cwd
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        let rel = "--proj--/s.jsonl";
        let sid = id("pi", rel);
        let path = root.join(rel);

        wm.begin_pass();
        let KeyLookup::Skipped(first) = wm.key_of(&sid, &path) else {
            panic!("mapping unexpectedly succeeded");
        };
        assert!(first.is_some(), "first failure logs");
        assert!(wm.tombstone_withheld("pi/--proj--/other.jsonl"));
        let KeyLookup::Skipped(second) = wm.key_of(&sid, &path) else {
            panic!("mapping unexpectedly succeeded");
        };
        assert!(second.is_none(), "repeat failure stays quiet");
        // the freeze lasts one pass; the log-once memory does not reset
        wm.begin_pass();
        assert!(!wm.tombstone_withheld("pi/--proj--/other.jsonl"));
    }

    #[test]
    fn append_only_follows_the_owning_adapter() {
        let root = scratch("append");
        let blob_root = scratch("append-blobs");
        let wm = Wiremap::new(vec![
            Box::new(PiAdapter::new("pi", &root)),
            Box::new(BlobStoreAdapter::new("omp-blobs", &blob_root)),
        ]);
        assert!(wm.append_only("pi/--proj--/s.jsonl"));
        assert!(!wm.append_only("omp-blobs/abc123"));
        assert!(!wm.append_only("ghost/x"));
    }

    #[test]
    fn is_session_file_requires_an_owning_root() {
        let root = scratch("owning");
        let wm = pi_map(&root);
        assert!(wm.is_session_file(&root.join("--proj--/s.jsonl")));
        assert!(!wm.is_session_file(Path::new("/elsewhere/--proj--/s.jsonl")));
    }
}
