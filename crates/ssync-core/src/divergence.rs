//! The divergence verdict for one index key: does the union of all authors'
//! versions differ from the winning entry, and if so, what merged bytes to
//! publish? Pure over plaintext bytes and content hashes — no age, no node
//! IO, no clock (DECISIONS §8). The engine fetches and decrypts blobs; this
//! module decides.

use std::collections::HashMap;
use std::sync::Mutex;

use ssync_net::iroh_blobs::Hash;

/// The verdict for one key's version set.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Some blob is not local (or not decryptable) yet — a union over a
    /// partial version set would transiently drop a fork's lines, so a merge
    /// is all-or-skip.
    Incomplete,
    /// The union equals the winner; nothing to publish.
    Settled,
    /// The union differs from the winner: the merged bytes to publish.
    Diverged(Vec<u8>),
}

/// Verdict computation plus a per-key cache keyed by the version-set
/// fingerprint, so a key that still carries a stale second author entry costs
/// one lookup per tick instead of re-decrypting every version.
#[derive(Default)]
pub struct Divergence {
    cache: Mutex<HashMap<String, (String, bool)>>,
}

impl Divergence {
    /// The cached boolean verdict for exactly this version set, if known.
    pub fn cached(&self, key: &str, versions: &[Hash]) -> Option<bool> {
        let fp = fingerprint(versions);
        self.cache
            .lock()
            .unwrap()
            .get(key)
            .and_then(|(f, d)| (*f == fp).then_some(*d))
    }

    /// Decide from plaintexts (`None` = blob unavailable; a set shorter than
    /// `versions` reads as unavailable too) and cache the boolean verdict.
    pub fn verdict(
        &self,
        key: &str,
        versions: &[Hash],
        winner: Option<Vec<u8>>,
        plaintexts: Vec<Option<Vec<u8>>>,
    ) -> Verdict {
        let Some(winner) = winner else {
            return Verdict::Incomplete;
        };
        if plaintexts.len() != versions.len() {
            return Verdict::Incomplete;
        }
        let mut pts = Vec::with_capacity(plaintexts.len());
        for pt in plaintexts {
            match pt {
                Some(pt) => pts.push(pt),
                None => return Verdict::Incomplete,
            }
        }
        let merged = merge_lines(&pts);
        let diverged = merged != winner;
        self.cache
            .lock()
            .unwrap()
            .insert(key.to_string(), (fingerprint(versions), diverged));
        if diverged {
            Verdict::Diverged(merged)
        } else {
            Verdict::Settled
        }
    }
}

/// Order-insensitive identity of a version set.
fn fingerprint(versions: &[Hash]) -> String {
    let mut fp: Vec<String> = versions.iter().map(|h| h.to_string()).collect();
    fp.sort();
    fp.join(",")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u8) -> Hash {
        Hash::new([n])
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

    #[test]
    fn verdict_incomplete_while_any_version_blob_is_missing() {
        let d = Divergence::default();
        let versions = [h(1), h(2)];
        let v = d.verdict(
            "k",
            &versions,
            Some(b"h\na\n".to_vec()),
            vec![Some(b"h\na\n".to_vec()), None],
        );
        assert_eq!(v, Verdict::Incomplete);
        assert_eq!(d.cached("k", &versions), None, "incomplete must not cache");
    }

    #[test]
    fn verdict_incomplete_without_winner_plaintext() {
        let d = Divergence::default();
        assert_eq!(d.verdict("k", &[h(1)], None, vec![]), Verdict::Incomplete);
    }

    #[test]
    fn verdict_incomplete_when_plaintexts_do_not_cover_the_version_set() {
        // the engine may stop decrypting at the first miss; a short set never merges.
        let d = Divergence::default();
        let v = d.verdict(
            "k",
            &[h(1), h(2)],
            Some(b"h\n".to_vec()),
            vec![Some(b"h\n".to_vec())],
        );
        assert_eq!(v, Verdict::Incomplete);
    }

    #[test]
    fn verdict_settled_when_union_equals_winner_and_caches() {
        let d = Divergence::default();
        let versions = [h(1), h(2)];
        let short = b"h\na\n".to_vec();
        let long = b"h\na\nb\n".to_vec();
        let v = d.verdict(
            "k",
            &versions,
            Some(long.clone()),
            vec![Some(short), Some(long)],
        );
        assert_eq!(v, Verdict::Settled);
        assert_eq!(d.cached("k", &versions), Some(false));
    }

    #[test]
    fn verdict_diverged_yields_the_lossless_union_and_caches() {
        let d = Divergence::default();
        let versions = [h(1), h(2)];
        let a = b"h\na1\n".to_vec();
        let b = b"h\nb1\n".to_vec();
        let v = d.verdict(
            "k",
            &versions,
            Some(a.clone()),
            vec![Some(a.clone()), Some(b.clone())],
        );
        assert_eq!(v, Verdict::Diverged(merge_lines(&[a, b])));
        assert_eq!(d.cached("k", &versions), Some(true));
    }

    #[test]
    fn cache_misses_when_the_version_set_changes() {
        let d = Divergence::default();
        let a = b"h\na1\n".to_vec();
        let b = b"h\nb1\n".to_vec();
        d.verdict("k", &[h(1), h(2)], Some(a.clone()), vec![Some(a), Some(b)]);
        assert_eq!(d.cached("k", &[h(1), h(3)]), None);
        assert_eq!(d.cached("other", &[h(1), h(2)]), None);
    }

    #[test]
    fn cache_ignores_version_order() {
        let d = Divergence::default();
        let a = b"h\na1\n".to_vec();
        let b = b"h\nb1\n".to_vec();
        d.verdict("k", &[h(1), h(2)], Some(a.clone()), vec![Some(a), Some(b)]);
        assert_eq!(d.cached("k", &[h(2), h(1)]), Some(true));
    }
}
