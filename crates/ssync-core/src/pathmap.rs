//! Prefix map between this machine's local paths and the mesh-wide canonical
//! form (issue #13, map #42): longest-prefix-match with component boundaries,
//! both directions, round-trip-guarded. Pure — no IO, no config parsing; the
//! engine consults it, `Config` validates and builds it.

use anyhow::{Result, bail, ensure};

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
