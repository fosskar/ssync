// ssync — p2p sync of coding-agent session files. See docs/DECISIONS.md.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use ssync_adapters::pi::PiAdapter;
use ssync_core::{Config, Engine, StatusReport};
use ssync_crypto::AgeIdentity;
use ssync_net::iroh_docs::{DocTicket, NamespaceId};
use ssync_net::{load_or_create_secret_key, Node};

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
    /// Show sync status (namespace, session count, conflicts).
    Status,
    /// List sessions that have diverged across machines.
    Conflicts,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config_path = match cli.config {
        Some(p) => p,
        None => Config::default_path()?,
    };
    match cli.command {
        Command::Init => cmd_init(&config_path),
        Command::Daemon => cmd_daemon(&config_path).await,
        Command::Ticket => cmd_ticket(&config_path),
        Command::Join { ticket } => cmd_join(&config_path, &ticket),
        Command::Status => cmd_status(&config_path),
        Command::Conflicts => cmd_conflicts(&config_path),
    }
}

fn cmd_init(config_path: &Path) -> Result<()> {
    let config = if config_path.exists() {
        Config::load(config_path)?
    } else {
        let c = Config::defaults()?;
        c.save(config_path)?;
        println!("wrote config {}", config_path.display());
        c
    };

    if config.age_identity_path.exists() {
        let id = load_identity(&config.age_identity_path)?;
        println!("age recipient: {}", id.recipient_string());
    } else {
        let id = AgeIdentity::generate()?;
        write_secret(&config.age_identity_path, &id.to_secret_string())?;
        println!(
            "generated age identity {}",
            config.age_identity_path.display()
        );
        println!("age recipient: {}", id.recipient_string());
        println!("copy this age key to your other machines (same key everywhere).");
    }
    Ok(())
}

async fn cmd_daemon(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    std::fs::create_dir_all(&config.data_dir)?;
    std::fs::create_dir_all(&config.session_dir)?;

    // Auto-generate the age identity on first run. It is shared across machines,
    // so a second standalone machine must be given this same key (clan.vars does
    // this automatically; standalone: copy it or pair once it can be transferred).
    if !config.age_identity_path.exists() {
        let id = AgeIdentity::generate()?;
        write_secret(&config.age_identity_path, &id.to_secret_string())?;
        eprintln!(
            "ssync: generated age identity {}; other machines must use this same key",
            config.age_identity_path.display()
        );
    }
    let identity = load_identity(&config.age_identity_path)?;

    let secret = load_or_create_secret_key(&config.data_dir.join("node.key")).await?;
    let mut node = Node::spawn(&config.data_dir, secret).await?;

    // resolve the index namespace: reopen persisted, else join a staged ticket,
    // else create a fresh one.
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

    // (re)publish this node's pairing ticket with fresh addresses.
    let ticket = node.share().await?;
    std::fs::write(config.data_dir.join("ticket"), ticket.to_string())?;

    if config.agent != "pi" {
        return Err(anyhow!(
            "unsupported agent {:?} (v1 supports pi)",
            config.agent
        ));
    }
    let adapter = PiAdapter::new(&config.session_dir);
    let engine = Engine::new(adapter, identity, node);
    println!("ssync daemon watching {}", config.session_dir.display());
    engine.run(&config.data_dir.join("status.toml")).await
}

fn read_status(config: &Config) -> Result<StatusReport> {
    let text = std::fs::read_to_string(config.data_dir.join("status.toml"))
        .context("no status yet — start the daemon first")?;
    Ok(toml::from_str(&text)?)
}

fn cmd_status(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let s = read_status(&config)?;
    println!("namespace: {}", s.namespace.as_deref().unwrap_or("(none)"));
    println!("sessions:  {}", s.sessions);
    println!("conflicts: {}", s.conflicts.len());
    Ok(())
}

fn cmd_conflicts(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let s = read_status(&config)?;
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

fn load_identity(path: &Path) -> Result<AgeIdentity> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path)
        .with_context(|| format!("reading age identity {}", path.display()))?;
    if meta.permissions().mode() & 0o077 != 0 {
        return Err(anyhow!(
            "age identity {} is group/world-accessible; run `chmod 600` on it",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading age identity {}", path.display()))?;
    AgeIdentity::from_secret_string(text.trim())
}

/// Write a secret string with `0600` permissions.
fn write_secret(path: &Path, contents: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
