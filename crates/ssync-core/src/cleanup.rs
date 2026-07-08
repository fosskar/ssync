//! `ssync cleanup`: select and delete local session files; the engine's normal
//! tombstone path propagates the deletions to peers. Selection is by creation
//! time (the adapter's `created_at`, never mtime — the engine's write-backs
//! reset it) and/or by empty session title. Refuses to delete an agent's last
//! sessions: the reconcile wipe guard would silently suppress propagation.

use std::path::PathBuf;
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
        if filter.agent.as_deref().is_some_and(|a| a != adapter.agent()) {
            continue;
        }
        let files = crate::session_files(adapter.session_root(), adapter.as_ref());
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
                "refusing: would delete all {} {} sessions — deleting an agent's \
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
        let named = root.join("--p--").join(
            "2026-01-02T00-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4b.jsonl",
        );
        std::fs::write(&named, "{\"type\":\"title\",\"v\":1,\"title\":\"keep me\"}\n").unwrap();
        // no title record at all (plain pi): kept
        let plain = root.join("--p--").join(
            "2026-01-03T00-00-00-000Z_019e539d-f6ab-71ac-be20-d3ae2b23ea4c.jsonl",
        );
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
        assert!(plan(&adapters(&root), &filter(Some("codex"), Some(SystemTime::now()))).is_err());
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
}
