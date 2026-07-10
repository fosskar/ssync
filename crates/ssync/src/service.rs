//! `ssync service install|uninstall` — systemd unit management for plain-binary
//! deployments (issue #26). Pure unit rendering + path/mode decisions, with thin
//! `systemctl` wrappers around them.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail, ensure};
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
pub(crate) const HARDENING: &str = "\
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

/// Refuse a value a unit file cannot carry verbatim: systemd expands `%`
/// specifiers in every interpolated setting, splits `ReadWritePaths` on
/// whitespace, and `"`/`\` alter its quoting — better one loud install
/// error than a unit that silently means something else.
pub(crate) fn ensure_unit_safe(value: &str, what: &str) -> Result<()> {
    ensure!(
        !value.contains(['%', '"', '\\']) && !value.chars().any(|c| c.is_whitespace()),
        "{what} `{value}` contains characters a systemd unit cannot carry \
         (whitespace, %, \", \\)"
    );
    Ok(())
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

/// Where the user-manager units live (`$XDG_CONFIG_HOME/systemd/user/`).
pub fn user_unit_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("systemd/user")
}

/// The directory whose units the current invocation manages.
pub(crate) fn unit_dir(user_mode: bool) -> Result<PathBuf> {
    Ok(if user_mode {
        user_unit_dir(&dirs::config_dir().context("no config dir")?)
    } else {
        PathBuf::from(SYSTEM_UNIT_DIR)
    })
}

const SYSTEM_UNIT_DIR: &str = "/etc/systemd/system";
const UNIT_NAME: &str = "ssync.service";

/// System-mode configs must not use `~/` paths: install expands them with
/// root's home, the daemon with `User=`'s — silently different sandboxes.
pub fn config_uses_tilde(raw: &str) -> bool {
    ["\"~/", "'~/", "\"~\"", "'~'"]
        .iter()
        .any(|needle| raw.contains(needle))
}

/// Effective uid, via the ownership of this process's own /proc entry
/// (std has no geteuid, and the workspace forbids unsafe/libc).
pub(crate) fn is_root() -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata("/proc/self").context("reading /proc/self (uid check)")?;
    Ok(meta.uid() == 0)
}

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

pub(crate) fn systemctl(user_mode: bool, args: &[&str]) -> Result<()> {
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

/// Identity the system-mode daemon runs as, resolved via `id` (std has no
/// passwd lookup). `groups` includes the primary gid.
pub(crate) struct ServiceUser {
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
}

pub(crate) fn service_user_of(user: &str) -> Result<ServiceUser> {
    let lookup = |flag: &str| -> Result<String> {
        let out = Command::new("id")
            .args([flag, user])
            .output()
            .context("running id")?;
        ensure!(out.status.success(), "unknown user {user}");
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let parse = |s: &str| {
        s.parse::<u32>()
            .with_context(|| format!("parsing id output `{s}`"))
    };
    let groups = lookup("-G")?
        .split_whitespace()
        .map(parse)
        .collect::<Result<Vec<u32>>>()?;
    Ok(ServiceUser {
        uid: parse(&lookup("-u")?)?,
        gid: parse(&lookup("-g")?)?,
        groups,
    })
}

/// Can `who` act on `meta` with `bits`? Owner/group/other mode bits only —
/// ACLs are invisible here, so this can reject a reachable path, never
/// accept an unreachable one.
fn mode_allows(meta: &fs::Metadata, who: &ServiceUser, bits: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    if who.uid == 0 {
        return true;
    }
    let shift = if meta.uid() == who.uid {
        6
    } else if who.groups.contains(&meta.gid()) {
        3
    } else {
        0
    };
    (meta.mode() >> shift) & bits == bits
}

/// Fail at install time when the service user cannot reach `path`: search
/// (x) on every ancestor, `leaf_bits` on the leaf. A root install resolving
/// root-only locations (0700 /root, nix profiles) otherwise becomes a
/// runtime crash-loop under `User=`.
pub(crate) fn check_access(path: &Path, who: &ServiceUser, leaf_bits: u32) -> Result<()> {
    let mut cur = Some(path);
    while let Some(p) = cur {
        let meta =
            fs::metadata(p).with_context(|| format!("stat {} (access check)", p.display()))?;
        let bits = if p == path { leaf_bits } else { 0o1 };
        ensure!(
            mode_allows(&meta, who, bits),
            "{} is not accessible to the service user (uid {}): the daemon runs \
             as that user — move it somewhere reachable or adjust permissions",
            p.display(),
            who.uid
        );
        cur = p.parent().filter(|q| !q.as_os_str().is_empty());
    }
    Ok(())
}

/// Create the write paths 0700. A root (system-mode) install chowns every
/// component it created to the service user — the daemon runs as `User=` and
/// root-owned dirs would fail it on first start.
pub(crate) fn create_write_paths(paths: &[PathBuf], owner: Option<&ServiceUser>) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    for p in paths {
        let created = missing_components(p);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(p)
            .with_context(|| format!("creating {}", p.display()))?;
        if let Some(who) = owner {
            for dir in &created {
                std::os::unix::fs::chown(dir, Some(who.uid), Some(who.gid))
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
    let service_user = match &user {
        Some(name) => Some(service_user_of(name)?),
        None => None,
    };

    // resolve everything fallible before touching the filesystem, so a
    // failed install leaves nothing behind
    let exec = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("resolving the ssync binary path")?;
    let config_abs = config_path
        .canonicalize()
        .with_context(|| format!("resolving {}", config_path.display()))?;
    let (path, age_bins) = daemon_path(&std::env::var("PATH").context("reading PATH")?)?;

    // a system unit runs as `User=`, not the installing root: everything the
    // unit references must be reachable by that user
    if let Some(who) = &service_user {
        check_access(&exec, who, 0o5)?;
        check_access(&config_abs, who, 0o4)?;
        for bin in &age_bins {
            check_access(bin, who, 0o5)?;
        }
    }

    let paths = write_paths_of(&config);
    create_write_paths(&paths, service_user.as_ref())?;
    if let Some(who) = &service_user {
        // pre-existing dirs were never chowned; catch a root-owned leaf now
        for p in &paths {
            check_access(p, who, 0o7)?;
        }
    }

    let spec = ServiceSpec {
        exec,
        config_path: config_abs,
        read_write_paths: paths,
        path,
        user,
    };
    validate_spec(&spec)?;

    let unit_path = unit_dir(user_mode)?.join(UNIT_NAME);
    if let Some(parent) = unit_path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&unit_path, render_unit(&spec))
        .with_context(|| format!("writing {}", unit_path.display()))?;

    systemctl(user_mode, &["daemon-reload"])?;
    systemctl(user_mode, &["enable", UNIT_NAME])?;
    // restart, not start: a reinstall must replace an already-running daemon
    systemctl(user_mode, &["restart", UNIT_NAME])?;
    if user_mode {
        // without linger the user manager (and the daemon with it) dies at
        // logout and is absent after boot until the next login. Best effort:
        // needs a logind session, which sandboxes and CI lack.
        let lingered = Command::new("loginctl")
            .arg("enable-linger")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !lingered {
            eprintln!(
                "ssync: could not enable linger; the daemon stops at logout — \
                 run `loginctl enable-linger` manually"
            );
        }
    }

    let scope = if user_mode { "--user " } else { "" };
    println!("installed and started {}", unit_path.display());
    println!("check: systemctl {scope}status ssync");
    Ok(())
}

pub fn cmd_service_uninstall() -> Result<()> {
    let user_mode = !is_root()?;
    let unit_path = unit_dir(user_mode)?.join(UNIT_NAME);
    if !unit_path.exists() {
        bail!(
            "nothing to uninstall: {} does not exist",
            unit_path.display()
        );
    }
    // best effort, separately: a masked or never-enabled unit must not block
    // removal, and a failed `disable` must not leave the daemon running
    for args in [&["stop", UNIT_NAME], &["disable", UNIT_NAME]] {
        if let Err(e) = systemctl(user_mode, args) {
            eprintln!("ssync: {e}");
        }
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
    fn user_unit_dir_is_under_the_user_manager_dir() {
        assert_eq!(
            user_unit_dir(Path::new("/home/alice/.config")),
            PathBuf::from("/home/alice/.config/systemd/user")
        );
    }

    #[test]
    fn tilde_detection_matches_both_toml_string_kinds() {
        assert!(config_uses_tilde("data_dir = \"~/.local/share/ssync\"\n"));
        assert!(config_uses_tilde("data_dir = '~/.local/share/ssync'\n"));
        assert!(!config_uses_tilde("data_dir = \"/var/lib/ssync\"\n"));
        // a bare `~` also expands in Config::parse (strip_prefix("~"))
        assert!(config_uses_tilde("data_dir = \"~\"\n"));
        assert!(config_uses_tilde("data_dir = '~'\n"));
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

    fn user(uid: u32, gid: u32, groups: &[u32]) -> ServiceUser {
        ServiceUser {
            uid,
            gid,
            groups: groups.to_vec(),
        }
    }

    #[test]
    fn create_write_paths_chowns_created_components_to_the_owner() {
        use std::os::unix::fs::MetadataExt;
        let base = scratch("chown");
        let me = std::fs::metadata(&base).unwrap();
        let leaf = base.join("owned/sessions");
        // chown to our own uid/gid: a no-op the kernel permits unprivileged,
        // but it drives the owner branch end to end
        let who = user(me.uid(), me.gid(), &[me.gid()]);
        create_write_paths(std::slice::from_ref(&leaf), Some(&who)).unwrap();
        let got = std::fs::metadata(&leaf).unwrap();
        assert_eq!((got.uid(), got.gid()), (me.uid(), me.gid()));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn check_access_denies_a_foreign_user_on_private_dirs() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let base = scratch("access");
        let leaf = base.join("private");
        std::fs::create_dir(&leaf).unwrap();
        std::fs::set_permissions(&leaf, std::fs::Permissions::from_mode(0o700)).unwrap();
        let me = std::fs::metadata(&base).unwrap();
        let owner = user(me.uid(), me.gid(), &[me.gid()]);
        let stranger = user(me.uid() + 1, me.gid() + 1, &[me.gid() + 1]);
        check_access(&leaf, &owner, 0o7).unwrap();
        assert!(check_access(&leaf, &stranger, 0o7).is_err());
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Group semantics on `mode_allows` directly: a full `check_access` walk
    /// would depend on ancestor permissions (0700 /build in the nix sandbox).
    #[test]
    fn mode_allows_honors_supplementary_groups() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let base = scratch("modebits");
        let leaf = base.join("shared");
        std::fs::create_dir(&leaf).unwrap();
        std::fs::set_permissions(&leaf, std::fs::Permissions::from_mode(0o070)).unwrap();
        let me = std::fs::metadata(&base).unwrap();
        let meta = std::fs::metadata(&leaf).unwrap();
        // supplementary membership in the dir's group grants the group bits
        let groupie = user(me.uid() + 1, me.gid() + 1, &[me.gid() + 1, me.gid()]);
        assert!(mode_allows(&meta, &groupie, 0o7));
        // without membership only the (empty) other bits apply
        let stranger = user(me.uid() + 1, me.gid() + 1, &[me.gid() + 1]);
        assert!(!mode_allows(&meta, &stranger, 0o7));
        std::fs::set_permissions(&leaf, std::fs::Permissions::from_mode(0o700)).unwrap();
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
