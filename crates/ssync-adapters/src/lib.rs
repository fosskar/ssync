//! Adapter interface: declares *where* an agent stores session files and how to
//! identify them. An adapter MUST NOT parse a session's transcript format for
//! storage/sync purposes (DECISIONS §2). Reading a single header field for identity
//! is allowed; parsing entry lines is not.

use std::path::{Path, PathBuf};

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

    /// Identify a path under `session_root`. May read minimal metadata (filename,
    /// a single header field) but MUST NOT parse the transcript.
    fn identify(&self, path: &Path) -> anyhow::Result<SessionIdentity>;

    /// Whether sessions are strictly append-only (gates merge).
    fn append_only(&self) -> bool;

    /// Filter sessions vs locks/temp files.
    fn is_session_file(&self, _path: &Path) -> bool {
        true
    }
}

pub mod pi;

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
        other => Err(anyhow::anyhow!(
            "unsupported agent {other:?} (supported: pi, omp)"
        )),
    }
}
