//! omp blob-store adapter — the global content-addressed image store at
//! `~/.omp/agent/blobs`: a flat dir where each blob is `<sha256>` plus a
//! hardlinked `<sha256>.<ext>` alias. omp session files reference blobs as
//! `blob:sha256:<hash>`, so blobs are session content and must travel with
//! the sessions (DECISIONS §9, amended). Identity is the hash filename alone;
//! content-addressing makes every file immutable, so two machines can never
//! disagree about a blob's bytes — conflicts are impossible and merge never
//! applies. `created_at` and `title` stay `None` on purpose: a blob's age
//! says nothing about whether a *newer* session still references it (ssync
//! never parses transcripts, DECISIONS §2), so `cleanup --before`/`--unnamed`
//! must never select blobs.

use std::path::{Path, PathBuf};

use anyhow::ensure;

use crate::{Adapter, SessionIdentity, stem_str};

/// A flat content-addressed blob store, labelled by the agent that owns it
/// (`omp-blobs`).
#[derive(Debug)]
pub struct BlobStoreAdapter {
    agent: String,
    session_root: PathBuf,
}

impl BlobStoreAdapter {
    pub fn new(agent: impl Into<String>, session_root: impl Into<PathBuf>) -> Self {
        Self {
            agent: agent.into(),
            session_root: session_root.into(),
        }
    }
}

/// Lowercase sha256 hex — the shape of every blob filename stem.
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Adapter for BlobStoreAdapter {
    fn agent(&self) -> &str {
        &self.agent
    }

    fn session_root(&self) -> &Path {
        &self.session_root
    }

    fn identify(&self, path: &Path) -> anyhow::Result<SessionIdentity> {
        let relative_path = self.relative_to_root(path)?;
        ensure!(
            relative_path.components().count() == 1,
            "{}: blob store is flat, nested path is not a blob",
            relative_path.display()
        );
        let stem = stem_str(&relative_path)?;
        ensure!(
            is_sha256_hex(stem),
            "{stem}: expected a lowercase sha256 hex blob name"
        );
        Ok(SessionIdentity {
            agent: self.agent.clone(),
            session_id: stem.to_string(),
            // flat store shared by all projects; no project to name
            project_id: String::new(),
            relative_path,
        })
    }

    /// Whole-file newest-wins. Content-addressing makes blobs immutable, so
    /// concurrent writers always produce identical bytes; merge never applies.
    fn append_only(&self) -> bool {
        false
    }

    fn is_session_file(&self, path: &Path) -> bool {
        path.file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(is_sha256_hex)
    }
}
#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::adapter_for;

    const HASH: &str = "a2a7f46769739a24d0d13eb5544a6041f830ac69395805c2da51d8de11b62711";

    #[test]
    fn identifies_bare_hash_and_extension_alias() {
        let root = Path::new("/home/simon/.omp/agent/blobs");
        let adapter = adapter_for("omp-blobs", root).unwrap();
        assert_eq!(adapter.agent(), "omp-blobs");

        for name in [HASH.to_string(), format!("{HASH}.png")] {
            let path = root.join(&name);
            let id = adapter.identify(&path).unwrap();
            assert_eq!(id.session_id, HASH);
            assert_eq!(id.project_id, "");
            assert_eq!(id.relative_path, Path::new(&name));
            assert!(adapter.is_session_file(&path));
        }
    }

    #[test]
    fn rejects_nested_and_non_hex_names() {
        let root = Path::new("/blobs");
        let adapter = adapter_for("omp-blobs", root).unwrap();

        // nested paths and non-hash names are not blobs
        assert!(adapter.identify(&root.join("sub").join(HASH)).is_err());
        assert!(adapter.identify(&root.join("notes.txt")).is_err());
        // uppercase hex never appears in the store; reject it
        assert!(adapter.identify(&root.join(HASH.to_uppercase())).is_err());
        // truncated hash
        assert!(adapter.identify(&root.join(&HASH[..40])).is_err());

        assert!(!adapter.is_session_file(&root.join("notes.txt")));
        assert!(!adapter.is_session_file(&root.join(".tmp-partial")));
    }

    #[test]
    fn immutable_store_contract() {
        let root = Path::new("/blobs");
        let adapter = adapter_for("omp-blobs", root).unwrap();
        let path = root.join(HASH);

        // whole-file newest-wins; line-union merge never applies
        assert!(!adapter.append_only());
        // None keeps blobs out of `cleanup --before` (age-based selection)
        assert!(adapter.created_at(&path).is_none());
        // None keeps blobs out of `cleanup --unnamed`
        assert!(adapter.title(&path).is_none());
    }
}
