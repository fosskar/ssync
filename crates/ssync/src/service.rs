//! `ssync service install|uninstall` — systemd unit management for plain-binary
//! deployments (issue #26). Pure unit rendering + path/mode decisions, with thin
//! `systemctl` wrappers around them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use ssync_core::Config;

use crate::systemd::{
    self, Activation, HARDENING, RemoveResult, UnitFile, UnitSet, ensure_unit_safe,
};

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

pub fn validate_spec(spec: &ServiceSpec) -> Result<()> {
    let check = ensure_unit_safe;
    check(&spec.exec.display().to_string(), "binary path")?;
    check(&spec.config_path.display().to_string(), "config path")?;
    for p in &spec.read_write_paths {
        check(&p.display().to_string(), "write path")?;
    }
    check(&spec.path, "unit PATH")?;
    if let Some(u) = &spec.user {
        check(u, "user name")?;
    }
    Ok(())
}

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

const UNIT_NAME: &str = "ssync.service";

/// The daemon's unit `PATH`: where `age`/`age-keygen` live *now* (the daemon
/// shells out to them; a nix-profile or devshell install has them nowhere a
/// unit's default PATH reaches) plus the distro defaults. Errors when age is
/// missing — better one install-time failure than a crash-looping unit.
/// Also returns the resolved binaries so a system-mode install can verify
/// the service user may execute them.
fn daemon_path(search_path: &str) -> Result<(String, Vec<PathBuf>)> {
    use std::os::unix::fs::PermissionsExt;
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut bins: Vec<PathBuf> = Vec::new();
    for bin in ["age", "age-keygen"] {
        let dir = std::env::split_paths(search_path)
            .find(|d| {
                fs::metadata(d.join(bin))
                    .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            })
            .with_context(|| format!("{bin} not found on PATH (age >= 1.3 required)"))?;
        bins.push(dir.join(bin));
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    let mut path = std::env::join_paths(dirs)
        .context("age dir not usable in PATH")?
        .into_string()
        .map_err(|_| anyhow!("age dir is not valid unicode"))?;
    path.push_str(":/usr/local/bin:/usr/bin:/bin");
    Ok((path, bins))
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

pub fn cmd_service_install(
    config_path: &Path,
    config_explicit: bool,
    user: Option<String>,
) -> Result<()> {
    let installed = systemd::install(config_path, config_explicit, user, |config, context| {
        let (path, age_bins) = daemon_path(&std::env::var("PATH").context("reading PATH")?)?;
        let write_paths = write_paths_of(config);
        let spec = ServiceSpec {
            exec: context.exec,
            config_path: context.config_path,
            read_write_paths: write_paths.clone(),
            path,
            user: context.user,
        };
        validate_spec(&spec)?;
        Ok(UnitSet {
            files: vec![UnitFile {
                name: UNIT_NAME,
                contents: render_unit(&spec),
            }],
            write_paths,
            required_executables: age_bins,
            activation: Activation::Restart(UNIT_NAME),
        })
    })?;

    if installed.user_mode {
        // without linger the user manager (and the daemon with it) dies at
        // logout and is absent after boot until the next login. Best effort:
        // needs a logind session, which sandboxes and CI lack.
        let lingered = Command::new("loginctl")
            .arg("enable-linger")
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !lingered {
            eprintln!(
                "ssync: could not enable linger; the daemon stops at logout — \
                 run `loginctl enable-linger` manually"
            );
        }
    }

    let scope = if installed.user_mode { "--user " } else { "" };
    let unit_path = installed.unit_dir.join(UNIT_NAME);
    println!("installed and started {}", unit_path.display());
    println!("check: systemctl {scope}status ssync");
    Ok(())
}

pub fn cmd_service_uninstall() -> Result<()> {
    match systemd::remove(UNIT_NAME, &[UNIT_NAME])? {
        RemoveResult::Missing(path) => {
            bail!("nothing to uninstall: {} does not exist", path.display())
        }
        RemoveResult::Removed(path) => {
            println!("removed {}", path.display());
            Ok(())
        }
    }
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
        // full line: nothing beyond the spec'd paths may be writable
        assert!(unit.contains(
            "\nReadWritePaths=/home/alice/.pi/agent/sessions /home/alice/.local/share/ssync\n"
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

    /// The three copies of the hardening set (this unit, the NixOS module,
    /// the home-manager module) must not drift apart silently.
    #[test]
    fn hardening_set_matches_the_nix_modules() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for module in ["nix/nixos-module.nix", "nix/hm-module.nix"] {
            let nix = std::fs::read_to_string(root.join(module)).unwrap();
            for line in HARDENING.lines() {
                let key = line.split_once('=').unwrap().0;
                assert!(
                    nix.contains(key),
                    "{module} missing hardening property {key}"
                );
            }
        }
    }

    #[test]
    fn spec_validation_rejects_unit_hostile_characters() {
        validate_spec(&spec(None)).unwrap();
        // systemd splits ReadWritePaths on whitespace
        let mut s = spec(None);
        s.read_write_paths[0] = PathBuf::from("/home/al ice/sessions");
        assert!(validate_spec(&s).is_err());
        // `%` is a specifier in every interpolated setting
        let mut s = spec(None);
        s.exec = PathBuf::from("/opt/%i/ssync");
        assert!(validate_spec(&s).is_err());
        // `"` terminates the template's own quoting
        let mut s = spec(None);
        s.path = "/opt/\"quoted\":/usr/bin".into();
        assert!(validate_spec(&s).is_err());
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ssync-service-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
        let (path, bins) = daemon_path(&search).unwrap();
        assert_eq!(
            path,
            format!("{}:/usr/local/bin:/usr/bin:/bin", base.display())
        );
        assert_eq!(bins, vec![base.join("age"), base.join("age-keygen")]);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn daemon_path_fails_early_without_age() {
        let err = daemon_path("/nonexistent").unwrap_err();
        assert!(err.to_string().contains("age"), "{err}");
    }
}
