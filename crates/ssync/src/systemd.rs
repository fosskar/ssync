//! systemd unit lifecycle shared by `service` and `cleanup-timer` commands.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, ensure};
use ssync_core::Config;

/// Hardening shared with the NixOS and Home Manager modules (DECISIONS §12).
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

/// Values resolved by the lifecycle before command-specific unit rendering.
pub(crate) struct InstallContext<'a> {
    pub exec: &'a Path,
    pub config_path: &'a Path,
    pub user: Option<&'a str>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct UnitFile {
    pub name: &'static str,
    pub contents: String,
}

pub(crate) struct UnitSet {
    pub files: Vec<UnitFile>,
    pub write_paths: Vec<PathBuf>,
    pub required_executables: Vec<PathBuf>,
    pub activation: Activation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Activation {
    Restart(&'static str),
    EnableNow(&'static str),
}

impl Activation {
    fn control_steps(self) -> [Option<SystemctlStep>; 3] {
        match self {
            Self::Restart(name) => [
                Some(SystemctlStep::DaemonReload),
                Some(SystemctlStep::Enable(name)),
                Some(SystemctlStep::Restart(name)),
            ],
            Self::EnableNow(name) => [
                Some(SystemctlStep::DaemonReload),
                Some(SystemctlStep::EnableNow(name)),
                None,
            ],
        }
    }
}

pub(crate) struct InstallResult {
    pub unit_dir: PathBuf,
    pub user_mode: bool,
}

pub(crate) struct RemoveSpec<'a> {
    pub primary: &'static str,
    pub files: &'a [&'static str],
    pub missing_action: &'static str,
}

/// Refuse a value a unit file cannot carry verbatim: systemd expands `%`
/// specifiers, splits path lists on whitespace, and interprets quotes/backslashes.
pub(crate) fn ensure_unit_safe(value: &str, what: &str) -> Result<()> {
    ensure!(
        !value.contains(['%', '"', '\\']) && !value.chars().any(|c| c.is_whitespace()),
        "{what} `{value}` contains characters a systemd unit cannot carry \
         (whitespace, %, \", \\)"
    );
    Ok(())
}

/// Prepare, render, install, and activate one command's complete unit set.
pub(crate) fn install(
    config_path: &Path,
    config_explicit: bool,
    user: Option<String>,
    render: impl FnOnce(&Config, InstallContext<'_>) -> Result<UnitSet>,
) -> Result<InstallResult> {
    let user_mode = !is_root()?;
    ensure_install_mode(user_mode, user.as_ref(), config_explicit, config_path)?;

    let config = Config::load(config_path)
        .with_context(|| format!("loading {} (run `ssync init` first)", config_path.display()))?;
    let service_user = match &user {
        Some(name) => Some(service_user_of(name)?),
        None => None,
    };

    let exec = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .context("resolving the ssync binary path")?;
    let config_abs = config_path
        .canonicalize()
        .with_context(|| format!("resolving {}", config_path.display()))?;
    let units = render(
        &config,
        InstallContext {
            exec: &exec,
            config_path: &config_abs,
            user: user.as_deref(),
        },
    )?;

    if let Some(who) = &service_user {
        check_access(&exec, who, 0o5)?;
        check_access(&config_abs, who, 0o4)?;
        for path in &units.required_executables {
            check_access(path, who, 0o5)?;
        }
    }

    create_write_paths(&units.write_paths, service_user.as_ref())?;
    if let Some(who) = &service_user {
        for path in &units.write_paths {
            check_access(path, who, 0o7)?;
        }
    }

    let unit_dir = unit_dir(user_mode)?;
    InstallPlan {
        unit_dir: &unit_dir,
        user_mode,
        files: &units.files,
        activation: units.activation,
    }
    .execute()?;
    Ok(InstallResult {
        unit_dir,
        user_mode,
    })
}

pub(crate) fn remove(spec: RemoveSpec<'_>) -> Result<PathBuf> {
    let user_mode = !is_root()?;
    let unit_dir = unit_dir(user_mode)?;
    let primary_path = unit_dir.join(spec.primary);
    ensure!(
        primary_path.exists(),
        "nothing to {}: {} does not exist",
        spec.missing_action,
        primary_path.display()
    );
    RemovalPlan {
        unit_dir: &unit_dir,
        user_mode,
        primary: spec.primary,
        files: spec.files,
    }
    .execute()?;
    Ok(primary_path)
}

/// Show a unit's systemd status. A non-zero `systemctl status` is still output.
pub(crate) fn show_status(name: &str) -> Result<bool> {
    let user_mode = !is_root()?;
    if !unit_dir(user_mode)?.join(name).exists() {
        return Ok(false);
    }
    systemctl_command(user_mode)
        .args(["--no-pager", "status", name])
        .status()
        .context("running systemctl (is systemd available?)")?;
    Ok(true)
}

fn systemctl_command(user_mode: bool) -> Command {
    let mut command = Command::new("systemctl");
    if user_mode {
        command.arg("--user");
    }
    command
}

fn run_systemctl(user_mode: bool, args: &[&str]) -> Result<()> {
    let status = systemctl_command(user_mode)
        .args(args)
        .status()
        .context("running systemctl (is systemd available?)")?;
    ensure!(status.success(), "systemctl {} failed", args.join(" "));
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SystemctlStep {
    DaemonReload,
    Enable(&'static str),
    Restart(&'static str),
    EnableNow(&'static str),
    Stop(&'static str),
    Disable(&'static str),
}

impl SystemctlStep {
    fn execute(self, user_mode: bool) -> Result<()> {
        match self {
            Self::DaemonReload => run_systemctl(user_mode, &["daemon-reload"]),
            Self::Enable(name) => run_systemctl(user_mode, &["enable", name]),
            Self::Restart(name) => run_systemctl(user_mode, &["restart", name]),
            Self::EnableNow(name) => run_systemctl(user_mode, &["enable", "--now", name]),
            Self::Stop(name) => run_systemctl(user_mode, &["stop", name]),
            Self::Disable(name) => run_systemctl(user_mode, &["disable", name]),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum InstallStep<'a> {
    Write(&'a UnitFile),
    Control(SystemctlStep),
}

struct InstallPlan<'a> {
    unit_dir: &'a Path,
    user_mode: bool,
    files: &'a [UnitFile],
    activation: Activation,
}

impl InstallPlan<'_> {
    fn steps(&self) -> impl Iterator<Item = InstallStep<'_>> {
        self.files.iter().map(InstallStep::Write).chain(
            self.activation
                .control_steps()
                .into_iter()
                .flatten()
                .map(InstallStep::Control),
        )
    }

    fn execute(&self) -> Result<()> {
        fs::create_dir_all(self.unit_dir)
            .with_context(|| format!("creating {}", self.unit_dir.display()))?;
        for step in self.steps() {
            match step {
                InstallStep::Write(unit) => {
                    let path = self.unit_dir.join(unit.name);
                    fs::write(&path, &unit.contents)
                        .with_context(|| format!("writing {}", path.display()))?;
                }
                InstallStep::Control(command) => command.execute(self.user_mode)?,
            }
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
enum RemovalStep {
    Control {
        step: SystemctlStep,
        best_effort: bool,
    },
    Remove(&'static str),
}

struct RemovalPlan<'a> {
    unit_dir: &'a Path,
    user_mode: bool,
    primary: &'static str,
    files: &'a [&'static str],
}

impl RemovalPlan<'_> {
    fn steps(&self) -> impl Iterator<Item = RemovalStep> + '_ {
        std::iter::once(RemovalStep::Control {
            step: SystemctlStep::Stop(self.primary),
            best_effort: true,
        })
        .chain(std::iter::once(RemovalStep::Control {
            step: SystemctlStep::Disable(self.primary),
            best_effort: true,
        }))
        .chain(self.files.iter().copied().map(RemovalStep::Remove))
        .chain(std::iter::once(RemovalStep::Control {
            step: SystemctlStep::DaemonReload,
            best_effort: false,
        }))
    }

    fn execute(&self) -> Result<()> {
        for step in self.steps() {
            match step {
                RemovalStep::Control { step, best_effort } => {
                    settle_control_result(step.execute(self.user_mode), best_effort)?;
                }
                RemovalStep::Remove(name) => {
                    let path = self.unit_dir.join(name);
                    if path.exists() {
                        fs::remove_file(&path)
                            .with_context(|| format!("removing {}", path.display()))?;
                    }
                }
            }
        }
        Ok(())
    }
}

fn settle_control_result(result: Result<()>, best_effort: bool) -> Result<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) if best_effort => {
            eprintln!("ssync: {error}");
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn ensure_install_mode(
    user_mode: bool,
    user: Option<&String>,
    config_explicit: bool,
    config_path: &Path,
) -> Result<()> {
    if user_mode {
        ensure!(
            user.is_none(),
            "--user only applies to system units; run as root to install them"
        );
    } else {
        ensure!(
            user.is_some(),
            "system units need an explicit --user <name> to run as \
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
             home at install time but to --user's home in the unit"
        );
    }
    Ok(())
}

fn config_uses_tilde(raw: &str) -> bool {
    ["\"~/", "'~/", "\"~\"", "'~'"]
        .iter()
        .any(|needle| raw.contains(needle))
}

fn is_root() -> Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata("/proc/self").context("reading /proc/self (uid check)")?;
    Ok(metadata.uid() == 0)
}

fn unit_dir(user_mode: bool) -> Result<PathBuf> {
    Ok(if user_mode {
        user_unit_dir(&dirs::config_dir().context("no config dir")?)
    } else {
        PathBuf::from("/etc/systemd/system")
    })
}

fn user_unit_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("systemd/user")
}

struct ServiceUser {
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
}

fn service_user_of(user: &str) -> Result<ServiceUser> {
    let lookup = |flag: &str| -> Result<String> {
        let output = Command::new("id")
            .args([flag, user])
            .output()
            .context("running id")?;
        ensure!(output.status.success(), "unknown user {user}");
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let parse = |text: &str| {
        text.parse::<u32>()
            .with_context(|| format!("parsing id output `{text}`"))
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

fn mode_allows(metadata: &fs::Metadata, user: &ServiceUser, bits: u32) -> bool {
    use std::os::unix::fs::MetadataExt;

    if user.uid == 0 {
        return true;
    }
    let shift = if metadata.uid() == user.uid {
        6
    } else if user.groups.contains(&metadata.gid()) {
        3
    } else {
        0
    };
    (metadata.mode() >> shift) & bits == bits
}

fn check_access(path: &Path, user: &ServiceUser, leaf_bits: u32) -> Result<()> {
    let mut current = Some(path);
    while let Some(part) = current {
        let metadata = fs::metadata(part)
            .with_context(|| format!("stat {} (access check)", part.display()))?;
        let bits = if part == path { leaf_bits } else { 0o1 };
        ensure!(
            mode_allows(&metadata, user, bits),
            "{} is not accessible to the service user (uid {}): the daemon runs \
             as that user — move it somewhere reachable or adjust permissions",
            part.display(),
            user.uid
        );
        current = part
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty());
    }
    Ok(())
}

fn missing_components(path: &Path) -> Vec<PathBuf> {
    let mut missing = Vec::new();
    let mut current = path;
    while !current.exists() {
        missing.push(current.to_path_buf());
        match current.parent() {
            Some(parent) => current = parent,
            None => break,
        }
    }
    missing
}

fn create_write_paths(paths: &[PathBuf], owner: Option<&ServiceUser>) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    for path in paths {
        let created = missing_components(path);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .with_context(|| format!("creating {}", path.display()))?;
        if let Some(user) = owner {
            for directory in &created {
                std::os::unix::fs::chown(directory, Some(user.uid), Some(user.gid))
                    .with_context(|| format!("chowning {}", directory.display()))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ssync-systemd-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn restart_install_orders_writes_before_control() {
        let dir = scratch("restart");
        let files = [UnitFile {
            name: "ssync.service",
            contents: "daemon unit\n".into(),
        }];
        let plan = InstallPlan {
            unit_dir: &dir,
            user_mode: true,
            files: &files,
            activation: Activation::Restart("ssync.service"),
        };

        assert_eq!(
            plan.steps().collect::<Vec<_>>(),
            vec![
                InstallStep::Write(&files[0]),
                InstallStep::Control(SystemctlStep::DaemonReload),
                InstallStep::Control(SystemctlStep::Enable("ssync.service")),
                InstallStep::Control(SystemctlStep::Restart("ssync.service")),
            ]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn systemctl_command_scopes_only_user_mode() {
        let command = systemctl_command(true);
        let user_args = command
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(user_args, vec!["--user"]);
        assert!(systemctl_command(false).get_args().next().is_none());
    }

    #[test]
    fn timer_install_orders_both_writes_before_enable_now() {
        let dir = scratch("timer");
        let files = [
            UnitFile {
                name: "ssync-cleanup.service",
                contents: "cleanup service\n".into(),
            },
            UnitFile {
                name: "ssync-cleanup.timer",
                contents: "cleanup timer\n".into(),
            },
        ];
        let plan = InstallPlan {
            unit_dir: &dir,
            user_mode: false,
            files: &files,
            activation: Activation::EnableNow("ssync-cleanup.timer"),
        };

        assert_eq!(
            plan.steps().collect::<Vec<_>>(),
            vec![
                InstallStep::Write(&files[0]),
                InstallStep::Write(&files[1]),
                InstallStep::Control(SystemctlStep::DaemonReload),
                InstallStep::Control(SystemctlStep::EnableNow("ssync-cleanup.timer")),
            ]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removal_plan_keeps_stop_and_disable_best_effort() {
        let dir = scratch("remove");
        let plan = RemovalPlan {
            unit_dir: &dir,
            user_mode: false,
            primary: "ssync-cleanup.timer",
            files: &["ssync-cleanup.timer", "ssync-cleanup.service"],
        };

        assert_eq!(
            plan.steps().collect::<Vec<_>>(),
            vec![
                RemovalStep::Control {
                    step: SystemctlStep::Stop("ssync-cleanup.timer"),
                    best_effort: true,
                },
                RemovalStep::Control {
                    step: SystemctlStep::Disable("ssync-cleanup.timer"),
                    best_effort: true,
                },
                RemovalStep::Remove("ssync-cleanup.timer"),
                RemovalStep::Remove("ssync-cleanup.service"),
                RemovalStep::Control {
                    step: SystemctlStep::DaemonReload,
                    best_effort: false,
                },
            ]
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn removal_control_failures_obey_step_policy() {
        let best_effort = settle_control_result(Err(anyhow::anyhow!("stop failed")), true);
        assert!(best_effort.is_ok());

        let required =
            settle_control_result(Err(anyhow::anyhow!("reload failed")), false).unwrap_err();
        assert_eq!(required.to_string(), "reload failed");
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
        assert!(config_uses_tilde("data_dir = \"~\"\n"));
        assert!(config_uses_tilde("data_dir = '~'\n"));
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
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn write_paths_are_created_private() {
        use std::os::unix::fs::PermissionsExt;

        let base = scratch("create");
        let leaf = base.join("nested/sessions");
        create_write_paths(std::slice::from_ref(&leaf), None).unwrap();
        let mode = fs::metadata(&leaf).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "mode {mode:o}");
        fs::remove_dir_all(base).unwrap();
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
        let me = fs::metadata(&base).unwrap();
        let leaf = base.join("owned/sessions");
        let who = user(me.uid(), me.gid(), &[me.gid()]);
        create_write_paths(std::slice::from_ref(&leaf), Some(&who)).unwrap();
        let got = fs::metadata(&leaf).unwrap();
        assert_eq!((got.uid(), got.gid()), (me.uid(), me.gid()));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn check_access_denies_a_foreign_user_on_private_dirs() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let base = scratch("access");
        let leaf = base.join("private");
        fs::create_dir(&leaf).unwrap();
        fs::set_permissions(&leaf, fs::Permissions::from_mode(0o700)).unwrap();
        let me = fs::metadata(&base).unwrap();
        let owner = user(me.uid(), me.gid(), &[me.gid()]);
        let stranger = user(me.uid() + 1, me.gid() + 1, &[me.gid() + 1]);
        check_access(&leaf, &owner, 0o7).unwrap();
        assert!(check_access(&leaf, &stranger, 0o7).is_err());
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn mode_allows_honors_supplementary_groups() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let base = scratch("modebits");
        let leaf = base.join("shared");
        fs::create_dir(&leaf).unwrap();
        fs::set_permissions(&leaf, fs::Permissions::from_mode(0o070)).unwrap();
        let me = fs::metadata(&base).unwrap();
        let metadata = fs::metadata(&leaf).unwrap();
        let groupie = user(me.uid() + 1, me.gid() + 1, &[me.gid() + 1, me.gid()]);
        assert!(mode_allows(&metadata, &groupie, 0o7));
        let stranger = user(me.uid() + 1, me.gid() + 1, &[me.gid() + 1]);
        assert!(!mode_allows(&metadata, &stranger, 0o7));
        fs::set_permissions(&leaf, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_dir_all(base).unwrap();
    }
}
