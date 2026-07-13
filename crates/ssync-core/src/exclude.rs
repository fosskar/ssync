//! Session exclusion (issue #14): per-agent `exclude` patterns matched
//! against a key's relative path (`<encoded-cwd>/<file>` as it appears on
//! disk). A matching key is invisible to reconcile from BOTH sides — not
//! imported, not exported, never tombstoned — so excluding an already-synced
//! project freezes it everywhere instead of deleting anything. Patterns are
//! plain substrings with `*` wildcards (no `?`, no character classes); the
//! encoded dir name keeps the project path's words, so `*client*` works
//! without decoding it.

/// Whether `rel` matches any exclude pattern. A pattern without `*` must
/// match the whole relative path, so `exclude = ["*client*"]` (not
/// `"client"`) is the substring form — same contract as a shell glob.
pub fn is_excluded(patterns: &[String], rel: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, rel))
}

/// Minimal `*`-only glob: `*` matches any run of characters (including `/`
/// — sessions nest under per-project dirs, and a project filter must reach
/// the files below it).
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    // no wildcard: exact match
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut rest = text;
    // the first literal is anchored at the start, the last at the end
    if let Some(first) = parts.first() {
        let Some(r) = rest.strip_prefix(first) else {
            return false;
        };
        rest = r;
    }
    let last = parts.last().expect("split yields at least one part");
    for mid in &parts[1..parts.len() - 1] {
        if mid.is_empty() {
            continue; // consecutive `*`s collapse
        }
        let Some(at) = rest.find(mid) else {
            return false;
        };
        rest = &rest[at + mid.len()..];
    }
    rest.ends_with(last) && (parts.len() == 1 || rest.len() >= last.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pats(ps: &[&str]) -> Vec<String> {
        ps.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn empty_patterns_exclude_nothing() {
        assert!(!is_excluded(&[], "--proj--/a.jsonl"));
    }

    #[test]
    fn substring_form_matches_the_encoded_dir() {
        let p = pats(&["*work-client*"]);
        assert!(is_excluded(&p, "--home-x-work-client--/s.jsonl"));
        assert!(!is_excluded(&p, "--home-x-hobby--/s.jsonl"));
    }

    #[test]
    fn bare_pattern_is_exact_not_substring() {
        let p = pats(&["client"]);
        assert!(!is_excluded(&p, "--client--/s.jsonl"));
        assert!(is_excluded(&p, "client"));
    }

    #[test]
    fn star_crosses_directory_separators() {
        // a project filter must reach files nested below the project dir
        let p = pats(&["*secret-proj*"]);
        assert!(is_excluded(&p, "--secret-proj--/2026/05/rollout.jsonl"));
    }

    #[test]
    fn prefix_and_suffix_anchor() {
        let p = pats(&["--work*"]);
        assert!(is_excluded(&p, "--work-x--/s.jsonl"));
        assert!(!is_excluded(&p, "--home-work-x--/s.jsonl"));

        let s = pats(&["*.tmp"]);
        assert!(is_excluded(&s, "--p--/a.tmp"));
        assert!(!is_excluded(&s, "--p--/a.jsonl"));
    }

    #[test]
    fn multiple_wildcards_match_in_order() {
        let p = pats(&["*a*b*"]);
        assert!(is_excluded(&p, "xx-a-yy-b-zz"));
        assert!(!is_excluded(&p, "xx-b-yy-a-zz"));
    }

    #[test]
    fn any_of_several_patterns_excludes() {
        let p = pats(&["*alpha*", "*beta*"]);
        assert!(is_excluded(&p, "--beta--/s.jsonl"));
        assert!(is_excluded(&p, "--alpha--/s.jsonl"));
        assert!(!is_excluded(&p, "--gamma--/s.jsonl"));
    }

    #[test]
    fn overlapping_literal_needs_both_occurrences() {
        // `rest.len() >= last.len()` guard: the middle match must not be
        // re-consumed by the end anchor.
        let p = pats(&["*ab*ab"]);
        assert!(is_excluded(&p, "xxabyyab"));
        assert!(!is_excluded(&p, "xxab"));
    }
}
