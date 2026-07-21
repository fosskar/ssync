//! `ssync cleanup`: select and delete local session files; the engine's normal
//! tombstone path propagates the deletions to peers. Selection is by creation
//! time (the adapter's `created_at`, never mtime — the engine's write-backs
//! reset it) and/or by empty session title. Refuses to delete an agent's last
//! sessions: the reconcile wipe guard would silently suppress propagation.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Result, anyhow, bail};
use ssync_adapters::Adapter;

/// What to delete. `before` and `unnamed` combine with AND when both set.
pub struct Filter {
    /// Restrict to this agent name (`None` = all configured agents).
    pub agent: Option<String>,
    /// Sessions created strictly before this instant.
    pub before: Option<SystemTime>,
    /// Sessions whose title record is present but empty (`Some("")`); agents
    /// without title records never match.
    pub unnamed: bool,
}

/// One selected session file.
#[derive(Debug)]
pub struct Victim {
    pub agent: String,
    pub path: PathBuf,
    pub size: u64,
}

/// Parse a `--keep` duration: `<n>` + `d`/`w`/`m`/`y` (m = 30 days).
pub fn parse_keep(s: &str) -> Result<Duration> {
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: u64 = num
        .parse()
        .map_err(|_| anyhow!("invalid duration {s:?} (expected e.g. 30d, 6w, 3m, 1y)"))?;
    let days = match unit {
        "d" => n,
        "w" => n * 7,
        "m" => n * 30,
        "y" => n * 365,
        _ => bail!("invalid duration unit {unit:?} (expected d, w, m or y)"),
    };
    Ok(Duration::from_secs(days * 86400))
}

/// Walk every (matching) adapter's sessions and select the victims.
/// Errors when a filter names an unknown agent, when no selector is set, or
/// when the selection would delete *all* of an agent's sessions (the empty-dir
/// wipe guard would suppress tombstones, so the deletion would not propagate).
pub fn plan(adapters: &[Box<dyn Adapter>], filter: &Filter) -> Result<Vec<Victim>> {
    if filter.before.is_none() && !filter.unnamed {
        bail!("no selector: pass --keep, --before and/or --unnamed");
    }
    if let Some(agent) = &filter.agent
        && !adapters.iter().any(|a| a.agent() == agent)
    {
        bail!("unknown agent {agent:?} (configured: {:?})", {
            adapters.iter().map(|a| a.agent()).collect::<Vec<_>>()
        });
    }
    let mut victims = Vec::new();
    for adapter in adapters {
        if filter
            .agent
            .as_deref()
            .is_some_and(|a| a != adapter.agent())
        {
            continue;
        }
        let files = crate::wiremap::session_files(adapter.session_root(), adapter.as_ref());
        let mut selected = Vec::new();
        for path in &files {
            let old_enough = match filter.before {
                None => true,
                Some(cutoff) => adapter.created_at(path).is_some_and(|t| t < cutoff),
            };
            let unnamed_match = !filter.unnamed || adapter.title(path).as_deref() == Some("");
            if old_enough && unnamed_match {
                selected.push(path.clone());
            }
        }
        if !files.is_empty() && selected.len() == files.len() {
            bail!(
                "refusing: would delete all {} {} session files — deleting an agent's \
                 last session does not propagate (empty-dir wipe guard); keep at \
                 least one or narrow the filter",
                files.len(),
                adapter.agent()
            );
        }
        for path in selected {
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            victims.push(Victim {
                agent: adapter.agent().to_string(),
                path,
                size,
            });
        }
    }
    Ok(victims)
}
/// Best-effort sweep of now-empty parent dirs after deleting a session file,
/// up to (never including) the session root: an omp artifact dir whose files
/// were all deleted is residue, not a session (DECISIONS §9).
pub fn remove_empty_parents(path: &Path, root: &Path) {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) {
            break;
        }
        if std::fs::remove_dir(d).is_err() {
            break; // not empty (or already gone)
        }
        dir = d.parent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssync_adapters::pi::PiAdapter;
    use std::path::Path;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ssync-cleanup-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("--p--")).unwrap();
        dir
    }

    fn session(root: &Path, ts: &str, body: &str) -> PathBuf {
        let path = root
            .join("--p--")
            .join(format!("{ts}_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl"));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn adapters(root: &Path) -> Vec<Box<dyn Adapter>> {
        vec![Box::new(PiAdapter::new("pi", root))]
    }

    #[test]
    fn keep_durations_parse() {
        assert_eq!(parse_keep("30d").unwrap(), Duration::from_secs(30 * 86400));
        assert_eq!(parse_keep("6w").unwrap(), Duration::from_secs(42 * 86400));
        assert_eq!(parse_keep("1m").unwrap(), Duration::from_secs(30 * 86400));
        assert_eq!(parse_keep("1y").unwrap(), Duration::from_secs(365 * 86400));
        assert!(parse_keep("7x").is_err());
        assert!(parse_keep("").is_err());
        assert!(parse_keep("d").is_err());
    }

    #[test]
    fn selects_by_creation_time_not_mtime() {
        let root = scratch("time");
        let old = session(&root, "2026-01-01T00-00-00-000Z", "{}\n");
        session(&root, "2026-07-01T00-00-00-000Z", "{}\n");
        // cutoff between the two
        let cutoff = SystemTime::UNIX_EPOCH + Duration::from_secs(1_776_000_000); // 2026-04-12
        let victims = plan(
            &adapters(&root),
            &Filter {
                agent: None,
                before: Some(cutoff),
                unnamed: false,
            },
        )
        .unwrap();
        assert_eq!(victims.len(), 1);
        assert_eq!(victims[0].path, old);
        assert_eq!(victims[0].agent, "pi");
    }

    #[test]
    fn unnamed_matches_only_empty_title_records() {
        let root = scratch("unnamed");
        let unnamed = session(
            &root,
            "2026-01-01T00-00-00-000Z",
            "{\"type\":\"title\",\"v\":1,\"title\":\"\",\"pad\":\" \"}\n",
        );
        // named: kept
        let named = root
            .join("--p--")
            .join("2026-01-02T00-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl");
        std::fs::write(
            &named,
            "{\"type\":\"title\",\"v\":1,\"title\":\"keep me\"}\n",
        )
        .unwrap();
        // no title record at all (plain pi): kept
        let plain = root
            .join("--p--")
            .join("2026-01-03T00-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4c.jsonl");
        std::fs::write(&plain, "{\"type\":\"session\",\"version\":3}\n").unwrap();

        let victims = plan(
            &adapters(&root),
            &Filter {
                agent: None,
                before: None,
                unnamed: true,
            },
        )
        .unwrap();
        assert_eq!(victims.len(), 1);
        assert_eq!(victims[0].path, unnamed);
    }

    #[test]
    fn refuses_to_empty_an_agent() {
        let root = scratch("wipe");
        session(&root, "2026-01-01T00-00-00-000Z", "{}\n");
        let err = plan(
            &adapters(&root),
            &Filter {
                agent: None,
                before: Some(SystemTime::now()),
                unnamed: false,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("wipe guard"), "got: {err}");
    }

    #[test]
    fn agent_filter_and_missing_selector_are_validated() {
        let root = scratch("validate");
        session(&root, "2026-01-01T00-00-00-000Z", "{}\n");
        session(&root, "2026-07-01T00-00-00-000Z", "{}\n");
        let filter = |agent: Option<&str>, before: Option<SystemTime>| Filter {
            agent: agent.map(String::from),
            before,
            unnamed: false,
        };
        assert!(plan(&adapters(&root), &filter(None, None)).is_err());
        assert!(
            plan(
                &adapters(&root),
                &filter(Some("codex"), Some(SystemTime::now()))
            )
            .is_err()
        );
        // agent-restricted: selects only from that agent
        let victims = plan(
            &adapters(&root),
            &filter(
                Some("pi"),
                Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_776_000_000)),
            ),
        )
        .unwrap();
        assert_eq!(victims.len(), 1);
    }

    #[test]
    fn before_cutoff_selects_old_session_artifacts_not_new_ones() {
        // omp nests subagent transcripts one level below the main session file:
        // <root>/<enc>/<ts>_<uuid>/<Name>.jsonl. A --before sweep must select an
        // old session's artifact files alongside its main file, and leave a new
        // session's artifact files untouched (created_at from the dir timestamp).
        let root = scratch("nested");
        let enc = root.join("--p--");

        // Old session: main file + nested artifact transcript.
        let old_main =
            enc.join("2026-01-01T00-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a.jsonl");
        std::fs::write(&old_main, "{\"type\":\"session\",\"version\":3}\n").unwrap();
        let old_dir = enc.join("2026-01-01T00-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4a");
        std::fs::create_dir_all(&old_dir).unwrap();
        let old_artifact = old_dir.join("Tests.jsonl");
        std::fs::write(&old_artifact, "{\"type\":\"session\",\"version\":3}\n").unwrap();

        // New session: main file + nested artifact, both after the cutoff.
        let new_main =
            enc.join("2026-07-01T00-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl");
        std::fs::write(&new_main, "{\"type\":\"session\",\"version\":3}\n").unwrap();
        let new_dir = enc.join("2026-07-01T00-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b");
        std::fs::create_dir_all(&new_dir).unwrap();
        let new_artifact = new_dir.join("Tests.jsonl");
        std::fs::write(&new_artifact, "{\"type\":\"session\",\"version\":3}\n").unwrap();

        // cutoff between the two sessions (2026-04-12).
        let cutoff = SystemTime::UNIX_EPOCH + Duration::from_secs(1_776_000_000);
        let victims = plan(
            &adapters(&root),
            &Filter {
                agent: None,
                before: Some(cutoff),
                unnamed: false,
            },
        )
        .unwrap();
        let paths: Vec<_> = victims.iter().map(|v| v.path.clone()).collect();

        // The old session's main file AND its nested artifact are both selected.
        assert!(
            paths.contains(&old_main),
            "old main not selected: {paths:?}"
        );
        assert!(
            paths.contains(&old_artifact),
            "old session artifact not selected: {paths:?}"
        );
        // The new session's files are left alone.
        assert!(!paths.contains(&new_main), "new main wrongly selected");
        assert!(
            !paths.contains(&new_artifact),
            "new session artifact wrongly selected"
        );
    }
    #[test]
    fn remove_empty_parents_sweeps_emptied_dirs_but_never_the_root() {
        let root = scratch("sweep");
        let enc = root.join("--p--");
        let art = enc.join("2026-01-01T00-00-00-000Z_id");
        std::fs::create_dir_all(&art).unwrap();
        let artifact = art.join("Sub.jsonl");
        std::fs::write(&artifact, b"x").unwrap();
        let keep = enc.join("other.jsonl");
        std::fs::write(&keep, b"x").unwrap();

        // artifact dir empties -> swept; <enc> still holds a file -> stays
        std::fs::remove_file(&artifact).unwrap();
        remove_empty_parents(&artifact, &root);
        assert!(!art.exists(), "emptied artifact dir must be removed");
        assert!(enc.exists(), "non-empty <encoded-cwd> dir must stay");

        // last file goes -> <enc> swept too, but never the session root
        std::fs::remove_file(&keep).unwrap();
        remove_empty_parents(&keep, &root);
        assert!(!enc.exists(), "emptied <encoded-cwd> dir must be removed");
        assert!(root.exists(), "session root must never be removed");
    }
}
