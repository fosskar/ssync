//! Prefix map between this machine's local paths and the mesh-wide canonical
//! form (issue #13, map #42): longest-prefix-match with component boundaries,
//! both directions, round-trip-guarded. [`PathMap`] is the pure core — no IO,
//! no config parsing; `Config` validates and builds it. [`Resolver`] is the
//! engine-side orchestration on top: the per-project-dir translation cache,
//! header reads/rewrites through the adapter, and the per-pass agent freeze
//! (#49). The engine consults the resolver; everything path-map lives here.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail, ensure};
use ssync_adapters::Adapter;

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
        for entry in std::fs::read_dir(project_dir)
            .with_context(|| format!("reading {}", project_dir.display()))?
        {
            let p = entry?.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") || !p.is_file() {
                continue;
            }
            if let Ok(bytes) = std::fs::read(&p)
                && let Some(cwd) = adapter.header_cwd(&bytes)
            {
                local_cwd = Some(cwd);
                break;
            }
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

    mod resolver {
        use super::*;
        use ssync_adapters::pi::PiAdapter;

        fn scratch(tag: &str) -> PathBuf {
            let base =
                std::env::temp_dir().join(format!("ssync-resolver-{}-{}", tag, std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            base
        }

        fn header(cwd: &str) -> Vec<u8> {
            format!(
                "{{\"type\":\"session\",\"version\":3,\"id\":\"019e539d-f6ab-71ac-be20-d3ae2b23ea4a\",\"cwd\":\"{cwd}\"}}\n{{\"m\":\"1\"}}\n"
            )
            .into_bytes()
        }

        #[test]
        fn empty_map_is_pass_through() {
            let root = scratch("inert");
            let pi = PiAdapter::new("pi", &root);
            let mut r = Resolver::default();
            assert_eq!(r.wire_rel(&pi, "--proj--/a.jsonl").unwrap(), None);
            let bytes = header("/data/x");
            let (dest, out) = r
                .localize(&pi, "--proj--/a.jsonl", bytes.clone())
                .unwrap()
                .unwrap();
            assert_eq!(dest, root.join("--proj--/a.jsonl"));
            assert_eq!(out, bytes);
            assert_eq!(
                r.local_dest_of(&pi, "--proj--/a.jsonl"),
                Some(root.join("--proj--/a.jsonl"))
            );
            assert_eq!(r.canonical_plaintext(&pi, bytes.clone()).unwrap(), bytes);
        }

        #[test]
        fn wire_rel_maps_project_dir_via_header_and_caches() {
            let root = scratch("wire");
            let pi = PiAdapter::new("pi", &root);
            let dir = root.join("--data-Projects-x--");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("s.jsonl"), header("/data/Projects/x")).unwrap();
            let mut r = Resolver::new(map(&[("/data/Projects", "/canon/Projects")]), None);
            assert_eq!(
                r.wire_rel(&pi, "--data-Projects-x--/s.jsonl")
                    .unwrap()
                    .as_deref(),
                Some("--canon-Projects-x--/s.jsonl")
            );
            // resolved once per dir: the mapping survives the dir's removal
            std::fs::remove_dir_all(&dir).unwrap();
            assert_eq!(
                r.wire_rel(&pi, "--data-Projects-x--/t.jsonl")
                    .unwrap()
                    .as_deref(),
                Some("--canon-Projects-x--/t.jsonl")
            );
        }

        #[test]
        fn unmapped_cwd_is_pass_through() {
            let root = scratch("passthrough");
            let pi = PiAdapter::new("pi", &root);
            let dir = root.join("--elsewhere-y--");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("s.jsonl"), header("/elsewhere/y")).unwrap();
            let mut r = Resolver::new(map(&[("/data/Projects", "/canon/Projects")]), None);
            assert_eq!(r.wire_rel(&pi, "--elsewhere-y--/s.jsonl").unwrap(), None);
        }

        #[test]
        fn missing_header_is_an_error_and_freezes_the_agent() {
            let root = scratch("freeze");
            let pi = PiAdapter::new("pi", &root);
            let dir = root.join("--proj--");
            std::fs::create_dir_all(&dir).unwrap(); // no session header inside
            let mut r = Resolver::new(map(&[("/data", "/canon")]), None);
            assert!(r.wire_rel(&pi, "--proj--/s.jsonl").is_err());
            assert!(r.note_failure("pi", dir.clone()), "first failure logs");
            assert!(!r.note_failure("pi", dir), "repeat failure stays quiet");
            assert!(r.agent_failed("pi"));
            r.begin_pass();
            assert!(!r.agent_failed("pi"), "freeze lasts one pass");
        }

        #[test]
        fn omp_without_canonical_home_is_an_error() {
            let root = scratch("omp-home");
            let omp = PiAdapter::new("omp", &root);
            let dir = root.join("-Projects-x");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("s.jsonl"), header("/data/Projects/x")).unwrap();
            let mut r = Resolver::new(map(&[("/data", "/canon-home")]), None);
            let err = r.wire_rel(&omp, "-Projects-x/s.jsonl").unwrap_err();
            assert!(err.to_string().contains("canonical_home"), "{err:#}");
        }

        #[test]
        fn localize_rewrites_header_and_learns_the_dir_translation() {
            let root = scratch("localize");
            let pi = PiAdapter::new("pi", &root);
            let mut r = Resolver::new(map(&[("/local/work", "/canon/Projects")]), None);
            let (dest, bytes) = r
                .localize(
                    &pi,
                    "--canon-Projects-x--/s.jsonl",
                    header("/canon/Projects/x"),
                )
                .unwrap()
                .unwrap();
            assert_eq!(dest, root.join("--local-work-x--/s.jsonl"));
            assert_eq!(bytes, header("/local/work/x"));

            // header-less artifact bytes ride the learned translation ...
            let art = b"{\"type\":\"advisor\"}\n".to_vec();
            let (art_dest, art_bytes) = r
                .localize(&pi, "--canon-Projects-x--/art/a.jsonl", art.clone())
                .unwrap()
                .unwrap();
            assert_eq!(art_dest, root.join("--local-work-x--/art/a.jsonl"));
            assert_eq!(art_bytes, art);
            // ... and so does DeleteLocal's dest lookup
            assert_eq!(
                r.local_dest_of(&pi, "--canon-Projects-x--/s.jsonl"),
                Some(root.join("--local-work-x--/s.jsonl"))
            );
        }

        #[test]
        fn localize_artifact_of_unmaterialized_project_retries_later() {
            let root = scratch("unresolved");
            let pi = PiAdapter::new("pi", &root);
            let mut r = Resolver::new(map(&[("/local/work", "/canon/Projects")]), None);
            // no translation learned yet: not resolvable, not an error
            let out = r
                .localize(
                    &pi,
                    "--canon-Projects-x--/art/a.jsonl",
                    b"no header\n".to_vec(),
                )
                .unwrap();
            assert_eq!(out, None);
        }

        #[test]
        fn canonical_plaintext_maps_the_header_cwd() {
            let root = scratch("canonical");
            let pi = PiAdapter::new("pi", &root);
            let r = Resolver::new(map(&[("/local/work", "/canon/Projects")]), None);
            assert_eq!(
                r.canonical_plaintext(&pi, header("/local/work/x")).unwrap(),
                header("/canon/Projects/x")
            );
            // header-less artifact records pass through
            let art = b"{\"type\":\"advisor\"}\n".to_vec();
            assert_eq!(r.canonical_plaintext(&pi, art.clone()).unwrap(), art);
            // an unmapped cwd is already canonical
            assert_eq!(
                r.canonical_plaintext(&pi, header("/elsewhere/y")).unwrap(),
                header("/elsewhere/y")
            );
        }
    }
}
