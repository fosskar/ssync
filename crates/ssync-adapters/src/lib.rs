//! Adapter interface: declares *where* an agent stores session files and how to
//! identify them. An adapter MUST NOT parse a session's transcript format for
//! storage/sync purposes (DECISIONS §2). Reading a single header field for identity
//! is allowed; parsing entry lines is not.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};

/// Machine-independent identity of a session file (no transcript parsing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    /// Agent name this session belongs to (the adapter's `agent()`).
    pub agent: String,
    pub session_id: String,
    /// Stable project id (pi: the encoded-cwd dir name).
    pub project_id: String,
    /// Path relative to the session root.
    pub relative_path: PathBuf,
}

/// Describes where an agent stores session files and how to identify them.
pub trait Adapter: Send + Sync + std::fmt::Debug {
    fn agent(&self) -> &str;

    fn session_root(&self) -> &Path;

    /// Path relative to `session_root` (the identity's `relative_path`).
    fn relative_to_root(&self, path: &Path) -> anyhow::Result<PathBuf> {
        Ok(path
            .strip_prefix(self.session_root())
            .with_context(|| format!("{} is not under session root", path.display()))?
            .to_path_buf())
    }

    /// Identify a path under `session_root`. May read minimal metadata (filename,
    /// a single header field) but MUST NOT parse the transcript.
    fn identify(&self, path: &Path) -> anyhow::Result<SessionIdentity>;

    /// Whether sessions are strictly append-only (gates merge).
    fn append_only(&self) -> bool;

    /// Filter sessions vs locks/temp files.
    fn is_session_file(&self, _path: &Path) -> bool {
        true
    }

    /// Session creation time, derived from cheap metadata (pi: the filename
    /// timestamp). `None` when unavailable. Never file mtime — the engine's own
    /// write-backs reset it.
    fn created_at(&self, _path: &Path) -> Option<std::time::SystemTime> {
        None
    }

    /// Best-effort user-given session title for local CLI features (`cleanup
    /// --unnamed`); never used for storage/sync. `Some("")` = a present but
    /// empty title record; `None` = the agent has no title record.
    fn title(&self, _path: &Path) -> Option<String> {
        None
    }

    /// Whether this adapter has cwd semantics the path map can act on
    /// (issue #13). `false` = the engine passes its keys and bytes through
    /// untouched even with an active map.
    fn maps_paths(&self) -> bool {
        false
    }

    /// The session's cwd from its header record — the sanctioned single-
    /// header-field read (scans the first few lines for the `type:session`
    /// record; omp keeps a `type:title` record above it). `None` = this
    /// adapter has no cwd semantics or the bytes carry no header.
    fn header_cwd(&self, _bytes: &[u8]) -> Option<String> {
        None
    }

    /// Rewrite the header record's cwd, leaving every other byte untouched.
    /// The one sanctioned content rewrite (path map — opt-in crossing of
    /// store-as-is; DECISIONS §2, map #42). `None` = unsupported.
    fn rewrite_header_cwd(&self, _bytes: &[u8], _cwd: &str) -> Option<Vec<u8>> {
        None
    }

    /// Encode a cwd into this agent's project-dir name; `home` is the home
    /// context for home-relative encodings (omp). `None` = unsupported, or
    /// undecidable without a home context.
    fn encode_cwd(&self, _cwd: &str, _home: Option<&Path>) -> Option<String> {
        None
    }
}

/// UTF-8 filename stem — the start of every path-derived identity.
pub(crate) fn stem_str(path: &Path) -> anyhow::Result<&str> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("{}: no filename stem", path.display()))
}

/// Uuid shape: 36 chars, dashes at 8/13/18/23, hex elsewhere.
pub(crate) fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_hexdigit(),
        })
}

pub mod blob_store;
pub mod claude_code;
pub mod codex;
pub mod pi;

use blob_store::BlobStoreAdapter;
use claude_code::ClaudeCodeAdapter;
use codex::CodexAdapter;
use pi::PiAdapter;

/// Build the adapter for a configured agent name, boxed so the engine can hold a
/// mix of unrelated adapter types. This is the plug point: a new coding agent is
/// one `impl Adapter` plus one match arm here. Agents that share pi's on-disk
/// layout (pi and its forks, e.g. omp) reuse [`PiAdapter`]; an agent with a
/// different layout gets its own `impl Adapter`.
pub fn adapter_for(
    agent: &str,
    session_root: impl Into<PathBuf>,
) -> anyhow::Result<Box<dyn Adapter>> {
    match agent {
        "pi" | "omp" => Ok(Box::new(PiAdapter::new(agent, session_root))),
        "omp-blobs" => Ok(Box::new(BlobStoreAdapter::new(agent, session_root))),
        "claude-code" => Ok(Box::new(ClaudeCodeAdapter::new(session_root))),
        "codex" => Ok(Box::new(CodexAdapter::new(session_root))),
        other => Err(anyhow::anyhow!(
            "unsupported agent {other:?} (supported: pi, omp, omp-blobs, claude-code, codex)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_for_builds_every_supported_agent() {
        for (agent, root) in [
            ("pi", "/x"),
            ("omp", "/x"),
            ("claude-code", "/x"),
            ("codex", "/x"),
            ("omp-blobs", "/x"),
        ] {
            let a = adapter_for(agent, root).unwrap();
            assert_eq!(a.agent(), agent);
        }
        assert!(adapter_for("amp", "/x").is_err(), "amp has no local files");
        assert!(
            adapter_for("opencode", "/x").is_err(),
            "opencode is db-backed"
        );
    }
}
