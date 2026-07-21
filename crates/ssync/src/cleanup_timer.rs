//! `ssync cleanup-timer enable|disable|status` — scheduled auto-cleanup via a
//! systemd timer/service pair running `ssync cleanup --apply` (issue #24).
//! Scheduling belongs to systemd, not a daemon-internal scheduler. Deletions
//! propagate mesh-wide through the daemon's tombstone path: a timer on ONE
//! machine prunes ALL machines.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};

use crate::systemd::{
    self, Activation, HARDENING, RemoveSpec, UnitFile, UnitSet, ensure_unit_safe,
};

const SERVICE_NAME: &str = "ssync-cleanup.service";
const TIMER_NAME: &str = "ssync-cleanup.timer";

/// How the timer fires. Calendar schedules get `Persistent=true` (a machine
/// that slept through the window catches up); day counts `OnCalendar` cannot
/// express become monotonic timers instead.
#[derive(Debug, PartialEq)]
pub enum Schedule {
    /// A systemd calendar expression (`weekly`, `*-*-* 03:00:00`, ...).
    Calendar(String),
    /// Fire every N days of unit uptime (`OnUnitActiveSec`), first run
    /// shortly after boot.
    EveryDays(u64),
}

/// Parse `--every`: `<n>d`/`<n>w` durations or a calendar expression passed
/// through verbatim (validated against `systemd-analyze` at enable time).
/// Expressible durations (1d, 1w, 7d) become the equivalent calendar
/// shorthand so they gain catch-up semantics.
pub fn schedule_of(every: &str) -> Result<Schedule> {
    let days = |suffix: char, per: u64| {
        every
            .strip_suffix(suffix)
            .and_then(|n| n.parse::<u64>().ok())
            .map(|n| (n, per))
    };
    if let Some((n, per)) = days('d', 1).or_else(|| days('w', 7)) {
        let n = n
            .checked_mul(per)
            .with_context(|| format!("--every {every} is out of range"))?;
        ensure!(n > 0, "--every {every} never fires");
        return Ok(match n {
            1 => Schedule::Calendar("daily".into()),
            7 => Schedule::Calendar("weekly".into()),
            _ => Schedule::EveryDays(n),
        });
    }
    ensure!(!every.trim().is_empty(), "--every must not be empty");
    Ok(Schedule::Calendar(every.to_string()))
}

/// Everything that varies between the user-mode and system-mode unit pair.
pub struct TimerSpec<'a> {
    /// Absolute path of the ssync binary to run.
    pub exec: &'a Path,
    /// Config file cleanup runs with (embedded in `ExecStart`).
    pub config_path: &'a Path,
    /// Extra `cleanup` arguments (`--keep 90d`, `--unnamed`, `--agent pi`);
    /// every token already validated unit-safe.
    pub cleanup_args: Vec<String>,
    /// The agents' session dirs — all the sandbox may write.
    pub session_dirs: Vec<PathBuf>,
    /// `Some(name)` renders a system unit with `User=`; `None` a user unit.
    pub user: Option<&'a str>,
    pub schedule: Schedule,
}

pub fn validate_spec(spec: &TimerSpec<'_>) -> Result<()> {
    ensure_unit_safe(&spec.exec.display().to_string(), "binary path")?;
    ensure_unit_safe(&spec.config_path.display().to_string(), "config path")?;
    for a in &spec.cleanup_args {
        ensure_unit_safe(a, "cleanup argument")?;
    }
    for p in &spec.session_dirs {
        ensure_unit_safe(&p.display().to_string(), "session dir")?;
    }
    if let Some(u) = &spec.user {
        ensure_unit_safe(u, "user name")?;
    }
    if let Schedule::Calendar(expr) = &spec.schedule {
        // spaces are legal in OnCalendar; only quoting/specifier chars are not
        ensure!(
            !expr.contains(['%', '"', '\\', '\n']),
            "schedule `{expr}` contains characters a systemd unit cannot carry"
        );
    }
    Ok(())
}

/// Render `ssync-cleanup.service`: a oneshot under the same hardening set as
/// the daemon unit, sandboxed to the session dirs (cleanup only deletes local
/// files; the running daemon tombstones and propagates).
pub fn render_service(spec: &TimerSpec<'_>) -> String {
    let user = spec.user.map(|u| format!("User={u}\n")).unwrap_or_default();
    let args = spec
        .cleanup_args
        .iter()
        .map(|a| format!(" {a}"))
        .collect::<String>();
    let rw_paths = spec
        .session_dirs
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\n\
         Description=ssync scheduled session cleanup\n\
         After=ssync.service\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart=\"{exec}\" --config \"{config}\" cleanup{args} --apply\n\
         {user}\
         ReadWritePaths={rw_paths}\n\
         {HARDENING}",
        exec = spec.exec.display(),
        config = spec.config_path.display(),
    )
}

/// Render `ssync-cleanup.timer`.
pub fn render_timer(spec: &TimerSpec<'_>) -> String {
    let trigger = match &spec.schedule {
        Schedule::Calendar(expr) => format!("OnCalendar={expr}\nPersistent=true\n"),
        Schedule::EveryDays(n) => format!("OnBootSec=15min\nOnUnitActiveSec={n}d\n"),
    };
    format!(
        "[Unit]\n\
         Description=ssync scheduled session cleanup timer\n\
         \n\
         [Timer]\n\
         {trigger}\
         RandomizedDelaySec=1h\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n"
    )
}

/// The cleanup selectors the timer runs with (`ssync cleanup-timer enable
/// --keep/--unnamed/--agent`).
pub struct CleanupSelectors {
    pub keep: Option<String>,
    pub unnamed: bool,
    pub agent: Option<String>,
}

/// The `cleanup` arguments the timer runs with. `--keep` defaults to 90d
/// unless `--unnamed` is given (then age is not a criterion).
pub fn cleanup_args_of(sel: CleanupSelectors) -> Result<Vec<String>> {
    let keep = match sel.keep {
        Some(k) => Some(k),
        None if sel.unnamed => None,
        None => Some("90d".to_string()),
    };
    let mut args = Vec::new();
    if let Some(k) = &keep {
        ssync_core::cleanup::parse_keep(k)?;
        args.extend(["--keep".to_string(), k.clone()]);
    }
    if sel.unnamed {
        args.push("--unnamed".to_string());
    }
    if let Some(a) = sel.agent {
        args.extend(["--agent".to_string(), a]);
    }
    Ok(args)
}

/// Reject a calendar expression systemd cannot parse — better one loud
/// enable error than a timer that never fires.
fn validate_calendar(expr: &str) -> Result<()> {
    let out = Command::new("systemd-analyze")
        .args(["calendar", expr])
        .output()
        .context("running systemd-analyze (is systemd available?)")?;
    ensure!(
        out.status.success(),
        "invalid calendar expression `{expr}`: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

pub fn cmd_enable(
    config_path: &Path,
    config_explicit: bool,
    every: String,
    selectors: CleanupSelectors,
    user: Option<String>,
) -> Result<()> {
    let installed = systemd::install(
        config_path,
        config_explicit,
        user,
        move |config, context| {
            if let Some(agent) = &selectors.agent {
                ensure!(
                    config.agents.iter().any(|entry| entry.agent == *agent),
                    "agent {agent:?} is not in the config"
                );
            }
            let schedule = schedule_of(&every)?;
            if let Schedule::Calendar(expression) = &schedule {
                validate_calendar(expression)?;
            }
            let cleanup_args = cleanup_args_of(selectors)?;
            let session_dirs: Vec<PathBuf> = config
                .agents
                .iter()
                .map(|agent| agent.session_dir.clone())
                .collect();
            let spec = TimerSpec {
                exec: context.exec,
                config_path: context.config_path,
                cleanup_args,
                session_dirs,
                user: context.user,
                schedule,
            };
            validate_spec(&spec)?;
            let service_contents = render_service(&spec);
            let timer_contents = render_timer(&spec);
            Ok(UnitSet {
                files: vec![
                    UnitFile {
                        name: SERVICE_NAME,
                        contents: service_contents,
                    },
                    UnitFile {
                        name: TIMER_NAME,
                        contents: timer_contents,
                    },
                ],
                write_paths: spec.session_dirs,
                required_executables: Vec::new(),
                activation: Activation::EnableNow(TIMER_NAME),
            })
        },
    )?;

    let scope = if installed.user_mode { "--user " } else { "" };
    println!("installed and started {TIMER_NAME}");
    println!(
        "cleanup deletes sessions on EVERY machine (the daemon propagates \
         deletions); one machine with a timer is enough"
    );
    println!("check: systemctl {scope}list-timers {TIMER_NAME}");
    Ok(())
}

pub fn cmd_disable() -> Result<()> {
    systemd::remove(RemoveSpec {
        primary: TIMER_NAME,
        files: &[TIMER_NAME, SERVICE_NAME],
        missing_action: "disable",
    })?;
    println!("removed {TIMER_NAME}");
    Ok(())
}

pub fn cmd_status() -> Result<()> {
    if !systemd::show_status(TIMER_NAME)? {
        println!("cleanup timer not installed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(keep: Option<String>, unnamed: bool, agent: Option<String>) -> CleanupSelectors {
        CleanupSelectors {
            keep,
            unnamed,
            agent,
        }
    }

    fn spec(schedule: Schedule, user: Option<&'static str>) -> TimerSpec<'static> {
        TimerSpec {
            exec: Path::new("/usr/local/bin/ssync"),
            config_path: Path::new("/home/alice/.config/ssync/config.toml"),
            cleanup_args: vec!["--keep".into(), "90d".into()],
            session_dirs: vec![
                PathBuf::from("/home/alice/.pi/agent/sessions"),
                PathBuf::from("/home/alice/.omp/agent/sessions"),
            ],
            user,
            schedule,
        }
    }

    #[test]
    fn schedule_maps_expressible_durations_to_calendar() {
        assert_eq!(
            schedule_of("1d").unwrap(),
            Schedule::Calendar("daily".into())
        );
        assert_eq!(
            schedule_of("7d").unwrap(),
            Schedule::Calendar("weekly".into())
        );
        assert_eq!(
            schedule_of("1w").unwrap(),
            Schedule::Calendar("weekly".into())
        );
    }

    #[test]
    fn schedule_maps_other_durations_to_monotonic() {
        assert_eq!(schedule_of("2d").unwrap(), Schedule::EveryDays(2));
        assert_eq!(schedule_of("2w").unwrap(), Schedule::EveryDays(14));
    }

    #[test]
    fn schedule_passes_calendar_expressions_through() {
        assert_eq!(
            schedule_of("weekly").unwrap(),
            Schedule::Calendar("weekly".into())
        );
        assert_eq!(
            schedule_of("*-*-* 03:00:00").unwrap(),
            Schedule::Calendar("*-*-* 03:00:00".into())
        );
    }

    #[test]
    fn schedule_rejects_zero_and_empty() {
        assert!(schedule_of("0d").is_err());
        assert!(schedule_of("").is_err());
        assert!(schedule_of("  ").is_err());
    }

    #[test]
    fn schedule_rejects_overflowing_counts() {
        // u64-parseable count whose *7 overflows
        assert!(schedule_of("9999999999999999999w").is_err());
    }

    #[test]
    fn cleanup_args_default_retention_is_90d() {
        assert_eq!(
            cleanup_args_of(sel(None, false, None)).unwrap(),
            vec!["--keep", "90d"]
        );
    }

    #[test]
    fn cleanup_args_unnamed_only_skips_retention() {
        assert_eq!(
            cleanup_args_of(sel(None, true, None)).unwrap(),
            vec!["--unnamed"]
        );
    }

    #[test]
    fn cleanup_args_combine_all_selectors() {
        assert_eq!(
            cleanup_args_of(sel(Some("30d".into()), true, Some("pi".into()))).unwrap(),
            vec!["--keep", "30d", "--unnamed", "--agent", "pi"]
        );
    }

    #[test]
    fn cleanup_args_reject_bad_keep() {
        assert!(cleanup_args_of(sel(Some("soon".into()), false, None)).is_err());
    }

    #[test]
    fn service_unit_runs_oneshot_cleanup_with_apply() {
        let unit = render_service(&spec(Schedule::Calendar("weekly".into()), None));
        assert!(unit.contains("Type=oneshot\n"));
        assert!(unit.contains(
            "ExecStart=\"/usr/local/bin/ssync\" --config \
             \"/home/alice/.config/ssync/config.toml\" cleanup --keep 90d --apply\n"
        ));
        assert!(unit.contains("After=ssync.service\n"));
        assert!(unit.contains(
            "ReadWritePaths=/home/alice/.pi/agent/sessions /home/alice/.omp/agent/sessions\n"
        ));
        // hardening parity with the daemon unit
        assert!(unit.contains("ProtectSystem=strict\n"));
        assert!(!unit.contains("User="));
    }

    #[test]
    fn system_service_unit_carries_user() {
        let unit = render_service(&spec(Schedule::Calendar("weekly".into()), Some("alice")));
        assert!(unit.contains("User=alice\n"));
    }

    #[test]
    fn calendar_timer_is_persistent() {
        let unit = render_timer(&spec(Schedule::Calendar("weekly".into()), None));
        assert!(unit.contains("OnCalendar=weekly\n"));
        assert!(unit.contains("Persistent=true\n"));
        assert!(unit.contains("RandomizedDelaySec=1h\n"));
        assert!(unit.contains("WantedBy=timers.target\n"));
        assert!(!unit.contains("OnUnitActiveSec"));
    }

    #[test]
    fn monotonic_timer_fires_on_uptime() {
        let unit = render_timer(&spec(Schedule::EveryDays(2), None));
        assert!(unit.contains("OnBootSec=15min\n"));
        assert!(unit.contains("OnUnitActiveSec=2d\n"));
        assert!(!unit.contains("Persistent="));
    }

    #[test]
    fn validate_rejects_unit_hostile_values() {
        let mut s = spec(Schedule::Calendar("weekly".into()), None);
        s.cleanup_args = vec!["--agent".into(), "a b".into()];
        assert!(validate_spec(&s).is_err());

        let mut s = spec(Schedule::Calendar("%i daily".into()), None);
        s.cleanup_args.clear();
        assert!(validate_spec(&s).is_err());
    }

    #[test]
    fn validate_allows_spaces_in_calendar_expressions() {
        let s = spec(Schedule::Calendar("*-*-* 03:00:00".into()), None);
        assert!(validate_spec(&s).is_ok());
    }
}
