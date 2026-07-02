//! pi adapter. Identity is derived from the path/filename alone (no transcript
//! parsing): session_id = uuid after the last `_` in the stem, project_id =
//! `<encoded-cwd>` parent dir, relative_path = path under the session root.
//! Format reference: docs/pi-format-notes.md.

use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};

use crate::{Adapter, SessionIdentity};

pub struct PiAdapter {
    session_root: PathBuf,
}

impl PiAdapter {
    pub fn new(session_root: impl Into<PathBuf>) -> Self {
        Self {
            session_root: session_root.into(),
        }
    }
}

impl Adapter for PiAdapter {
    fn agent(&self) -> &str {
        "pi"
    }

    fn session_root(&self) -> &Path {
        &self.session_root
    }

    fn identify(&self, path: &Path) -> anyhow::Result<SessionIdentity> {
        let relative_path = path
            .strip_prefix(&self.session_root)
            .with_context(|| format!("{} is not under session root", path.display()))?
            .to_path_buf();

        let project_id = relative_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("{}: no <encoded-cwd> parent dir", path.display()))?
            .to_string();

        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow!("{}: no filename stem", path.display()))?;
        let session_id = stem
            .rsplit_once('_')
            .map(|(_ts, id)| id)
            .ok_or_else(|| anyhow!("{stem}: expected <ts>_<sessionId>"))?
            .to_string();

        Ok(SessionIdentity {
            session_id,
            project_id,
            relative_path,
        })
    }

    fn append_only(&self) -> bool {
        true
    }

    fn is_session_file(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str()) == Some("jsonl")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_pi_session_from_path() {
        let root = Path::new("/home/simon/.pi/agent/sessions");
        let adapter = PiAdapter::new(root);
        let path = root
            .join("--home-simon-Projects-nixfiles--")
            .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl");

        let id = adapter.identify(&path).unwrap();
        assert_eq!(id.session_id, "019e539d-f6ab-71ac-be20-d3ae2b23ea4a");
        assert_eq!(id.project_id, "--home-simon-Projects-nixfiles--");
        assert_eq!(
            id.relative_path,
            Path::new("--home-simon-Projects-nixfiles--")
                .join("2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl")
        );
        assert!(adapter.is_session_file(&path));
        assert!(adapter.append_only());
    }
}
