//! On-disk daemon configuration: parsing, defaults, and the per-machine `~/`
//! path expansion that lets one config file serve machines with different
//! home directories.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// One agent to sync: its name and the session directory to watch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Agent name (see `ssync_adapters::adapter_for` for the supported set).
    pub agent: String,
    pub session_dir: PathBuf,
}

/// On-disk daemon configuration (`$XDG_CONFIG_HOME/ssync/config.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Agents to sync side by side (`[[agents]]` tables).
    pub agents: Vec<AgentConfig>,
    /// Shared age identity file (same key on every machine).
    pub age_identity_path: PathBuf,
    pub data_dir: PathBuf,
    /// Shared namespace secret (same on every peer). When set, peers auto-join
    /// this one namespace with no ticket exchange (clan provides it).
    #[serde(default)]
    pub namespace_secret_path: Option<PathBuf>,
    /// Override the node key path (default: `data_dir/node.key`).
    #[serde(default)]
    pub node_key_path: Option<PathBuf>,
    /// Peer node-ids to sync with (clan fills this from the other machines).
    #[serde(default)]
    pub peers: Vec<String>,
    /// Peer machines' age recipients (multi-recipient encryption; own recipient
    /// is always included). Empty = shared-identity mode.
    #[serde(default)]
    pub recipients: Vec<String>,
}

impl Config {
    /// Default config path: `$XDG_CONFIG_HOME/ssync/config.toml`.
    pub fn default_path() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .ok_or_else(|| anyhow!("no config dir"))?
            .join("ssync/config.toml"))
    }

    /// Built-in defaults: every known agent whose session dir exists on this
    /// machine (pi `~/.pi/agent/sessions`, omp `~/.omp/agent/sessions`,
    /// claude-code `~/.claude/projects`, codex `~/.codex/sessions`), falling
    /// back to pi alone on a fresh machine.
    pub fn defaults() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
        let config = dirs::config_dir().ok_or_else(|| anyhow!("no config dir"))?;
        let data = dirs::data_dir().ok_or_else(|| anyhow!("no data dir"))?;
        let known = [
            ("pi", home.join(".pi/agent/sessions")),
            ("omp", home.join(".omp/agent/sessions")),
            ("claude-code", home.join(".claude/projects")),
            ("codex", home.join(".codex/sessions")),
        ];
        let mut agents: Vec<AgentConfig> = known
            .iter()
            .filter(|(_, dir)| dir.is_dir())
            .map(|(agent, dir)| AgentConfig {
                agent: agent.to_string(),
                session_dir: dir.clone(),
            })
            .collect();
        if agents.is_empty() {
            agents.push(AgentConfig {
                agent: "pi".to_string(),
                session_dir: home.join(".pi/agent/sessions"),
            });
        }
        Ok(Self {
            agents,
            age_identity_path: config.join("ssync/age.key"),
            data_dir: data.join("ssync"),
            namespace_secret_path: None,
            node_key_path: None,
            peers: Vec::new(),
            recipients: Vec::new(),
        })
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("parsing config {}", path.display()))
    }

    /// Parse config TOML, expanding a leading `~/` in every path so one config
    /// file works across machines with different home directories.
    pub fn parse(text: &str) -> Result<Self> {
        let mut cfg: Self = toml::from_str(text)?;
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
        let expand = |p: &mut PathBuf| {
            if let Ok(rest) = p.strip_prefix("~") {
                *p = home.join(rest);
            }
        };
        for a in &mut cfg.agents {
            expand(&mut a.session_dir);
        }
        expand(&mut cfg.age_identity_path);
        expand(&mut cfg.data_dir);
        if let Some(p) = &mut cfg.namespace_secret_path {
            expand(p);
        }
        if let Some(p) = &mut cfg.node_key_path {
            expand(p);
        }
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing config {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_with_peers() {
        // mirrors the daemon config the nix modules render, including the
        // shared-namespace fields and a multi-element peers array.
        let toml_str = r#"
            age_identity_path = "/run/secrets/age/key"
            data_dir = "/var/lib/ssync"
            namespace_secret_path = "/run/secrets/ns/secret"
            node_key_path = "/run/secrets/node/key"
            peers = [ "aaa", "bbb" ]

            [[agents]]
            agent = "pi"
            session_dir = "/home/x/.pi/agent/sessions"

            [[agents]]
            agent = "omp"
            session_dir = "/home/x/.omp/agent/sessions"
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.agents.len(), 2);
        assert_eq!(cfg.agents[1].agent, "omp");
        assert_eq!(cfg.peers, vec!["aaa".to_string(), "bbb".to_string()]);
        assert_eq!(
            cfg.namespace_secret_path.as_deref(),
            Some(Path::new("/run/secrets/ns/secret"))
        );
        assert_eq!(
            cfg.node_key_path.as_deref(),
            Some(Path::new("/run/secrets/node/key"))
        );
        // render back out and reparse: the fields survive a full round-trip.
        let rendered = toml::to_string(&cfg).unwrap();
        let cfg2: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(cfg.peers, cfg2.peers);
        assert_eq!(cfg.namespace_secret_path, cfg2.namespace_secret_path);
        assert_eq!(cfg.agents.len(), cfg2.agents.len());
    }

    #[test]
    fn config_expands_leading_tilde_in_paths() {
        let toml_str = r#"
            age_identity_path = "~/.config/ssync/age.key"
            data_dir = "~/.local/share/ssync"
            namespace_secret_path = "/run/secrets/ns"

            [[agents]]
            agent = "pi"
            session_dir = "~/.pi/agent/sessions"
        "#;
        let cfg = Config::parse(toml_str).unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(cfg.agents[0].session_dir, home.join(".pi/agent/sessions"));
        assert_eq!(cfg.age_identity_path, home.join(".config/ssync/age.key"));
        assert_eq!(cfg.data_dir, home.join(".local/share/ssync"));
        // absolute paths stay untouched
        assert_eq!(
            cfg.namespace_secret_path.as_deref(),
            Some(Path::new("/run/secrets/ns"))
        );
    }

    #[test]
    fn config_parses_recipients_and_defaults_empty() {
        let toml_str = r#"
            age_identity_path = "/a"
            data_dir = "/d"
            recipients = ["age1pq1peerb", "age1pq1peerc"]

            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        let cfg = Config::parse(toml_str).unwrap();
        assert_eq!(cfg.recipients, ["age1pq1peerb", "age1pq1peerc"]);
        let without = Config::parse(
            r#"
            age_identity_path = "/a"
            data_dir = "/d"

            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#,
        )
        .unwrap();
        assert!(without.recipients.is_empty());
    }

    #[test]
    fn config_defaults_when_shared_fields_absent() {
        // a pre-shared-namespace config (ticket flow) still parses.
        let toml_str = r#"
            age_identity_path = "/a"
            data_dir = "/d"

            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert!(cfg.peers.is_empty());
        assert_eq!(cfg.namespace_secret_path, None);
        assert_eq!(cfg.node_key_path, None);
    }
}
