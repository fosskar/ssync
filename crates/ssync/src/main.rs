// ssync — p2p sync of coding-agent session files. See docs/DECISIONS.md.

mod cleanup_timer;
mod cluster;
mod service;

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, ensure};
use clap::{Parser, Subcommand};
use ssync_adapters::adapter_for;
use ssync_core::{Config, Engine, StatusReport};
use ssync_crypto::AgeIdentity;
use ssync_net::iroh_docs::{DocTicket, NamespaceId};
use ssync_net::{Node, load_or_create_secret_key};

#[derive(Parser)]
#[command(
    name = "ssync",
    version,
    about = "p2p sync of coding-agent session files"
)]
struct Cli {
    /// Config file path (default: $XDG_CONFIG_HOME/ssync/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a default config and generate the shared age identity if missing.
    Init,
    /// Run the sync daemon (import existing, watch+import, export on updates).
    Daemon,
    /// Print this node's pairing ticket (the daemon writes it on start).
    Ticket,
    /// Stage a peer's pairing ticket; the daemon joins it on next start.
    Join {
        /// The ticket string from `ssync ticket` on another machine.
        ticket: String,
    },
    /// Manage cluster membership: one secret artifact (shared namespace
    /// secret + recipients + node-ids) distributed to every machine.
    Cluster {
        #[command(subcommand)]
        action: ClusterAction,
    },
    /// Show sync status (namespace, session count, conflicts).
    Status,
    /// List sessions that have diverged across machines.
    Conflicts,
    /// Find sessions by title or project path (case-insensitive substring).
    Search {
        query: String,
        /// Only this agent's sessions (default: all configured agents).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Delete old/unnamed local sessions; the daemon propagates the deletions.
    Cleanup {
        /// Only this agent's sessions (default: all configured agents).
        #[arg(long)]
        agent: Option<String>,
        /// Delete sessions created more than this long ago (e.g. 30d, 6w, 3m, 1y).
        #[arg(long, conflicts_with = "before")]
        keep: Option<String>,
        /// Delete sessions created before this date (YYYY-MM-DD, UTC).
        #[arg(long)]
        before: Option<String>,
        /// Delete sessions whose title record is present but empty.
        #[arg(long)]
        unnamed: bool,
        /// Actually delete. Without it, only list what would be deleted.
        #[arg(long)]
        apply: bool,
    },
    /// Manage a systemd timer running `ssync cleanup --apply` on a schedule.
    /// Deletions propagate to every peer: one machine's timer prunes all.
    CleanupTimer {
        #[command(subcommand)]
        action: CleanupTimerAction,
    },
    /// Install or remove a hardened systemd unit running the daemon.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Generate an iroh node key at PATH; print its node-id (for clan.vars).
    KeygenNode { path: PathBuf },
    /// Generate a shared namespace secret at PATH (for clan.vars; the cluster
    /// artifact embeds it via `ssync cluster render --secret-file`).
    KeygenNamespace { path: PathBuf },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Write the unit and `enable --now` it (user unit; system unit as root).
    Install {
        /// Run the system unit as this user (root-only; user units need none).
        #[arg(long)]
        user: Option<String>,
    },
    /// Disable the unit, delete it, and reload systemd.
    Uninstall,
}

#[derive(Subcommand)]
enum ClusterAction {
    /// Create the cluster artifact with this machine as the first member.
    Init {
        /// Artifact path (default: cluster.toml next to the config).
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Adopt a received cluster file on this machine.
    Join { file: PathBuf },
    /// Add a machine by its age recipient (printed by `ssync init` there).
    Add {
        recipient: String,
        /// The machine's iroh node-id (also printed by `ssync init`).
        #[arg(long)]
        node_id: Option<String>,
    },
    /// Remove a machine and rotate the namespace secret.
    Rm { recipient: String },
    /// List members and the namespace the current secret derives.
    Show,
    /// Assemble a cluster file non-interactively (clan.vars plumbing): the
    /// given (or a fresh) namespace secret plus the given members.
    Render {
        /// Output path (written 0600).
        #[arg(long)]
        out: PathBuf,
        /// 32-byte namespace secret file (`ssync keygen-namespace`); a fresh
        /// secret is generated when absent.
        #[arg(long)]
        secret_file: Option<PathBuf>,
        /// Members as `recipient[:node-id]`, at least one.
        #[arg(required = true)]
        members: Vec<String>,
    },
}

#[derive(Subcommand)]
enum CleanupTimerAction {
    /// Install and start the timer/service pair (user units; system as root).
    Enable {
        /// Schedule: `2d`, `7d`, `weekly`, or a raw systemd calendar expression.
        #[arg(long)]
        every: String,
        /// Delete sessions older than this (e.g. 30d, 6w, 3m, 1y). Defaults
        /// to 90d unless --unnamed is given.
        #[arg(long)]
        keep: Option<String>,
        /// Also delete sessions whose title record is present but empty.
        #[arg(long)]
        unnamed: bool,
        /// Only this agent's sessions (default: all configured agents).
        #[arg(long)]
        agent: Option<String>,
        /// Run cleanup as this user (root-only; user units need none).
        #[arg(long)]
        user: Option<String>,
    },
    /// Stop the timer, delete both units, and reload systemd.
    Disable,
    /// Show the timer's systemd status.
    Status,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_explicit = cli.config.is_some();
    let config_path = match cli.config {
        Some(p) => p,
        None => Config::default_path()?,
    };
    match cli.command {
        Command::Init => cmd_init(&config_path).await,
        Command::Daemon => cmd_daemon(&config_path).await,
        Command::Ticket => cmd_ticket(&config_path),
        Command::Join { ticket } => cmd_join(&config_path, &ticket),
        Command::Cluster { action } => match action {
            ClusterAction::Init { path } => cluster::cmd_init(&config_path, path).await,
            ClusterAction::Join { file } => cluster::cmd_join(&config_path, &file),
            ClusterAction::Add { recipient, node_id } => {
                cluster::cmd_add(&config_path, &recipient, node_id)
            }
            ClusterAction::Rm { recipient } => cluster::cmd_rm(&config_path, &recipient),
            ClusterAction::Show => cluster::cmd_show(&config_path),
            ClusterAction::Render {
                out,
                secret_file,
                members,
            } => cluster::cmd_render(&out, secret_file.as_deref(), &members),
        },
        Command::Status => cmd_status(&config_path),
        Command::Conflicts => cmd_conflicts(&config_path),
        Command::Search { query, agent } => cmd_search(&config_path, &query, agent.as_deref()),
        Command::Cleanup {
            agent,
            keep,
            before,
            unnamed,
            apply,
        } => cmd_cleanup(&config_path, agent, keep, before, unnamed, apply),
        Command::Service { action } => match action {
            ServiceAction::Install { user } => {
                service::cmd_service_install(&config_path, config_explicit, user)
            }
            ServiceAction::Uninstall => service::cmd_service_uninstall(),
        },
        Command::CleanupTimer { action } => match action {
            CleanupTimerAction::Enable {
                every,
                keep,
                unnamed,
                agent,
                user,
            } => cleanup_timer::cmd_enable(
                &config_path,
                config_explicit,
                every,
                cleanup_timer::CleanupSelectors {
                    keep,
                    unnamed,
                    agent,
                },
                user,
            ),
            CleanupTimerAction::Disable => cleanup_timer::cmd_disable(),
            CleanupTimerAction::Status => cleanup_timer::cmd_status(),
        },
        Command::KeygenNode { path } => {
            let bytes = ssync_net::generate_key_bytes();
            write_secret_bytes(&path, &bytes)?;
            println!("{}", ssync_net::node_id_of(&bytes));
            Ok(())
        }
        Command::KeygenNamespace { path } => {
            write_secret_bytes(&path, &ssync_net::generate_key_bytes())
        }
    }
}

/// Title/project search over the local session files — read-only, never
/// touches the daemon or the index (issue #18).
fn cmd_search(config_path: &Path, query: &str, agent: Option<&str>) -> Result<()> {
    let config = Config::load(config_path)?;
    let adapters = config
        .agents
        .iter()
        .map(|a| adapter_for(&a.agent, &a.session_dir))
        .collect::<Result<Vec<_>>>()?;
    let hits = ssync_core::search::search(&adapters, query, agent)?;
    if hits.is_empty() {
        println!("no sessions match {query:?}");
        return Ok(());
    }
    let home = dirs::home_dir();
    for h in &hits {
        let date = h
            .created_at
            .map(ssync_core::search::date_of)
            .unwrap_or_else(|| "????-??-??".into());
        let title = match h.title.as_deref() {
            Some("") | None => "(untitled)".to_string(),
            Some(t) => format!("{t:?}"),
        };
        // ~-shorten the project path for display
        let project = match &home {
            Some(home) => std::path::Path::new(&h.project)
                .strip_prefix(home)
                .map(|r| format!("~/{}", r.display()))
                .unwrap_or_else(|_| h.project.clone()),
            None => h.project.clone(),
        };
        println!("{:5} {date}  {:40} {title}", h.agent, project);
        println!("      {}", h.session_id);
    }
    println!("{} session(s)", hits.len());
    Ok(())
}

fn cmd_cleanup(
    config_path: &Path,
    agent: Option<String>,
    keep: Option<String>,
    before: Option<String>,
    unnamed: bool,
    apply: bool,
) -> Result<()> {
    use ssync_core::cleanup::{Filter, parse_keep, plan};

    let config = Config::load(config_path)?;
    let cutoff = match (keep, before) {
        (Some(k), None) => Some(
            std::time::SystemTime::now()
                .checked_sub(parse_keep(&k)?)
                .ok_or_else(|| anyhow!("duration out of range"))?,
        ),
        (None, Some(d)) => Some(
            ssync_adapters::pi::parse_pi_timestamp(&format!("{d}T00-00-00-000Z"))
                .ok_or_else(|| anyhow!("invalid date {d:?} (expected YYYY-MM-DD)"))?,
        ),
        (None, None) => None,
        (Some(_), Some(_)) => unreachable!("clap conflicts_with"),
    };
    let adapters = config
        .agents
        .iter()
        .map(|a| adapter_for(&a.agent, &a.session_dir))
        .collect::<Result<Vec<_>>>()?;
    let victims = plan(
        &adapters,
        &Filter {
            agent,
            before: cutoff,
            unnamed,
        },
    )?;
    if victims.is_empty() {
        println!("nothing to delete");
        return Ok(());
    }
    let total: u64 = victims.iter().map(|v| v.size).sum();
    for v in &victims {
        println!("{}  {}", v.agent, v.path.display());
    }
    println!(
        "{} session file(s), {:.1} MB{}",
        victims.len(),
        total as f64 / 1e6,
        if apply {
            ""
        } else {
            " — dry run, pass --apply to delete"
        }
    );
    if apply {
        let root_of: std::collections::HashMap<&str, &Path> = adapters
            .iter()
            .map(|a| (a.agent(), a.session_root()))
            .collect();
        for v in &victims {
            std::fs::remove_file(&v.path)
                .with_context(|| format!("deleting {}", v.path.display()))?;
            // sweep the artifact dir a fully-deleted session leaves empty
            if let Some(root) = root_of.get(v.agent.as_str()) {
                ssync_core::cleanup::remove_empty_parents(&v.path, root);
            }
        }
        println!("deleted; the daemon propagates the deletions to peers");
    }
    Ok(())
}

async fn cmd_init(config_path: &Path) -> Result<()> {
    let (config, _pre_existed) = load_or_bootstrap_config(config_path)?;
    let already_had_identity = config.age_identity_path.exists();
    let id = load_or_generate_identity(&config)?;
    println!("age recipient: {}", id.recipient_string());
    if !already_had_identity {
        println!(
            "run `ssync cluster init` here, then `ssync cluster add <recipient>` for each machine (recommended),"
        );
        println!(
            "or for ticket mode: copy this key everywhere, or list peers' recipients in `recipients`."
        );
    }
    // the node-id pairs with the recipient in `ssync cluster add`
    let node_key_path = config.node_key_file();
    let secret = load_or_create_secret_key(&node_key_path).await?;
    println!("node-id: {}", secret.public());
    Ok(())
}

async fn cmd_daemon(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    if config.agents.is_empty() {
        return Err(anyhow!("config has no [[agents]] entries"));
    }
    std::fs::create_dir_all(&config.data_dir)?;
    for a in &config.agents {
        std::fs::create_dir_all(&a.session_dir)?;
    }

    // Auto-generate the age identity on first run. In per-machine mode
    // (config lists the other machines' `recipients`) each machine keeps its
    // own key; otherwise the key is shared and a second standalone machine
    // must be given this same key (clan.vars handles either mode).
    if !config.age_identity_path.exists() {
        let id = AgeIdentity::generate()?;
        write_secret(&config.age_identity_path, &id.to_secret_string())?;
        if config.cluster_path.is_none() {
            if config.recipients.is_empty() {
                eprintln!(
                    "ssync: generated age identity {}; other machines must use this same key",
                    config.age_identity_path.display()
                );
            } else {
                eprintln!(
                    "ssync: generated age identity {}; add this recipient via `ssync cluster add` (recommended) or to the peers' `recipients`",
                    config.age_identity_path.display()
                );
            }
        } else {
            eprintln!(
                "ssync: generated age identity {}",
                config.age_identity_path.display()
            );
        }
    }
    let cluster = match &config.cluster_path {
        Some(p) => Some(
            ssync_core::cluster::ClusterFile::parse(&read_secret_text(p)?)
                .with_context(|| format!("cluster file {}", p.display()))?,
        ),
        None => None,
    };
    let mut identity = load_identity(&config.age_identity_path)?;
    match &cluster {
        Some(c) => identity.add_recipients(c.recipients()),
        None => {
            identity.add_recipients(config.recipients.iter().cloned());
            if config.recipients.is_empty() {
                eprintln!(
                    "ssync: shared-identity mode (`recipients` empty); per-machine keys enable revocation, see docs/setup.md"
                );
            }
        }
    }

    let node_key_path = config.node_key_file();
    let secret = load_or_create_secret_key(&node_key_path).await?;
    let mut node = Node::spawn(&config.data_dir, secret).await?;
    node.enable_mdns();

    if let Some(c) = &cluster {
        // cluster-artifact mode (issue #23): the file is the whole peer set.
        let own = node.endpoint_id().to_string();
        let id = node.open_shared_namespace(c.namespace_secret()).await?;
        let peers = c.peer_node_ids(&own);
        node.sync_with_peers(&peers).await?;
        println!(
            "ssync: cluster namespace {id}, syncing with {} peer(s)",
            peers.len()
        );
    } else {
        // ticket-based pairing: reopen persisted, else join a staged ticket,
        // else create a fresh namespace.
        let ns_file = config.data_dir.join("namespace");
        let remote_ticket = config.data_dir.join("remote-ticket");
        if let Ok(text) = std::fs::read_to_string(&ns_file) {
            let id: NamespaceId = text.trim().parse().context("parsing saved namespace")?;
            node.open_namespace(id).await?;
        } else if let Ok(text) = std::fs::read_to_string(&remote_ticket) {
            let ticket: DocTicket = text.trim().parse().context("parsing staged ticket")?;
            let id = node.join(ticket).await?;
            std::fs::write(&ns_file, id.to_string())?;
            std::fs::remove_file(&remote_ticket).ok();
            println!("joined namespace {id}");
        } else {
            let id = node.create_namespace().await?;
            std::fs::write(&ns_file, id.to_string())?;
            println!("created namespace {id}");
        }
        let ticket = node.share().await?;
        std::fs::write(config.data_dir.join("ticket"), ticket.to_string())?;
    }

    // namespace rotation = eviction (issue #22): abandon replicas the current
    // config no longer names, so a revoked peer cannot keep syncing them.
    for ns in node.drop_stale_replicas().await? {
        println!("ssync: dropped stale namespace {ns}");
    }

    let adapters = config
        .agents
        .iter()
        .map(|a| adapter_for(&a.agent, &a.session_dir))
        .collect::<Result<Vec<_>>>()?;
    let mut engine = Engine::with_adapters(adapters, identity, node);
    engine.set_excludes(
        config
            .agents
            .iter()
            .filter(|a| !a.exclude.is_empty())
            .map(|a| (a.agent.clone(), a.exclude.clone()))
            .collect(),
    );
    if !config.path_map.is_empty() {
        engine.set_path_map(config.build_path_map()?, config.canonical_home.clone());
        println!(
            "ssync: path map active ({} prefix pair(s))",
            config.path_map.len()
        );
    }
    engine.persist_state(&config.data_dir.join("state.toml"));
    for a in &config.agents {
        println!(
            "ssync daemon watching {} ({})",
            a.session_dir.display(),
            a.agent
        );
    }
    engine.run(&config.data_dir.join("status.toml")).await
}

/// Status snapshot plus its age (time since the daemon last wrote it).
fn read_status(config: &Config) -> Result<(StatusReport, Option<std::time::Duration>)> {
    let path = config.data_dir.join("status.toml");
    let text = std::fs::read_to_string(&path).context("no status yet — start the daemon first")?;
    let age = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok());
    Ok((toml::from_str(&text)?, age))
}

/// Older than this, the snapshot is suspect: the daemon rewrites it on every
/// change and its rescan ticks every 15s, so a live idle daemon stays fresher.
fn warn_if_stale(age: Option<std::time::Duration>) {
    match age {
        Some(age) if age.as_secs() > 300 => eprintln!(
            "warning: status is {}s old — is the daemon running?",
            age.as_secs()
        ),
        _ => {}
    }
}

fn cmd_status(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let (s, age) = read_status(&config)?;
    println!("namespace: {}", s.namespace.as_deref().unwrap_or("(none)"));
    println!("sessions:  {}", s.sessions);
    println!("conflicts: {}", s.conflicts.len());
    for p in &s.peers {
        println!("peer:      {} ({})", p.id, p.path);
    }
    if let Some(age) = age {
        println!("updated:   {}s ago", age.as_secs());
    }
    warn_if_stale(age);
    Ok(())
}

fn cmd_conflicts(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let (s, age) = read_status(&config)?;
    warn_if_stale(age);
    if s.conflicts.is_empty() {
        println!("no conflicts");
    } else {
        for c in s.conflicts {
            println!("{c}");
        }
    }
    Ok(())
}

fn cmd_ticket(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let ticket = std::fs::read_to_string(config.data_dir.join("ticket"))
        .context("no ticket yet — start the daemon first")?;
    print!("{ticket}");
    Ok(())
}

fn cmd_join(config_path: &Path, ticket: &str) -> Result<()> {
    let config = Config::load(config_path)?;
    ticket
        .trim()
        .parse::<DocTicket>()
        .context("invalid ticket")?;
    std::fs::create_dir_all(&config.data_dir)?;
    std::fs::write(config.data_dir.join("remote-ticket"), ticket.trim())?;
    println!("ticket staged; (re)start the daemon to join.");
    Ok(())
}

/// Read a secret file, refusing group/world-readable permissions.
fn read_secret_text(path: &Path) -> Result<String> {
    use std::os::unix::fs::PermissionsExt;
    let meta =
        std::fs::metadata(path).with_context(|| format!("reading secret {}", path.display()))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(anyhow!(
            "secret {} is group/world-accessible; run `chmod 600` on it",
            path.display()
        ));
    }
    std::fs::read_to_string(path).with_context(|| format!("reading secret {}", path.display()))
}

fn load_identity(path: &Path) -> Result<AgeIdentity> {
    let text =
        read_secret_text(path).with_context(|| format!("age identity {}", path.display()))?;
    AgeIdentity::from_secret_string(text.trim())
}

/// Load the config if present, else write built-in defaults; returns whether
/// the file pre-existed (`ssync cluster init`/`join` need that — F3).
fn load_or_bootstrap_config(config_path: &Path) -> Result<(Config, bool)> {
    if config_path.exists() {
        Ok((Config::load(config_path)?, true))
    } else {
        let c = Config::defaults()?;
        c.save(config_path)?;
        println!("wrote config {}", config_path.display());
        Ok((c, false))
    }
}

/// Load the shared age identity, generating and persisting one on first run.
fn load_or_generate_identity(config: &Config) -> Result<AgeIdentity> {
    if config.age_identity_path.exists() {
        load_identity(&config.age_identity_path)
    } else {
        let id = AgeIdentity::generate()?;
        write_secret(&config.age_identity_path, &id.to_secret_string())?;
        println!(
            "generated age identity {}",
            config.age_identity_path.display()
        );
        Ok(id)
    }
}

/// The cluster file replaces `recipients` — reject an ambiguous config.
fn ensure_no_recipients(config: &Config) -> Result<()> {
    ensure!(
        config.recipients.is_empty(),
        "config sets recipients — the cluster file replaces them, remove them first"
    );
    Ok(())
}

/// Write a secret string with `0600` permissions.
fn write_secret(path: &Path, contents: &str) -> Result<()> {
    write_secret_bytes(path, contents.as_bytes())
}

/// Write secret bytes with `0600` permissions.
fn write_secret_bytes(path: &Path, contents: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    file.write_all(contents)
        .with_context(|| format!("writing {}", tmp.display()))?;
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_secret_bytes_persists_0600_atomically() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ssync-secret-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret");
        write_secret_bytes(&path, b"top secret").unwrap();
        assert!(!path.with_extension("tmp").exists(), "tmp sibling leaked");
        let meta = std::fs::metadata(&path).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(&path).unwrap(), b"top secret");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
