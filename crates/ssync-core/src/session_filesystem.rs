//! Session filesystem policy: bounded discovery, wire-key ↔ local-path
//! translation, path-map resolution, freeze verdicts, and contained mutation.
//! Owns every adapter and every filesystem invariant around its session root.

mod pathmap;

pub use pathmap::PathMap;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use ssync_adapters::{Adapter, SessionIdentity};
use crate::reconcile::LocalFile;

use crate::exclude;
use pathmap::Resolver;


/// Session discovery, translation, policy, and mutation for one engine.
pub(crate) struct SessionFilesystem {
    adapters: Vec<Box<dyn Adapter>>,
    /// Per-agent session exclusion patterns (issue #14); a matching key is
    /// invisible to reconcile from both sides, freezing it everywhere.
    excludes: HashMap<String, Vec<String>>,
    /// Wire↔local path-map translation (issue #13, map #42); default = inert.
    resolver: Resolver,
    resolved_roots: Vec<Option<PathBuf>>,
    identify_logged: HashSet<PathBuf>,
}

impl SessionFilesystem {
    pub fn new(adapters: Vec<Box<dyn Adapter>>) -> Self {
        let root_count = adapters.len();
        Self {
            adapters,
            excludes: HashMap::new(),
            resolver: Resolver::default(),
            resolved_roots: (0..root_count).map(|_| None).collect(),
            identify_logged: HashSet::new(),
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


    /// Every configured session root (watch targets).
    pub fn roots(&self) -> impl Iterator<Item = &Path> {
        self.adapters.iter().map(|a| a.session_root())
    }

    /// Whether `path` is a session file of the adapter whose root contains it.
    pub fn is_session_file(&self, path: &Path) -> bool {
        self.adapter_of_path(path)
            .is_some_and(|a| a.is_session_file(path))
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
    /// Snapshot every configured session root directly into reconcile input.
    pub fn snapshot(&mut self) -> Vec<LocalFile> {
        self.resolver.begin_pass();
        let mut out = Vec::new();
        let resolver = &mut self.resolver;
        let identify_logged = &mut self.identify_logged;
        let excludes = &self.excludes;
        for adapter in &self.adapters {
            for_each_session_file(adapter.session_root(), adapter.as_ref(), |path| {
                let Some(stamp) = file_stamp_micros(&path) else {
                    return;
                };
                let id = match adapter.identify(&path) {
                    Ok(id) => id,
                    Err(error) => {
                        if identify_logged.insert(path.clone()) {
                            eprintln!("ssync: skipping {}: {error:#}", path.display());
                        }
                        return;
                    }
                };
                let rel = id.relative_path.to_string_lossy();
                let key = match resolver.wire_rel(adapter.as_ref(), &rel) {
                    Ok(None) => index_key(&id),
                    Ok(Some(mapped)) => format!("{}/{mapped}", id.agent),
                    Err(error) => {
                        let dir = path.parent().unwrap_or(&path).to_path_buf();
                        if resolver.note_failure(&id.agent, dir) {
                            eprintln!("ssync: skipping {}: {error:#}", path.display());
                        }
                        return;
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
                    return;
                }
                out.push(LocalFile { key, path, stamp });
            });
        }
        out
    }

    pub async fn read(&mut self, key: &str, path: &Path) -> Result<Vec<u8>> {
        let (idx, _) = self
            .key_parts(key)
            .ok_or_else(|| anyhow!("{key}: no configured agent"))?;
        let configured_root = self.adapters[idx].session_root().to_path_buf();
        let root = self.resolved_root(idx)?;
        let path = contained_destination(&configured_root, &root, path)?;
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading session file {}", path.display()))?;
        self.resolver
            .canonical_plaintext(self.adapters[idx].as_ref(), bytes)
    }


    /// Localize and atomically materialize one decrypted wire value.
    pub async fn write(&mut self, key: &str, plaintext: Vec<u8>) -> Result<Option<(u64, u64)>> {
        let Some((idx, rel)) = self.key_parts(key) else {
            return Ok(None);
        };
        validate_relative(rel)?;
        let root = self.resolved_root(idx)?;
        let adapter = self.adapters[idx].as_ref();
        let Some((dest, plaintext)) = self.resolver.localize(adapter, rel, plaintext)? else {
            return Ok(None);
        };
        let dest = contained_destination(adapter.session_root(), &root, &dest)?;
        atomic_write(&dest, &plaintext).await?;
        Ok(file_stamp_micros(&dest))
    }

    /// Delete one local wire value without following path-map or filesystem
    /// links outside its configured session root.
    pub async fn delete(&mut self, key: &str) -> Result<bool> {
        let Some((idx, rel)) = self.key_parts(key) else {
            return Ok(false);
        };
        validate_relative(rel)?;
        let root = self.resolved_root(idx)?;
        let adapter = self.adapters[idx].as_ref();
        let Some(dest) = self.resolver.local_dest_of(adapter, rel) else {
            return Ok(false);
        };
        let dest = contained_destination(adapter.session_root(), &root, &dest)?;
        let metadata = match tokio::fs::symlink_metadata(&dest).await {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", dest.display()));
            }
        };
        ensure!(
            metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
            "{} is not a regular session file",
            dest.display()
        );
        tokio::fs::remove_file(&dest)
            .await
            .with_context(|| format!("removing {}", dest.display()))?;
        crate::cleanup::remove_empty_parents(&dest, &root);
        Ok(true)
    }

    fn resolved_root(&mut self, idx: usize) -> Result<PathBuf> {
        if self.resolved_roots[idx].is_none() {
            let root = self.adapters[idx].session_root();
            self.resolved_roots[idx] = Some(
                root.canonicalize()
                    .with_context(|| format!("resolving session root {}", root.display()))?,
            );
        }
        Ok(self.resolved_roots[idx]
            .as_ref()
            .expect("resolved above")
            .clone())
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
    for_each_session_file(root, adapter, |path| out.push(path));
    out
}

fn for_each_session_file(
    root: &Path,
    adapter: &dyn Adapter,
    mut visit: impl FnMut(PathBuf),
) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file() && adapter.is_session_file(&path) {
                visit(path);
            }
        }
    }
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

fn contained_destination(configured_root: &Path, root: &Path, dest: &Path) -> Result<PathBuf> {
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
    let contained = root.join(relative);
    let parent = contained
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent dir", contained.display()))?;
    let mut ancestor = root.to_path_buf();
    for component in parent
        .strip_prefix(root)
        .expect("contained parent")
        .components()
    {
        ancestor.push(component);
        match std::fs::symlink_metadata(&ancestor) {
            Ok(metadata) => ensure!(
                metadata.is_dir() && !metadata.file_type().is_symlink(),
                "{} is not a real directory",
                ancestor.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", ancestor.display()));
            }
        }
    }
    if let Ok(metadata) = std::fs::symlink_metadata(&contained) {
        ensure!(
            !metadata.file_type().is_symlink(),
            "{} is a symlink",
            contained.display()
        );
    }
    Ok(contained)
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

async fn atomic_write(dest: &Path, data: &[u8]) -> Result<()> {
    let parent = dest
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent dir", dest.display()))?;
    tokio::fs::create_dir_all(parent)
        .await
        .with_context(|| format!("creating {}", parent.display()))?;
    let tmp = dest.with_extension("ssync-tmp");
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
    use ssync_adapters::blob_store::BlobStoreAdapter;
    use ssync_adapters::pi::PiAdapter;

    fn scratch(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("ssync-filesystem-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn pi_map(root: &Path) -> SessionFilesystem {
        SessionFilesystem::new(vec![Box::new(PiAdapter::new("pi", root))])
    }


    #[tokio::test]
    async fn snapshot_and_write_round_trip_without_map() {
        let root = scratch("roundtrip");
        let mut filesystem = pi_map(&root);
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
    }

    #[test]
    fn mapping_failure_withholds_tombstones_for_the_snapshot_pass() {
        let root = scratch("freeze");
        let mut filesystem = pi_map(&root);
        filesystem.set_path_map(
            PathMap::new(vec![("/data".into(), "/canon".into())]).unwrap(),
            None,
        );
        let dir = root.join("--proj--");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(
            "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl",
        );
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
        let name =
            "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        let path = dir.join(name);
        std::fs::write(
            &path,
            b"{\"type\":\"session\",\"version\":3,\"cwd\":\"/data/Projects/x\"}\n",
        )
        .unwrap();
        let mut filesystem = pi_map(&root);
        filesystem.set_path_map(
            PathMap::new(vec![
                ("/data/Projects".into(), "/canon/Projects".into()),
            ])
            .unwrap(),
            None,
        );

        let snapshot = filesystem.snapshot();
        assert_eq!(snapshot.len(), 1);
        let key = format!("pi/--canon-Projects-x--/{name}");
        assert_eq!(snapshot[0].key, key);
        let wire_bytes = filesystem.read(&key, &path).await.unwrap();
        assert!(String::from_utf8_lossy(&wire_bytes).contains("\"cwd\":\"/canon/Projects/x\""));

        std::fs::remove_file(&path).unwrap();
        filesystem.write(&key, wire_bytes).await.unwrap();
        let local_bytes = std::fs::read(path).unwrap();
        assert!(String::from_utf8_lossy(&local_bytes).contains("\"cwd\":\"/data/Projects/x\""));
    }

    #[test]
    fn missing_omp_canonical_home_freezes_the_snapshot_pass() {
        let root = scratch("omp-home");
        let dir = root.join("-Projects-x");
        std::fs::create_dir_all(&dir).unwrap();
        let name =
            "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        std::fs::write(
            dir.join(name),
            b"{\"type\":\"session\",\"version\":3,\"cwd\":\"/data/Projects/x\"}\n",
        )
        .unwrap();
        let mut filesystem = SessionFilesystem::new(vec![Box::new(PiAdapter::new(
            "omp", &root,
        ))]);
        filesystem.set_path_map(
            PathMap::new(vec![("/data".into(), "/canon-home".into())]).unwrap(),
            None,
        );

        assert!(filesystem.snapshot().is_empty());
        assert!(filesystem.tombstone_withheld("omp/-Projects-x/other.jsonl"));
    }

    #[test]
    fn mapped_exclude_filters_the_wire_key_from_the_local_snapshot() {
        let root = scratch("mapped-exclude");
        let dir = root.join("--data-Projects-x--");
        std::fs::create_dir_all(&dir).unwrap();
        let name =
            "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        std::fs::write(
            dir.join(name),
            b"{\"type\":\"session\",\"version\":3,\"cwd\":\"/data/Projects/x\"}\n",
        )
        .unwrap();
        let mut filesystem = pi_map(&root);
        filesystem.set_path_map(
            PathMap::new(vec![
                ("/data/Projects".into(), "/canon/Projects".into()),
            ])
            .unwrap(),
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
        let wm = SessionFilesystem::new(vec![
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

    #[test]
    fn scan_resolves_root_symlink_but_skips_descendant_symlinks() {
        use std::os::unix::fs::symlink;

        let base = scratch("symlinks");
        let real_root = base.join("real");
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(real_root.join("--project--")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(real_root.join("--project--/one_session.jsonl"), b"one").unwrap();
        std::fs::write(outside.join("outside_session.jsonl"), b"outside").unwrap();
        symlink(&real_root, &root).unwrap();
        symlink(&outside, real_root.join("--project--/linked-dir")).unwrap();
        symlink(
            outside.join("outside_session.jsonl"),
            real_root.join("--project--/linked-file.jsonl"),
        )
        .unwrap();

        let adapter = PiAdapter::new("pi", &root);

        assert_eq!(
            session_files(&root, &adapter),
            vec![root.join("--project--/one_session.jsonl")]
        );
    }

    #[tokio::test]
    async fn mapped_artifact_retries_until_main_session_learns_its_project_dir() {
        let root = scratch("mapped-artifact");
        let mut filesystem = pi_map(&root);
        filesystem.set_path_map(
            PathMap::new(vec![("/data".into(), "/canon".into())]).unwrap(),
            None,
        );
        let main_key =
            "pi/--canon-Projects-x--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        let artifact_key =
            "pi/--canon-Projects-x--/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a/advisor.jsonl";

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
                b"{\"type\":\"session\",\"version\":3,\"cwd\":\"/canon/Projects/x\"}\n"
                    .to_vec(),
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
        let name =
            "2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl";
        let mut bytes = vec![b'x'; 64 * 1024];
        bytes.extend_from_slice(b"\n{\"type\":\"session\",\"cwd\":\"/work/project\"}\n");
        std::fs::write(project.join(name), bytes).unwrap();
        let mut filesystem = pi_map(&root);
        filesystem.set_path_map(
            PathMap::new(vec![("/work".into(), "/canonical".into())]).unwrap(),
            None,
        );

        assert!(filesystem.snapshot().is_empty());
        assert!(filesystem.tombstone_withheld("pi/--project--/other.jsonl"));
    }

    #[tokio::test]
    async fn access_rejects_parent_components_and_symlinked_ancestors() {
        use std::os::unix::fs::symlink;

        let root = scratch("contained-write");
        let outside = scratch("contained-write-outside");
        let mut filesystem = pi_map(&root);

        assert!(
            filesystem
                .write("pi/../escape.jsonl", b"escape".to_vec())
                .await
                .is_err()
        );
        std::fs::write(outside.join("session_id.jsonl"), b"outside").unwrap();
        symlink(&outside, root.join("--project--")).unwrap();
        assert!(
            filesystem
                .write(
                    "pi/--project--/session_id.jsonl",
                    b"escape".to_vec()
                )
                .await
                .is_err()
        );
        assert!(
            filesystem
                .read(
                    "pi/--project--/session_id.jsonl",
                    &root.join("--project--/session_id.jsonl"),
                )
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(outside.join("session_id.jsonl")).unwrap(), b"outside");
    }

    #[tokio::test]
    async fn write_and_delete_are_atomic_contained_mutations() {
        let root = scratch("atomic-mutation");
        let mut filesystem = pi_map(&root);
        let key = "pi/--project--/session_id.jsonl";

        let stamp = filesystem
            .write(key, b"session".to_vec())
            .await
            .unwrap()
            .expect("write must resolve");
        assert_eq!(std::fs::read(root.join("--project--/session_id.jsonl")).unwrap(), b"session");
        assert_eq!(stamp.1, 7);
        assert!(filesystem.delete(key).await.unwrap());
        assert!(!root.join("--project--").exists());
    }
}
