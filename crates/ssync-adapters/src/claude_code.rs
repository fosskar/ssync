//! Claude Code adapter. Layout: `~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`,
//! one JSONL file per session. Identity is derived from the path alone:
//! session_id = filename stem (a bare uuid, no timestamp prefix), project_id =
//! `<encoded-cwd>` parent dir (non-alphanumeric chars replaced by `-`; opaque
//! here, never decoded). Format reference: docs/claude-code-format-notes.md.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, ensure};

use crate::{Adapter, SessionIdentity, is_uuid, stem_str};

/// A Claude Code session store.
#[derive(Debug)]
pub struct ClaudeCodeAdapter {
    session_root: PathBuf,
}

impl ClaudeCodeAdapter {
    pub fn new(session_root: impl Into<PathBuf>) -> Self {
        Self {
            session_root: session_root.into(),
        }
    }
}

impl Adapter for ClaudeCodeAdapter {
    fn agent(&self) -> &str {
        "claude-code"
    }

    fn session_root(&self) -> &Path {
        &self.session_root
    }

    fn identify(&self, path: &Path) -> anyhow::Result<SessionIdentity> {
        let relative_path = self.relative_to_root(path)?;

        let project_id = relative_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("{}: no <encoded-cwd> parent dir", path.display()))?
            .to_string();

        let session_id = stem_str(path)?;
        ensure!(
            is_uuid(session_id),
            "{session_id}: expected a bare uuid stem"
        );
        let session_id = session_id.to_string();

        Ok(SessionIdentity {
            agent: self.agent().to_string(),
            session_id,
            project_id,
            relative_path,
        })
    }

    /// Newest-wins permanently by policy (DECISIONS §8 amendment, #25):
    /// upstream docs describe the transcript as append-only, but a wrong
    /// `true` scrambles content on conflict, and per-version re-verification
    /// is a treadmill this project refuses.
    fn append_only(&self) -> bool {
        false
    }

    /// The project dir holds non-session entries too (a `memory/` subdir was
    /// observed); only uuid-named `.jsonl` files are sessions.
    fn is_session_file(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str()) == Some("jsonl")
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(is_uuid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_claude_code_session_from_path() {
        let root = Path::new("/home/simon/.claude/projects");
        let adapter = ClaudeCodeAdapter::new(root);
        let path = root
            .join("-home-simon-Projects-nixfiles")
            .join("b6933609-ab67-467e-af26-e48c3c8c129e.jsonl");

        let id = adapter.identify(&path).unwrap();
        assert_eq!(id.agent, "claude-code");
        assert_eq!(id.session_id, "b6933609-ab67-467e-af26-e48c3c8c129e");
        assert_eq!(id.project_id, "-home-simon-Projects-nixfiles");
        assert_eq!(
            id.relative_path,
            Path::new("-home-simon-Projects-nixfiles")
                .join("b6933609-ab67-467e-af26-e48c3c8c129e.jsonl")
        );
        assert!(adapter.is_session_file(&path));
    }

    #[test]
    fn rejects_session_without_project_dir() {
        let root = Path::new("/r");
        let adapter = ClaudeCodeAdapter::new(root);
        assert!(adapter.identify(&root.join("stray.jsonl")).is_err());
    }

    #[test]
    fn merge_stays_off_by_policy() {
        // newest-wins permanently (DECISIONS §8 amendment, #25) — a conflict
        // must never take the line-union merge.
        assert!(!ClaudeCodeAdapter::new("/r").append_only());
    }

    #[test]
    fn filters_non_jsonl_files() {
        let adapter = ClaudeCodeAdapter::new("/r");
        assert!(!adapter.is_session_file(Path::new("/r/p/notes.txt")));
        assert!(
            !adapter.is_session_file(Path::new("/r/p/b6933609-ab67-467e-af26-e48c3c8c129e.txt"))
        );
    }

    #[test]
    fn filters_non_uuid_jsonl_files() {
        // the project dir holds non-session .jsonl entries too
        // (docs/claude-code-format-notes.md); only uuid-named files are sessions.
        let adapter = ClaudeCodeAdapter::new("/r");
        assert!(!adapter.is_session_file(Path::new("/r/p/stray.jsonl")));
        assert!(!adapter.is_session_file(Path::new("/r/p/memory/topics.jsonl")));
        assert!(
            adapter.is_session_file(Path::new("/r/p/b6933609-ab67-467e-af26-e48c3c8c129e.jsonl"))
        );
    }
}
