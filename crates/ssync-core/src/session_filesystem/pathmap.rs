//! Prefix map between this machine's local paths and the mesh-wide canonical
//! form (issue #13, map #42): longest-prefix-match with component boundaries,
//! both directions, round-trip-guarded. [`PathMap`] is the pure core — no IO,
//! no config parsing; `Config` validates and builds it. [`Resolver`] is the
//! engine-side orchestration on top: the per-project-dir translation cache,
//! header reads/rewrites through the adapter, and the per-pass agent freeze
//! (#49). The engine consults the resolver; everything path-map lives here.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use ssync_adapters::Adapter;

const HEADER_PREFIX_LIMIT: usize = 64 * 1024;

/// Validated prefix pairs. Invariants from construction: absolute prefixes,
/// no trailing slash, no `/`, no duplicate local or canonical prefixes.
#[derive(Debug, Clone, Default)]
pub struct PathMap {
    /// `(local, canonical)`, longest local first for import-side LPM.
    entries: Vec<(String, String)>,
    /// `(canonical, local)`, longest canonical first for export-side LPM.
    by_canonical: Vec<(String, String)>,
}
impl PathMap {
    /// Build from `(local, canonical)` pairs (both already absolute; the
    /// config layer expands `~` in locals before this).
    pub fn new(pairs: Vec<(String, String)>) -> Result<Self> {
        let mut entries = Vec::with_capacity(pairs.len());
        for (local, canonical) in pairs {
            let local = normalize(&local, "local")?;
            let canonical = normalize(&canonical, "canonical")?;
            entries.push((local, canonical));
        }
        for (i, (l, c)) in entries.iter().enumerate() {
            if entries[..i].iter().any(|(l2, _)| l2 == l) {
                bail!("duplicate local prefix {l} in path_map");
            }
            if entries[..i].iter().any(|(_, c2)| c2 == c) {
                bail!("duplicate canonical prefix {c} in path_map");
            }
        }
        entries.sort_by_key(|(l, _)| std::cmp::Reverse(l.len()));
        let mut by_canonical: Vec<(String, String)> = entries
            .iter()
            .map(|(l, c)| (c.clone(), l.clone()))
            .collect();
        by_canonical.sort_by_key(|(c, _)| std::cmp::Reverse(c.len()));
        Ok(Self {
            entries,
            by_canonical,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Local → canonical. `None` = no prefix matches (pass-through: the
    /// path IS its own canonical form). Longest local prefix wins.
    pub fn to_canonical(&self, path: &str) -> Option<String> {
        lpm(self.entries.iter().map(|(l, c)| (l, c)), path)
    }

    /// Canonical → local, longest canonical prefix wins.
    pub fn to_local(&self, path: &str) -> Option<String> {
        lpm(self.by_canonical.iter().map(|(c, l)| (c, l)), path)
    }

    /// Import-side mapping with the round-trip guard (#49 amendment): the
    /// canonical must map back to the input, else the mapping would flip the
    /// key's identity on the next pass — refuse instead of executing a flip.
    /// `Ok(None)` = pass-through.
    pub fn canonical_of(&self, local: &str) -> Result<Option<String>> {
        let Some(canonical) = self.to_canonical(local) else {
            ensure!(
                self.to_local(local).is_none(),
                "{local} is inside a canonical prefix but unmapped locally — identity would flip on write-back"
            );
            return Ok(None);
        };
        let back = self.to_local(&canonical);
        ensure!(
            back.as_deref() == Some(local),
            "path map does not round-trip for {local}: canonical {canonical} maps back to {}",
            back.as_deref().unwrap_or("(pass-through)")
        );
        Ok(Some(canonical))
    }

    /// Export-side mapping with the symmetric round-trip guard.
    pub fn local_of(&self, canonical: &str) -> Result<Option<String>> {
        let Some(local) = self.to_local(canonical) else {
            ensure!(
                self.to_canonical(canonical).is_none(),
                "{canonical} is inside a local prefix but has no canonical mapping — identity would flip on re-import"
            );
            return Ok(None);
        };
        let back = self.to_canonical(&local);
        ensure!(
            back.as_deref() == Some(canonical),
            "path map does not round-trip for {canonical}: local {local} maps back to {}",
            back.as_deref().unwrap_or("(pass-through)")
        );
        Ok(Some(local))
    }
}

/// Wire↔local translation for the engine: caches how each project dir maps,
/// learns translations from write-backs, and tracks agents whose mapping
/// failed this pass so their tombstones are withheld (#49). With an empty
/// map every method is a pass-through.
#[derive(Debug, Default)]
pub(crate) struct Resolver {
    map: PathMap,
    /// Home context for the wire side of home-relative encodings (omp).
    canonical_home: Option<PathBuf>,
    /// Per project dir: how its keys and bytes translate (`None` value =
    /// pass-through). Dir↔cwd never changes, so entries never invalidate.
    dir_map: HashMap<PathBuf, Option<String>>,
    /// Dirs whose mapping failed, already logged (retry stays quiet).
    logged: HashSet<PathBuf>,
    /// Agents with an unresolvable mapping this pass (#49).
    failed_agents: HashSet<String>,
}

impl Resolver {
    pub fn new(map: PathMap, canonical_home: Option<PathBuf>) -> Self {
        Self {
            map,
            canonical_home,
            ..Self::default()
        }
    }

    /// The wire form of a local file's session-root-relative path (issue
    /// #13): the canonical first component when its project dir is mapped.
    /// `Ok(None)` = pass-through (today's relative path IS the wire form).
    /// `Err` = unresolvable this tick; the caller skips the file and a later
    /// pass retries.
    pub fn wire_rel(&mut self, adapter: &dyn Adapter, rel: &str) -> Result<Option<String>> {
        // adapters without cwd semantics (omp-blobs, codex, claude-code)
        // pass through untouched — their keys and bytes carry no paths the
        // map could act on
        if self.map.is_empty() || !adapter.maps_paths() {
            return Ok(None);
        }
        let Some((first, rest)) = rel.split_once('/') else {
            bail!("{rel}: expected <project>/<file>");
        };
        let project_dir = adapter.session_root().join(first);
        Ok(self
            .dir_mapping(adapter, &project_dir)?
            .map(|component| format!("{component}/{rest}")))
    }

    /// How a project dir translates, resolved once from any main session
    /// file's header and cached — a dir's cwd never changes. `Ok(None)` =
    /// pass-through (unmapped). `Err` = header unreadable, round-trip guard
    /// tripped, or encoding needs `canonical_home`: skip and retry.
    fn dir_mapping(&mut self, adapter: &dyn Adapter, project_dir: &Path) -> Result<Option<String>> {
        if let Some(cached) = self.dir_map.get(project_dir) {
            return Ok(cached.clone());
        }
        let mut local_cwd = None;
        let mut oversized = None;
        for entry in std::fs::read_dir(project_dir)
            .with_context(|| format!("reading {}", project_dir.display()))?
        {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = entry.path();
            if kind.is_symlink()
                || !kind.is_file()
                || path.extension().and_then(|extension| extension.to_str()) != Some("jsonl")
            {
                continue;
            }
            let file = std::fs::File::open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            let mut bytes = Vec::with_capacity(HEADER_PREFIX_LIMIT + 1);
            file.take((HEADER_PREFIX_LIMIT + 1) as u64)
                .read_to_end(&mut bytes)
                .with_context(|| format!("reading header from {}", path.display()))?;
            if let Some(cwd) = adapter.header_cwd(&bytes) {
                local_cwd = Some(cwd);
                break;
            }
            if bytes.len() > HEADER_PREFIX_LIMIT {
                oversized = Some(path);
            }
        }
        if let Some(path) = oversized
            && local_cwd.is_none()
        {
            bail!("{}: required session header exceeds 64 KiB", path.display());
        }
        let Some(local_cwd) = local_cwd else {
            bail!(
                "{}: no session header to resolve the project's cwd",
                project_dir.display()
            );
        };
        let mapping = match self.map.canonical_of(&local_cwd)? {
            None => None,
            Some(canonical_cwd) => Some(
                adapter
                    .encode_cwd(&canonical_cwd, self.canonical_home.as_deref())
                    .ok_or_else(|| {
                        anyhow!(
                            "cannot derive {}'s wire dir for {canonical_cwd}: set canonical_home",
                            adapter.agent()
                        )
                    })?,
            ),
        };
        self.dir_map
            .insert(project_dir.to_path_buf(), mapping.clone());
        Ok(mapping)
    }

    /// Local destination + on-disk bytes for a wire-relative path's
    /// plaintext. With an empty map: today's literal dest, bytes untouched.
    /// `Ok(None)` = not resolvable yet (artifact of a not-yet-materialized
    /// project) — the caller returns false and a later tick retries.
    pub fn localize(
        &mut self,
        adapter: &dyn Adapter,
        rel: &str,
        plaintext: Vec<u8>,
    ) -> Result<Option<(PathBuf, Vec<u8>)>> {
        let dest = adapter.session_root().join(rel);
        if self.map.is_empty() || !adapter.maps_paths() {
            return Ok(Some((dest, plaintext)));
        }
        let Some((first, rest)) = rel.split_once('/') else {
            bail!("{rel}: expected <project>/<file>");
        };
        match adapter.header_cwd(&plaintext) {
            Some(canonical_cwd) => match self.map.local_of(&canonical_cwd)? {
                // pass-through: the canonical path IS the local path
                None => Ok(Some((dest, plaintext))),
                Some(local_cwd) => {
                    let component = adapter
                        .encode_cwd(&local_cwd, dirs::home_dir().as_deref())
                        .ok_or_else(|| anyhow!("cannot derive local dir for {local_cwd}"))?;
                    let bytes = adapter
                        .rewrite_header_cwd(&plaintext, &local_cwd)
                        .ok_or_else(|| anyhow!("cannot rewrite header for {rel}"))?;
                    let local_dir = adapter.session_root().join(&component);
                    let local_dest = local_dir.join(rest);
                    // remember the translation for header-less siblings
                    // (artifact records) and DeleteLocal
                    self.dir_map.insert(local_dir, Some(first.to_string()));
                    Ok(Some((local_dest, bytes)))
                }
            },
            // header-less bytes (artifact records): dir-level translation
            None => Ok(self
                .local_project_dir(adapter.session_root(), first)
                .map(|dir| (dir.join(rest), plaintext))),
        }
    }

    /// [`localize`](Self::localize) for actions without plaintext in hand
    /// (DeleteLocal). `None` = nothing local to touch.
    pub fn local_dest_of(&self, adapter: &dyn Adapter, rel: &str) -> Option<PathBuf> {
        let dest = adapter.session_root().join(rel);
        if self.map.is_empty() || !adapter.maps_paths() {
            return Some(dest);
        }
        let (first, rest) = rel.split_once('/')?;
        Some(
            self.local_project_dir(adapter.session_root(), first)?
                .join(rest),
        )
    }

    /// The local project dir a canonical wire component maps to, from the
    /// learned/scanned translations (pass-through dirs match by name).
    fn local_project_dir(&self, root: &Path, canonical_component: &str) -> Option<PathBuf> {
        self.dir_map.iter().find_map(|(dir, m)| {
            if !dir.starts_with(root) {
                return None;
            }
            match m {
                Some(c) if c == canonical_component => Some(dir.clone()),
                None if dir.file_name().and_then(|n| n.to_str()) == Some(canonical_component) => {
                    Some(dir.clone())
                }
                _ => None,
            }
        })
    }

    /// The wire form of a local file's bytes: header cwd mapped to canonical
    /// (issue #13). A machine-local path must never reach the index — an
    /// unmappable header is a per-key error, not a pass-through (#49).
    pub fn canonical_plaintext(&self, adapter: &dyn Adapter, bytes: Vec<u8>) -> Result<Vec<u8>> {
        if self.map.is_empty() {
            return Ok(bytes);
        }
        let Some(local_cwd) = adapter.header_cwd(&bytes) else {
            return Ok(bytes); // header-less artifact records pass through
        };
        match self.map.canonical_of(&local_cwd)? {
            Some(canonical) => adapter
                .rewrite_header_cwd(&bytes, &canonical)
                .ok_or_else(|| anyhow!("cannot rewrite session header (cwd {canonical})")),
            None => Ok(bytes),
        }
    }

    /// Start a snapshot pass: last pass's failures no longer freeze agents.
    pub fn begin_pass(&mut self) {
        self.failed_agents.clear();
    }

    /// Record a mapping failure: freezes `agent`'s tombstones this pass
    /// (#49). Returns whether `dir`'s failure is newly seen — log it once.
    pub fn note_failure(&mut self, agent: &str, dir: PathBuf) -> bool {
        self.failed_agents.insert(agent.to_string());
        self.logged.insert(dir)
    }

    /// Whether `agent` had an unresolvable mapping this pass.
    pub fn agent_failed(&self, agent: &str) -> bool {
        self.failed_agents.contains(agent)
    }
}

/// Longest-prefix-match at component boundaries; `entries` need not be sorted
/// (caller pre-sorts by descending prefix length).
fn lpm<'a>(entries: impl Iterator<Item = (&'a String, &'a String)>, path: &str) -> Option<String> {
    for (from, to) in entries {
        if let Some(rest) = path.strip_prefix(from.as_str())
            && (rest.is_empty() || rest.starts_with('/'))
        {
            return Some(format!("{to}{rest}"));
        }
    }
    None
}

fn normalize(prefix: &str, side: &str) -> Result<String> {
    ensure!(
        prefix.starts_with('/'),
        "path_map {side} prefix {prefix} must be absolute"
    );
    let trimmed = prefix.trim_end_matches('/');
    ensure!(!trimmed.is_empty(), "path_map {side} prefix must not be /");
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> PathMap {
        PathMap::new(
            pairs
                .iter()
                .map(|(l, c)| (l.to_string(), c.to_string()))
                .collect(),
        )
        .unwrap()
    }

    #[test]
    fn maps_both_directions_below_the_prefix() {
        let m = map(&[("/srv/work", "/home/simon/Projects")]);
        assert_eq!(
            m.to_canonical("/srv/work/x").as_deref(),
            Some("/home/simon/Projects/x")
        );
        assert_eq!(
            m.to_local("/home/simon/Projects/x").as_deref(),
            Some("/srv/work/x")
        );
        // the prefix itself maps too
        assert_eq!(
            m.to_canonical("/srv/work").as_deref(),
            Some("/home/simon/Projects")
        );
    }

    #[test]
    fn unmapped_paths_pass_through() {
        let m = map(&[("/srv/work", "/home/simon/Projects")]);
        assert_eq!(m.to_canonical("/etc/other"), None);
        assert_eq!(m.to_local("/etc/other"), None);
        assert_eq!(m.canonical_of("/etc/other").unwrap(), None);
        assert_eq!(m.local_of("/etc/other").unwrap(), None);
    }

    #[test]
    fn component_boundaries_hold() {
        let m = map(&[("/srv/work", "/canon")]);
        // /srv/work2 must NOT match /srv/work
        assert_eq!(m.to_canonical("/srv/work2/x"), None);
        assert_eq!(m.to_local("/canon2/x"), None);
    }

    #[test]
    fn longest_prefix_wins_with_nesting() {
        let m = map(&[("/a", "/c1"), ("/a/b", "/c2")]);
        assert_eq!(m.to_canonical("/a/x").as_deref(), Some("/c1/x"));
        assert_eq!(m.to_canonical("/a/b/x").as_deref(), Some("/c2/x"));
        assert_eq!(m.to_local("/c2/x").as_deref(), Some("/a/b/x"));
        assert_eq!(m.to_local("/c1/x").as_deref(), Some("/a/x"));
    }

    #[test]
    fn round_trip_guard_blocks_identity_flips() {
        // the #49 flip: /a→/c1 nested with /a/b→/c2. canonical /c1/b/x lands
        // at local /a/b/x, which re-imports as /c2/x — a different key.
        let m = map(&[("/a", "/c1"), ("/a/b", "/c2")]);
        assert!(m.local_of("/c1/b/x").is_err(), "flip must be refused");
        // the unambiguous forms still work
        assert!(m.local_of("/c1/x").is_ok());
        assert!(m.local_of("/c2/x").is_ok());
        assert!(m.canonical_of("/a/b/x").is_ok());
    }

    #[test]
    fn pass_through_inside_a_mapped_canonical_is_refused_on_import() {
        // local /home/simon/Projects/x exists on the MAPPED machine: importing
        // it pass-through would collide with the canonical space that /srv/work
        // maps into, and its write-back would re-import as /srv/work/x.
        let m = map(&[("/srv/work", "/home/simon/Projects")]);
        assert!(m.canonical_of("/home/simon/Projects/x").is_err());
        // symmetric on export: a canonical path inside a local prefix
        assert!(m.local_of("/srv/work/x").is_err());
    }

    #[test]
    fn validation_rejects_bad_prefixes() {
        assert!(PathMap::new(vec![("relative".into(), "/c".into())]).is_err());
        assert!(PathMap::new(vec![("/l".into(), "c".into())]).is_err());
        assert!(PathMap::new(vec![("/".into(), "/c".into())]).is_err());
        assert!(
            PathMap::new(vec![
                ("/l".into(), "/c".into()),
                ("/l".into(), "/c2".into())
            ])
            .is_err(),
            "duplicate local"
        );
        assert!(
            PathMap::new(vec![
                ("/l".into(), "/c".into()),
                ("/l2".into(), "/c".into())
            ])
            .is_err(),
            "duplicate canonical"
        );
    }

    #[test]
    fn trailing_slashes_normalize() {
        let m = map(&[("/srv/work/", "/canon/")]);
        assert_eq!(m.to_canonical("/srv/work/x").as_deref(), Some("/canon/x"));
    }

    #[test]
    fn empty_map_is_inert() {
        let m = PathMap::default();
        assert!(m.is_empty());
        assert_eq!(m.canonical_of("/any").unwrap(), None);
        assert_eq!(m.local_of("/any").unwrap(), None);
    }
}
