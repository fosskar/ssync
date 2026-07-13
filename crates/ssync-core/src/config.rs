//! On-disk daemon configuration: parsing, defaults, and the per-machine `~/`
//! path expansion that lets one config file serve machines with different
//! home directories.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

/// One agent to sync: its name and the session directory to watch.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Agent name (see `ssync_adapters::adapter_for` for the supported set).
    pub agent: String,
    pub session_dir: PathBuf,
}

/// On-disk daemon configuration (`$XDG_CONFIG_HOME/ssync/config.toml`).
/// Unknown keys are hard errors so removed fields (pre-0.14
/// `namespace_secret_path`/`peers`) and typos fail loudly instead of
/// silently changing the pairing mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Agents to sync side by side (`[[agents]]` tables).
    pub agents: Vec<AgentConfig>,
    /// Shared age identity file (same key on every machine).
    pub age_identity_path: PathBuf,
    pub data_dir: PathBuf,
    /// Cluster membership artifact (`ssync cluster`, clan.vars): shared
    /// namespace secret, recipients, and peer node-ids in one distributable
    /// file. Mutually exclusive with `recipients`.
    #[serde(default)]
    pub cluster_path: Option<PathBuf>,
    /// Override the node key path (default: `data_dir/node.key`).
    #[serde(default)]
    pub node_key_path: Option<PathBuf>,
    /// Peer machines' age recipients (multi-recipient encryption; own recipient
    /// is always included). Empty = shared-identity mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recipients: Vec<String>,
}

impl Config {
    /// Default config path: `$XDG_CONFIG_HOME/ssync/config.toml`, falling back
    /// to `/etc/ssync/config.toml` when no user config exists (the NixOS module
    /// links the generated daemon config there so the CLI finds it).
    pub fn default_path() -> Result<PathBuf> {
        let user = dirs::config_dir()
            .ok_or_else(|| anyhow!("no config dir"))?
            .join("ssync/config.toml");
        Ok(resolve_config_path(
            user,
            Path::new("/etc/ssync/config.toml"),
        ))
    }

    /// Built-in defaults: every known agent whose watched dir exists on this
    /// machine (pi `~/.pi/agent/sessions`, omp `~/.omp/agent/sessions` plus its
    /// blob store `~/.omp/agent/blobs`, claude-code `~/.claude/projects`,
    /// codex `~/.codex/sessions`), falling back to pi alone on a fresh machine.
    pub fn defaults() -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("no home dir"))?;
        let config = dirs::config_dir().ok_or_else(|| anyhow!("no config dir"))?;
        let data = dirs::data_dir().ok_or_else(|| anyhow!("no data dir"))?;
        let known = [
            ("pi", home.join(".pi/agent/sessions")),
            ("omp", home.join(".omp/agent/sessions")),
            ("omp-blobs", home.join(".omp/agent/blobs")),
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
            cluster_path: None,
            node_key_path: None,
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
        if let Some(p) = &mut cfg.cluster_path {
            expand(p);
        }
        if let Some(p) = &mut cfg.node_key_path {
            expand(p);
        }
        if cfg.cluster_path.is_some() && !cfg.recipients.is_empty() {
            return Err(anyhow!(
                "cluster_path replaces recipients — remove them from the config"
            ));
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

/// Pick the config the CLI should read: the user config wherever it exists,
/// otherwise a present system-wide config, otherwise the user path (so `init`
/// still creates it there).
fn resolve_config_path(user: PathBuf, system: &Path) -> PathBuf {
    if !user.exists() && system.exists() {
        system.to_path_buf()
    } else {
        user
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_path_prefers_existing_user_config() {
        let dir = std::env::temp_dir().join(format!("ssync-cfgpath-{}-user", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let user = dir.join("user.toml");
        let system = dir.join("system.toml");
        std::fs::write(&user, "").unwrap();
        std::fs::write(&system, "").unwrap();
        assert_eq!(resolve_config_path(user.clone(), &system), user);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_path_falls_back_to_system_config() {
        let dir = std::env::temp_dir().join(format!("ssync-cfgpath-{}-sys", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let user = dir.join("user.toml");
        let system = dir.join("system.toml");
        std::fs::write(&system, "").unwrap();
        assert_eq!(resolve_config_path(user, &system), system);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn config_path_stays_user_when_neither_exists() {
        let dir = std::env::temp_dir().join(format!("ssync-cfgpath-{}-none", std::process::id()));
        let user = dir.join("user.toml");
        let system = dir.join("system.toml");
        assert_eq!(resolve_config_path(user.clone(), &system), user);
    }

    #[test]
    fn cluster_path_parses_and_expands_tilde() {
        let toml_str = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            cluster_path = "~/cluster.toml"
            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        let cfg = Config::parse(toml_str).unwrap();
        let p = cfg.cluster_path.unwrap();
        assert!(!p.starts_with("~"), "tilde must expand: {}", p.display());
        assert!(p.ends_with("cluster.toml"));
    }

    #[test]
    fn cluster_path_excludes_recipients() {
        // the artifact is the single source of truth for membership; a config
        // that also sets recipients is ambiguous.
        let toml_str = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            cluster_path = "/c/cluster.toml"
            recipients = ["age1x"]
            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        let err = Config::parse(toml_str).unwrap_err().to_string();
        assert!(err.contains("cluster_path"), "{err}");
    }

    #[test]
    fn removed_membership_fields_fail_loudly() {
        // pre-0.14 shared-namespace configs (clan-rendered) must not silently
        // degrade to ticket mode — an unknown key is a hard parse error.
        for legacy in ["namespace_secret_path = \"/n\"", "peers = [\"aaa\"]"] {
            let toml_str = format!(
                r#"
                age_identity_path = "/k"
                data_dir = "/d"
                {legacy}
                [[agents]]
                agent = "pi"
                session_dir = "/s"
            "#
            );
            assert!(
                Config::parse(&toml_str).is_err(),
                "{legacy} must be rejected"
            );
        }
    }

    #[test]
    fn config_round_trips_nix_module_fields() {
        // mirrors the daemon config the nix modules render, including the
        // cluster artifact path.
        let toml_str = r#"
            age_identity_path = "/run/secrets/age/key"
            data_dir = "/var/lib/ssync"
            cluster_path = "/run/secrets/cluster/cluster.toml"
            node_key_path = "/run/secrets/node/key"

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
        assert_eq!(
            cfg.cluster_path.as_deref(),
            Some(Path::new("/run/secrets/cluster/cluster.toml"))
        );
        assert_eq!(
            cfg.node_key_path.as_deref(),
            Some(Path::new("/run/secrets/node/key"))
        );
        // render back out and reparse: the fields survive a full round-trip.
        let rendered = toml::to_string(&cfg).unwrap();
        let cfg2: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(cfg.cluster_path, cfg2.cluster_path);
        assert_eq!(cfg.agents.len(), cfg2.agents.len());
    }

    #[test]
    fn config_expands_leading_tilde_in_paths() {
        let toml_str = r#"
            age_identity_path = "~/.config/ssync/age.key"
            data_dir = "~/.local/share/ssync"
            cluster_path = "/run/secrets/cluster.toml"

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
            cfg.cluster_path.as_deref(),
            Some(Path::new("/run/secrets/cluster.toml"))
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
    fn config_defaults_when_cluster_fields_absent() {
        // a plain ticket-flow config still parses.
        let toml_str = r#"
            age_identity_path = "/a"
            data_dir = "/d"

            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        let cfg: Config = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.cluster_path, None);
        assert_eq!(cfg.node_key_path, None);
    }
}
