//! Adapter interface: declares *where* an agent stores session files and how to
//! identify them. An adapter MUST NOT parse a session's transcript format for
//! storage/sync purposes (DECISIONS §2). Reading a single header field for identity
//! is allowed; parsing entry lines is not.

use std::path::{Path, PathBuf};

/// Machine-independent identity of a session file (no transcript parsing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    pub session_id: String,
    /// Stable project id (pi: the encoded-cwd dir name).
    pub project_id: String,
    /// Path relative to the session root.
    pub relative_path: PathBuf,
}

/// Describes where an agent stores session files and how to identify them.
pub trait Adapter: Send + Sync {
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
