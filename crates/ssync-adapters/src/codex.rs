//! Codex (OpenAI Codex CLI) adapter. Layout:
//! `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<thread-uuid>.jsonl`, one JSONL
//! file per session (the canonical format; codex's sqlite is only a metadata
//! index and is not touched). Identity is derived from the path alone:
//! session_id = the trailing uuid, project_id = the `YYYY/MM/DD` date
//! partition (codex encodes no project in the path; the cwd lives in the
//! header and identity never reads it). Format reference:
//! docs/codex-format-notes.md.

use std::path::{Path, PathBuf};

use anyhow::anyhow;

use crate::pi::parse_pi_timestamp;
use crate::{Adapter, SessionIdentity, is_uuid, stem_str};

/// A codex session store (`~/.codex/sessions`).
#[derive(Debug)]
pub struct CodexAdapter {
    session_root: PathBuf,
}

impl CodexAdapter {
    pub fn new(session_root: impl Into<PathBuf>) -> Self {
        Self {
            session_root: session_root.into(),
        }
    }
}

/// Split a `rollout-<ts>-<uuid>` stem into (timestamp, uuid). The uuid is the
/// fixed-width 36-char tail; the timestamp is whatever sits between.
fn split_rollout_stem(stem: &str) -> Option<(&str, &str)> {
    let rest = stem.strip_prefix("rollout-")?;
    if rest.len() < 38 {
        return None;
    }
    let (ts, dash_uuid) = rest.split_at(rest.len() - 37);
    let uuid = dash_uuid.strip_prefix('-')?;
    is_uuid(uuid).then_some((ts, uuid))
}

impl Adapter for CodexAdapter {
    fn agent(&self) -> &str {
        "codex"
    }

    fn session_root(&self) -> &Path {
        &self.session_root
    }

    fn identify(&self, path: &Path) -> anyhow::Result<SessionIdentity> {
        let relative_path = self.relative_to_root(path)?;

        let project_id = relative_path
            .parent()
            .map(|p| p.display().to_string())
            .filter(|p| !p.is_empty())
            .ok_or_else(|| anyhow!("{}: no date partition dirs", path.display()))?;

        let stem = stem_str(path)?;
        let (_ts, uuid) = split_rollout_stem(stem)
            .ok_or_else(|| anyhow!("{stem}: expected rollout-<ts>-<uuid>"))?;

        Ok(SessionIdentity {
            agent: self.agent().to_string(),
            session_id: uuid.to_string(),
            project_id,
            relative_path,
        })
    }

    /// Newest-wins permanently by policy (DECISIONS §8 amendment, #25):
    /// codex source opens rollouts with `OpenOptions::append(true)`, but a
    /// wrong `true` scrambles content on conflict, and per-version
    /// re-verification is a treadmill this project refuses.
    fn append_only(&self) -> bool {
        false
    }

    fn is_session_file(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with("rollout-"))
    }

    /// Filename timestamp `YYYY-MM-DDTHH-MM-SS` (seconds precision) → creation
    /// time, via the pi parser with zeroed millis.
    fn created_at(&self, path: &Path) -> Option<std::time::SystemTime> {
        let stem = path.file_stem()?.to_str()?;
        let (ts, _uuid) = split_rollout_stem(stem)?;
        parse_pi_timestamp(&format!("{ts}-000Z"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_codex_session_from_path() {
        let root = Path::new("/home/simon/.codex/sessions");
        let adapter = CodexAdapter::new(root);
        let path = root
            .join("2026/06/03")
            .join("rollout-2026-06-03T09-22-56-019e8b13-aaaa-7bbb-8ccc-0123456789ab.jsonl");

        let id = adapter.identify(&path).unwrap();
        assert_eq!(id.agent, "codex");
        assert_eq!(id.session_id, "019e8b13-aaaa-7bbb-8ccc-0123456789ab");
        assert_eq!(id.project_id, "2026/06/03");
        assert_eq!(
            id.relative_path,
            Path::new("2026/06/03")
                .join("rollout-2026-06-03T09-22-56-019e8b13-aaaa-7bbb-8ccc-0123456789ab.jsonl")
        );
        assert!(adapter.is_session_file(&path));
    }

    #[test]
    fn rejects_malformed_rollout_stems() {
        let root = Path::new("/r");
        let adapter = CodexAdapter::new(root);
        for bad in [
            "2026/06/03/notes.jsonl",
            "2026/06/03/rollout-.jsonl",
            "2026/06/03/rollout-2026-06-03T09-22-56-not-a-uuid-at-all-here-xyz.jsonl",
            "rollout-2026-06-03T09-22-56-019e8b13-aaaa-7bbb-8ccc-0123456789ab.jsonl", // no date dirs
        ] {
            assert!(adapter.identify(&root.join(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn created_at_comes_from_the_filename_timestamp() {
        let adapter = CodexAdapter::new("/r");
        let path = Path::new(
            "/r/2026/05/23/rollout-2026-05-23T06-55-21-019e8b13-aaaa-7bbb-8ccc-0123456789ab.jsonl",
        );
        let t = adapter.created_at(path).expect("parseable timestamp");
        let secs = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs, 1779519321); // date -u -d '2026-05-23T06:55:21Z' +%s
    }

    #[test]
    fn merge_stays_off_by_policy() {
        // newest-wins permanently (DECISIONS §8 amendment, #25) — a conflict
        // must never take the line-union merge.
        assert!(!CodexAdapter::new("/r").append_only());
    }

    #[test]
    fn filters_non_rollout_files() {
        let adapter = CodexAdapter::new("/r");
        assert!(!adapter.is_session_file(Path::new("/r/2026/06/03/other.jsonl")));
        assert!(!adapter.is_session_file(Path::new("/r/2026/06/03/rollout-x.txt")));
    }
}
