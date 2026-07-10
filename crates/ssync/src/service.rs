//! `ssync service install|uninstall` — systemd unit management for plain-binary
//! deployments (issue #26). Pure unit rendering + path/mode decisions, with thin
//! `systemctl` wrappers around them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use ssync_core::Config;

/// Everything that varies between the user-mode and system-mode unit.
pub struct ServiceSpec {
    /// Absolute path of the ssync binary to run.
    pub exec: PathBuf,
    /// Config file the daemon runs with (embedded in `ExecStart`).
    pub config_path: PathBuf,
    /// Dirs the sandbox may write: the agents' session dirs plus `data_dir`.
    pub read_write_paths: Vec<PathBuf>,
    /// `PATH` for the daemon: the install-time dirs of `age`/`age-keygen`
    /// plus the distro defaults (what the nix package's wrapper does).
    pub path: String,
    /// `Some(name)` renders a system unit with `User=`; `None` a user unit.
    pub user: Option<String>,
}

/// The hardening property set shared with `nix/nixos-module.nix` and
/// `nix/hm-module.nix` — one template, three consumers; change all three
/// together. The daemon gets RW to `ReadWritePaths`, a tmpfs
/// `RuntimeDirectory` for SecretFile, read access to the secrets it is
/// pointed at, and outbound QUIC/UDP plus netlink for iroh; everything else
/// is denied.
const HARDENING: &str = "\
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=read-only
PrivateTmp=yes
PrivateDevices=yes
ProtectClock=yes
ProtectHostname=yes
ProtectKernelTunables=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectControlGroups=yes
ProtectProc=invisible
ProcSubset=pid
RestrictNamespaces=yes
RestrictRealtime=yes
RestrictSUIDSGID=yes
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK
LockPersonality=yes
MemoryDenyWriteExecute=yes
RemoveIPC=yes
CapabilityBoundingSet=
AmbientCapabilities=
SystemCallFilter=@system-service
SystemCallFilter=~@privileged
SystemCallFilter=~@resources
SystemCallErrorNumber=EPERM
SystemCallArchitectures=native
UMask=0077
";

/// Render the full `ssync.service` unit text for a spec.
pub fn render_unit(spec: &ServiceSpec) -> String {
    // system units pull network-online.target in; user managers don't have it,
    // so a user unit only orders after it if something else provides it.
    let wants = if spec.user.is_some() {
        "Wants=network-online.target\n"
    } else {
        ""
    };
    let user = spec
        .user
        .as_deref()
        .map(|u| format!("User={u}\n"))
        .unwrap_or_default();
    let wanted_by = if spec.user.is_some() {
        "multi-user.target"
    } else {
        "default.target"
    };
    let rw_paths = spec
        .read_write_paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\n\
         Description=ssync coding-agent session sync\n\
         {wants}\
         After=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart=\"{exec}\" --config \"{config}\" daemon\n\
         {user}\
         Restart=on-failure\n\
         RestartSec=5\n\
         Environment=\"MALLOC_ARENA_MAX=2\"\n\
         Environment=\"XDG_RUNTIME_DIR=%t/ssync\"\n\
         Environment=\"PATH={path}\"\n\
         RuntimeDirectory=ssync\n\
         ReadWritePaths={rw_paths}\n\
         {HARDENING}\
         \n\
         [Install]\n\
         WantedBy={wanted_by}\n",
        exec = spec.exec.display(),
        config = spec.config_path.display(),
        path = spec.path,
    )
}

/// Where the user-manager unit lives (`$XDG_CONFIG_HOME/systemd/user/`).
pub fn user_unit_path(config_dir: &Path) -> PathBuf {
    config_dir.join("systemd/user/ssync.service")
}

const SYSTEM_UNIT_PATH: &str = "/etc/systemd/system/ssync.service";
const UNIT_NAME: &str = "ssync.service";

/// System-mode configs must not use `~/` paths: install expands them with
/// root's home, the daemon with `User=`'s — silently different sandboxes.
pub fn config_uses_tilde(raw: &str) -> bool {
    raw.contains("\"~/") || raw.contains("'~/")
}

/// Effective uid, via the ownership of this process's own /proc entry
/// (std has no geteuid, and the workspace forbids unsafe/libc).
fn is_root() -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata("/proc/self").context("reading /proc/self (uid check)")?;
    Ok(meta.uid() == 0)
}

/// The daemon's unit `PATH`: where `age`/`age-keygen` live *now* (the daemon
/// shells out to them; a nix-profile or devshell install has them nowhere a
/// unit's default PATH reaches) plus the distro defaults. Errors when age is
/// missing — better one install-time failure than a crash-looping unit.
fn daemon_path(search_path: &str) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let mut dirs: Vec<PathBuf> = Vec::new();
    for bin in ["age", "age-keygen"] {
        let dir = std::env::split_paths(search_path)
            .find(|d| {
                fs::metadata(d.join(bin))
                    .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            })
            .with_context(|| format!("{bin} not found on PATH (age >= 1.3 required)"))?;
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    let mut path = std::env::join_paths(dirs)
        .context("age dir not usable in PATH")?
        .into_string()
        .map_err(|_| anyhow::anyhow!("age dir is not valid unicode"))?;
    path.push_str(":/usr/local/bin:/usr/bin:/bin");
    Ok(path)
}

fn systemctl(user_mode: bool, args: &[&str]) -> Result<()> {
    let mut cmd = Command::new("systemctl");
    if user_mode {
        cmd.arg("--user");
    }
    let status = cmd
        .args(args)
        .status()
        .context("running systemctl (is systemd available?)")?;
    ensure!(status.success(), "systemctl {} failed", args.join(" "));
    Ok(())
}

/// Session dirs + data dir: what the sandbox must be able to write. Created
/// 0700 up front — `ReadWritePaths` requires them to exist at unit start
/// (same job as the nix modules' tmpfiles rules).
fn write_paths_of(config: &Config) -> Vec<PathBuf> {
    config
        .agents
        .iter()
        .map(|a| a.session_dir.clone())
        .chain([config.data_dir.clone()])
        .collect()
}

/// Path components `create_dir_all` would have to create, leaf first.
fn missing_components(p: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut cur = p;
    while !cur.exists() {
        missing.push(cur.to_path_buf());
        match cur.parent() {
            Some(parent) => cur = parent,
            None => break,
        }
    }
    missing
}

/// The service user's uid/gid, via `id` (std has no passwd lookup).
fn uid_gid_of(user: &str) -> Result<(u32, u32)> {
    let lookup = |flag: &str| -> Result<u32> {
        let out = Command::new("id")
            .args([flag, user])
            .output()
            .context("running id")?;
        ensure!(out.status.success(), "unknown user {user}");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .with_context(|| format!("parsing `id {flag} {user}` output"))
    };
    Ok((lookup("-u")?, lookup("-g")?))
}

/// Create the write paths 0700. A root (system-mode) install chowns every
/// component it created to the service user — the daemon runs as `User=` and
/// root-owned dirs would fail it on first start.
fn create_write_paths(paths: &[PathBuf], owner: Option<(u32, u32)>) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    for p in paths {
        let created = missing_components(p);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(p)
            .with_context(|| format!("creating {}", p.display()))?;
        if let Some((uid, gid)) = owner {
            for dir in &created {
                std::os::unix::fs::chown(dir, Some(uid), Some(gid))
                    .with_context(|| format!("chowning {}", dir.display()))?;
            }
        }
    }
    Ok(())
}

pub fn cmd_service_install(
    config_path: &Path,
    config_explicit: bool,
    user: Option<String>,
) -> Result<()> {
    let user_mode = !is_root()?;
    if user_mode {
        ensure!(
            user.is_none(),
            "--user only applies to a system unit; run as root to install one"
        );
    } else {
        ensure!(
            user.is_some(),
            "a system unit needs an explicit --user <name> to run the daemon as \
             (sessions, keys, and watched dirs are per-user)"
        );
        ensure!(
            config_explicit,
            "system mode needs an explicit --config: root's default config path \
             is not readable by the service user"
        );
        let raw = fs::read_to_string(config_path)
            .with_context(|| format!("reading {}", config_path.display()))?;
        ensure!(
            !config_uses_tilde(&raw),
            "system-mode config must use absolute paths: `~/` expands to root's \
             home at install time but to --user's home in the daemon"
        );
    }

    let config = Config::load(config_path)
        .with_context(|| format!("loading {} (run `ssync init` first)", config_path.display()))?;
    let owner = match &user {
        Some(name) => Some(uid_gid_of(name)?),
        None => None,
    };
    let paths = write_paths_of(&config);
    create_write_paths(&paths, owner)?;

    let exec = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("resolving the ssync binary path")?;
    let spec = ServiceSpec {
        exec,
        config_path: config_path
            .canonicalize()
            .with_context(|| format!("resolving {}", config_path.display()))?,
        read_write_paths: paths,
        path: daemon_path(&std::env::var("PATH").context("reading PATH")?)?,
        user,
    };

    let unit_path = if user_mode {
        user_unit_path(&dirs::config_dir().context("no config dir")?)
    } else {
        PathBuf::from(SYSTEM_UNIT_PATH)
    };
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&unit_path, render_unit(&spec))
        .with_context(|| format!("writing {}", unit_path.display()))?;

    systemctl(user_mode, &["daemon-reload"])?;
    systemctl(user_mode, &["enable", "--now", UNIT_NAME])?;

    let scope = if user_mode { "--user " } else { "" };
    println!("installed and started {}", unit_path.display());
    println!("check: systemctl {scope}status ssync");
    Ok(())
}

pub fn cmd_service_uninstall() -> Result<()> {
    let user_mode = !is_root()?;
    let unit_path = if user_mode {
        user_unit_path(&dirs::config_dir().context("no config dir")?)
    } else {
        PathBuf::from(SYSTEM_UNIT_PATH)
    };
    if !unit_path.exists() {
        bail!(
            "nothing to uninstall: {} does not exist",
            unit_path.display()
        );
    }
    // best effort: a masked/never-enabled unit must not block removal
    if let Err(e) = systemctl(user_mode, &["disable", "--now", UNIT_NAME]) {
        eprintln!("ssync: {e}");
    }
    fs::remove_file(&unit_path).with_context(|| format!("removing {}", unit_path.display()))?;
    systemctl(user_mode, &["daemon-reload"])?;
    println!("removed {}", unit_path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(user: Option<&str>) -> ServiceSpec {
        ServiceSpec {
            exec: PathBuf::from("/opt/bin/ssync"),
            config_path: PathBuf::from("/home/alice/.config/ssync/config.toml"),
            read_write_paths: vec![
                PathBuf::from("/home/alice/.pi/agent/sessions"),
                PathBuf::from("/home/alice/.local/share/ssync"),
            ],
            path: "/opt/agedir:/usr/local/bin:/usr/bin:/bin".into(),
            user: user.map(String::from),
        }
    }

    #[test]
    fn user_unit_runs_daemon_with_config() {
        let unit = render_unit(&spec(None));
        assert!(unit.contains(
            "ExecStart=\"/opt/bin/ssync\" --config \"/home/alice/.config/ssync/config.toml\" daemon"
        ));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(!unit.contains("\nUser="));
        // user managers have no network-online.target to pull in
        assert!(!unit.contains("Wants=network-online.target"));
        assert!(unit.contains("Environment=\"PATH=/opt/agedir:/usr/local/bin:/usr/bin:/bin\""));
    }

    #[test]
    fn system_unit_sets_user_and_system_target() {
        let unit = render_unit(&spec(Some("alice")));
        assert!(unit.contains("\nUser=alice\n"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        assert!(unit.contains("Wants=network-online.target"));
    }

    #[test]
    fn unit_opens_exactly_the_needed_write_paths() {
        let unit = render_unit(&spec(None));
        assert!(unit.contains(
            "ReadWritePaths=/home/alice/.pi/agent/sessions /home/alice/.local/share/ssync"
        ));
    }

    #[test]
    fn unit_carries_the_shared_hardening_set() {
        let unit = render_unit(&spec(None));
        // spot-check the load-bearing properties (parity with the nix modules)
        for needle in [
            "ProtectSystem=strict",
            "ProtectHome=read-only",
            "NoNewPrivileges=yes",
            "SystemCallFilter=@system-service",
            "RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK",
            "MemoryDenyWriteExecute=yes",
            "UMask=0077",
            // SecretFile needs a writable tmpfs under ProtectSystem=strict
            "RuntimeDirectory=ssync",
            "Environment=\"XDG_RUNTIME_DIR=%t/ssync\"",
            "Environment=\"MALLOC_ARENA_MAX=2\"",
        ] {
            assert!(unit.contains(needle), "missing {needle} in unit:\n{unit}");
        }
    }

    #[test]
    fn user_unit_path_is_under_the_user_manager_dir() {
        assert_eq!(
            user_unit_path(Path::new("/home/alice/.config")),
            PathBuf::from("/home/alice/.config/systemd/user/ssync.service")
        );
    }

    #[test]
    fn tilde_detection_matches_both_toml_string_kinds() {
        assert!(config_uses_tilde("data_dir = \"~/.local/share/ssync\"\n"));
        assert!(config_uses_tilde("data_dir = '~/.local/share/ssync'\n"));
        assert!(!config_uses_tilde("data_dir = \"/var/lib/ssync\"\n"));
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ssync-service-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_components_lists_only_what_create_would_add() {
        let base = scratch("missing");
        let leaf = base.join("a/b");
        assert_eq!(
            missing_components(&leaf),
            vec![leaf.clone(), base.join("a")]
        );
        assert!(missing_components(&base).is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn write_paths_are_created_private() {
        use std::os::unix::fs::PermissionsExt;
        let base = scratch("create");
        let leaf = base.join("nested/sessions");
        create_write_paths(std::slice::from_ref(&leaf), None).unwrap();
        let mode = std::fs::metadata(&leaf).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "mode {mode:o}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn daemon_path_pins_the_age_dir_and_keeps_distro_defaults() {
        use std::os::unix::fs::PermissionsExt;
        let base = scratch("agedir");
        for bin in ["age", "age-keygen"] {
            let p = base.join(bin);
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let search = format!("/nonexistent:{}", base.display());
        assert_eq!(
            daemon_path(&search).unwrap(),
            format!("{}:/usr/local/bin:/usr/bin:/bin", base.display())
        );
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn daemon_path_fails_early_without_age() {
        let err = daemon_path("/nonexistent").unwrap_err();
        assert!(err.to_string().contains("age"), "{err}");
    }
}
