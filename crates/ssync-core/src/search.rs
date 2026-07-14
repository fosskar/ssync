//! `ssync search`: find sessions by title or project path — a read-only,
//! best-effort view for humans (issue #18). Reads at most the leading header
//! records per file (the sanctioned metadata access, DECISIONS §2); transcript
//! content is never parsed and nothing here feeds back into storage or sync.

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::{Result, bail};
use ssync_adapters::Adapter;

/// One matching session.
#[derive(Debug)]
pub struct Hit {
    pub agent: String,
    pub session_id: String,
    pub path: PathBuf,
    /// User-given title (`None` = agent has no title record; `Some("")` =
    /// present but empty — shown as untitled).
    pub title: Option<String>,
    /// The session's cwd from its header record, for display; the encoded
    /// project dir stands in when unreadable.
    pub project: String,
    pub created_at: Option<SystemTime>,
}

/// Case-insensitive substring search over titles and project paths of every
/// (matching) adapter's sessions, newest first. Artifact files dedupe into
/// their session. Errors only on an unknown `--agent`; unreadable files are
/// skipped (best-effort by design).
pub fn search(adapters: &[Box<dyn Adapter>], query: &str, agent: Option<&str>) -> Result<Vec<Hit>> {
    if let Some(agent) = agent
        && !adapters.iter().any(|a| a.agent() == agent)
    {
        bail!("unknown agent {agent:?} (configured: {:?})", {
            adapters.iter().map(|a| a.agent()).collect::<Vec<_>>()
        });
    }
    let needle = query.to_lowercase();
    let mut hits: Vec<Hit> = Vec::new();
    for adapter in adapters {
        if agent.is_some_and(|a| a != adapter.agent()) {
            continue;
        }
        for path in crate::session_files(adapter.session_root(), adapter.as_ref()) {
            let Ok(id) = adapter.identify(&path) else {
                continue;
            };
            // artifact files share their session's id; keep one hit per
            // session, preferring the file that carries the title record
            if let Some(prev) = hits
                .iter_mut()
                .find(|h| h.agent == id.agent && h.session_id == id.session_id)
            {
                if prev.title.is_none()
                    && let Some(t) = adapter.title(&path)
                {
                    prev.title = Some(t);
                    prev.path = path;
                }
                continue;
            }
            let project = leading_header_cwd(adapter.as_ref(), &path)
                .unwrap_or_else(|| id.project_id.clone());
            hits.push(Hit {
                agent: id.agent,
                session_id: id.session_id,
                title: adapter.title(&path),
                created_at: adapter.created_at(&path),
                project,
                path,
            });
        }
    }
    hits.retain(|h| {
        h.title
            .as_deref()
            .is_some_and(|t| t.to_lowercase().contains(&needle))
            || h.project.to_lowercase().contains(&needle)
    });
    hits.sort_by_key(|h| std::cmp::Reverse(h.created_at));
    Ok(hits)
}

/// The header cwd from at most the first bytes of the file — enough for the
/// leading records, never the transcript.
fn leading_header_cwd(adapter: &dyn Adapter, path: &std::path::Path) -> Option<String> {
    use std::io::Read;
    let mut head = vec![0u8; 16 * 1024];
    let mut f = std::fs::File::open(path).ok()?;
    let n = f.read(&mut head).ok()?;
    head.truncate(n);
    adapter.header_cwd(&head)
}

/// `YYYY-MM-DD` for display (civil-from-days, inverse of the filename math
/// in ssync-adapters).
pub fn date_of(t: SystemTime) -> String {
    let Ok(secs) = t.duration_since(std::time::UNIX_EPOCH) else {
        return "????-??-??".into();
    };
    let days = (secs.as_secs() / 86400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssync_adapters::pi::PiAdapter;
    use std::path::Path;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ssync-search-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_session(root: &Path, project: &str, ts: &str, id: &str, title: &str, cwd: &str) {
        let dir = root.join(project);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{ts}_{id}.jsonl")),
            format!(
                "{{\"type\":\"title\",\"v\":1,\"title\":\"{title}\"}}\n{{\"type\":\"session\",\"version\":3,\"id\":\"{id}\",\"cwd\":\"{cwd}\"}}\n{{\"m\":\"body text never searched\"}}\n"
            ),
        )
        .unwrap();
    }

    fn omp(root: &Path) -> Vec<Box<dyn Adapter>> {
        vec![Box::new(PiAdapter::new("omp", root))]
    }

    #[test]
    fn matches_title_case_insensitively() {
        let root = scratch("title");
        write_session(
            &root,
            "-Projects-a",
            "2026-05-23T06-55-21-771Z",
            "019e539d-f6ab-71ac-be20-d3ae2b23ea4a",
            "NixOS module hardening",
            "/home/x/Projects/a",
        );
        write_session(
            &root,
            "-Projects-b",
            "2026-05-24T06-55-21-771Z",
            "019e539d-f6ab-71ac-be20-d3ae2b23ea4b",
            "unrelated",
            "/home/x/Projects/b",
        );
        let hits = search(&omp(&root), "nixos", None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title.as_deref(), Some("NixOS module hardening"));
        assert_eq!(hits[0].project, "/home/x/Projects/a");
    }

    #[test]
    fn matches_project_path_for_untitled_sessions() {
        let root = scratch("proj");
        write_session(
            &root,
            "-work-nixfiles",
            "2026-05-23T06-55-21-771Z",
            "019e539d-f6ab-71ac-be20-d3ae2b23ea4a",
            "",
            "/home/x/work/nixfiles",
        );
        let hits = search(&omp(&root), "nixfiles", None).unwrap();
        assert_eq!(hits.len(), 1, "project path must match");
        // body text must never match
        assert!(search(&omp(&root), "body text", None).unwrap().is_empty());
    }

    #[test]
    fn newest_first_and_artifacts_dedupe() {
        let root = scratch("order");
        for (ts, id, title) in [
            (
                "2026-05-23T06-55-21-771Z",
                "019e539d-f6ab-71ac-be20-d3ae2b23ea4a",
                "old nix",
            ),
            (
                "2026-06-23T06-55-21-771Z",
                "019e539d-f6ab-71ac-be20-d3ae2b23ea4b",
                "new nix",
            ),
        ] {
            write_session(&root, "-p", ts, id, title, "/home/x/p");
        }
        // artifact file inside the old session's artifact dir
        let art = root.join("-p/2026-05-23T06-55-21-771Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a");
        std::fs::create_dir_all(&art).unwrap();
        std::fs::write(art.join("__advisor.jsonl"), b"{\"a\":1}\n").unwrap();

        let hits = search(&omp(&root), "nix", None).unwrap();
        assert_eq!(hits.len(), 2, "artifact must not add a hit");
        assert_eq!(hits[0].title.as_deref(), Some("new nix"));
        assert_eq!(hits[1].title.as_deref(), Some("old nix"));
    }

    #[test]
    fn unknown_agent_filter_errors() {
        let root = scratch("agent");
        assert!(search(&omp(&root), "x", Some("codex")).is_err());
        assert!(search(&omp(&root), "x", Some("omp")).unwrap().is_empty());
    }

    #[test]
    fn date_of_formats_civil_dates() {
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_779_519_321);
        assert_eq!(date_of(t), "2026-05-23");
        assert_eq!(date_of(std::time::UNIX_EPOCH), "1970-01-01");
    }
}
