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
    /// Session paths to withhold from sync (issue #14): `*`-glob patterns
    /// against the session-dir-relative path (project dir + file name). A
    /// matching session is frozen on every machine — never published,
    /// never materialized, never deleted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}
/// One `[[path_map]]` prefix pair. `local` may start with `~` (expanded on
/// this machine); `canonical` is an absolute literal, identical everywhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathMapEntry {
    pub local: PathBuf,
    pub canonical: PathBuf,
}

/// How the node finds and reaches peers beyond the local network
/// (DECISIONS §6, issue #11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Discovery {
    /// n0 public infrastructure: DNS/pkarr node lookup plus the public
    /// relays (or the `relay` override). mDNS on the LAN as always.
    #[default]
    Default,
    /// Never leave the local network: no relays, no DNS/pkarr — peers are
    /// found by mDNS alone. Pair with the cluster artifact (node-id-only).
    LanOnly,
}

impl Discovery {
    fn is_default(&self) -> bool {
        *self == Self::Default
    }
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
    /// Prefix map bridging differing absolute paths (issue #13, map #42):
    /// this machine's `local` prefixes ↔ the mesh-wide `canonical` form.
    /// Opt-in; empty = every path is its own canonical form (store-as-is).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path_map: Vec<PathMapEntry>,
    /// The home dir canonical paths are relative to — required only for omp's
    /// home-relative dir encoding of mapped canonical paths (#46 amendment).
    /// Must equal the real $HOME of the machines hosting canonical paths.
    #[serde(default)]
    pub canonical_home: Option<PathBuf>,
    /// Override the iroh relay (self-hosted; docs/setup.md "Self-hosted
    /// relay"). Replaces the n0 public relays entirely — every machine must
    /// set the same URL. Absent = n0 defaults (DECISIONS §6).
    #[serde(default)]
    pub relay: Option<String>,
    /// Peer reach beyond the LAN; `lan-only` disables all n0 infrastructure.
    #[serde(default, skip_serializing_if = "Discovery::is_default")]
    pub discovery: Discovery,
    /// How often the daemon re-initiates sync with its known peers, in
    /// seconds (default 60). Lower = faster recovery after a peer restart,
    /// at the cost of more idle chatter.
    #[serde(default)]
    pub resync_interval_secs: Option<u64>,
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
                exclude: Vec::new(),
            })
            .collect();
        if agents.is_empty() {
            agents.push(AgentConfig {
                agent: "pi".to_string(),
                session_dir: home.join(".pi/agent/sessions"),
                exclude: Vec::new(),
            });
        }
        Ok(Self {
            agents,
            age_identity_path: config.join("ssync/age.key"),
            data_dir: data.join("ssync"),
            cluster_path: None,
            node_key_path: None,
            recipients: Vec::new(),
            path_map: Vec::new(),
            canonical_home: None,
            relay: None,
            discovery: Discovery::Default,
            resync_interval_secs: None,
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
        for e in &mut cfg.path_map {
            expand(&mut e.local);
            // `~` in canonical would be machine-dependent — refuse before the
            // absolute check can be fooled by this machine's expansion.
            if e.canonical.starts_with("~") {
                return Err(anyhow!(
                    "path_map canonical {} must be an absolute literal (no ~)",
                    e.canonical.display()
                ));
            }
        }
        if let Some(h) = &cfg.canonical_home
            && !h.is_absolute()
        {
            return Err(anyhow!("canonical_home {} must be absolute", h.display()));
        }
        if let Some(r) = &cfg.relay
            && r.parse::<ssync_net::iroh::RelayUrl>().is_err()
        {
            return Err(anyhow!("relay {r:?} is not a valid relay url"));
        }
        if cfg.discovery == Discovery::LanOnly && cfg.relay.is_some() {
            return Err(anyhow!(
                "discovery = \"lan-only\" never uses a relay — remove the relay key"
            ));
        }
        if cfg.resync_interval_secs == Some(0) {
            return Err(anyhow!("resync_interval_secs must be at least 1"));
        }
        cfg.build_path_map()?; // fail loudly at parse, not daemon-time (#46)
        // omp's wire encoding is home-relative; without the home context
        // every mapped omp dir would fail at daemon-time instead
        if !cfg.path_map.is_empty()
            && cfg.canonical_home.is_none()
            && cfg.agents.iter().any(|a| a.agent == "omp")
        {
            return Err(anyhow!(
                "path_map with an omp agent requires canonical_home (the home canonical paths are relative to)"
            ));
        }
        if cfg.cluster_path.is_some() && !cfg.recipients.is_empty() {
            return Err(anyhow!(
                "cluster_path replaces recipients — remove them from the config"
            ));
        }
        Ok(cfg)
    }

    /// The validated [`PathMap`] this config describes (empty = inert).
    pub fn build_path_map(&self) -> Result<crate::PathMap> {
        crate::PathMap::new(
            self.path_map
                .iter()
                .map(|e| {
                    (
                        e.local.to_string_lossy().into_owned(),
                        e.canonical.to_string_lossy().into_owned(),
                    )
                })
                .collect(),
        )
    }

    /// Atomic overwrite (temp + rename), mirroring `SyncState::save`
    /// (reconcile.rs) — a crash mid-write must never leave a truncated config.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, toml::to_string_pretty(self)?)
            .with_context(|| format!("writing config {}", path.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing config {}", path.display()))
    }

    /// The node key path to use: `node_key_path` when set, else
    /// `data_dir/node.key`.
    pub fn node_key_file(&self) -> PathBuf {
        self.node_key_path
            .clone()
            .unwrap_or_else(|| self.data_dir.join("node.key"))
    }
}

/// Prepend a top-level `cluster_path = "…"` key to existing config text.
/// Prepend, not append: a bare key must precede every table header
/// (`[[agents]]` etc.), and prepending is the only placement that reparses
/// correctly no matter where the caller's tables start. Textual, not
/// parse-mutate-reserialize: `parse`'s cross-machine promise (above) rests on
/// this file's comments and portable `~/` paths surviving untouched, and only
/// leaving every other line byte-for-byte alone guarantees that.
pub fn insert_cluster_path(text: &str, path: &Path) -> String {
    let escaped = toml::Value::String(path.display().to_string()).to_string();
    format!("cluster_path = {escaped}\n{text}")
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
    fn agent_exclude_parses_and_defaults_empty() {
        let toml_str = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            [[agents]]
            agent = "pi"
            session_dir = "/s"
            exclude = ["*client-x*"]
            [[agents]]
            agent = "omp"
            session_dir = "/o"
        "#;
        let cfg = Config::parse(toml_str).unwrap();
        assert_eq!(cfg.agents[0].exclude, ["*client-x*"]);
        assert!(cfg.agents[1].exclude.is_empty());
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
    fn discovery_parses_lan_only_and_defaults_to_n0() {
        let toml_str = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            discovery = "lan-only"
            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        let cfg = Config::parse(toml_str).unwrap();
        assert_eq!(cfg.discovery, Discovery::LanOnly);
        let cfg = Config::parse(&toml_str.replace("discovery = \"lan-only\"\n", "")).unwrap();
        assert_eq!(cfg.discovery, Discovery::Default);
        // unknown variants fail loudly like unknown keys
        assert!(Config::parse(&toml_str.replace("lan-only", "lan_only")).is_err());
    }

    #[test]
    fn lan_only_discovery_excludes_a_relay() {
        // lan-only means "never leave the local network"; a relay URL under
        // it would silently never be used.
        let toml_str = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            discovery = "lan-only"
            relay = "https://relay.example.com"
            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        let err = Config::parse(toml_str).unwrap_err().to_string();
        assert!(err.contains("lan-only"), "{err}");
    }

    #[test]
    fn resync_interval_parses_and_rejects_zero() {
        let toml_str = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            resync_interval_secs = 15
            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        let cfg = Config::parse(toml_str).unwrap();
        assert_eq!(cfg.resync_interval_secs, Some(15));
        let cfg = Config::parse(&toml_str.replace("resync_interval_secs = 15\n", "")).unwrap();
        assert_eq!(cfg.resync_interval_secs, None);
        let err = Config::parse(&toml_str.replace("= 15", "= 0"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("resync_interval_secs"), "{err}");
    }

    #[test]
    fn saved_defaults_omit_unset_knobs() {
        // `ssync init` writes defaults(); the generated file must not pin
        // discovery/resync values the user never chose.
        let text = toml::to_string_pretty(&Config {
            agents: vec![],
            age_identity_path: "/k".into(),
            data_dir: "/d".into(),
            cluster_path: None,
            node_key_path: None,
            recipients: vec![],
            path_map: vec![],
            canonical_home: None,
            relay: None,
            discovery: Discovery::Default,
            resync_interval_secs: None,
        })
        .unwrap();
        assert!(!text.contains("discovery"), "{text}");
        assert!(!text.contains("resync_interval_secs"), "{text}");
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
            discovery = "lan-only"
            resync_interval_secs = 15

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
        assert_eq!(cfg.discovery, Discovery::LanOnly);
        assert_eq!(cfg.resync_interval_secs, Some(15));
        // render back out and reparse: the fields survive a full round-trip.
        let rendered = toml::to_string(&cfg).unwrap();
        let cfg2: Config = toml::from_str(&rendered).unwrap();
        assert_eq!(cfg.cluster_path, cfg2.cluster_path);
        assert_eq!(cfg.discovery, cfg2.discovery);
        assert_eq!(cfg.resync_interval_secs, cfg2.resync_interval_secs);
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

    #[test]
    fn save_writes_no_tmp_sibling_and_round_trips() {
        let dir = std::env::temp_dir().join(format!("ssync-cfgsave-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let cfg = Config::defaults().unwrap();
        cfg.save(&path).unwrap();
        assert!(!path.with_extension("tmp").exists(), "tmp sibling leaked");
        let loaded = Config::load(&path).unwrap();
        assert_eq!(loaded.data_dir, cfg.data_dir);
        assert_eq!(loaded.agents.len(), cfg.agents.len());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn node_key_file_defaults_under_data_dir() {
        let mut cfg = Config::defaults().unwrap();
        cfg.data_dir = PathBuf::from("/d");
        cfg.node_key_path = None;
        assert_eq!(cfg.node_key_file(), PathBuf::from("/d/node.key"));
        cfg.node_key_path = Some(PathBuf::from("/custom/key"));
        assert_eq!(cfg.node_key_file(), PathBuf::from("/custom/key"));
    }

    #[test]
    fn insert_cluster_path_prepends_before_tables_and_keeps_comments() {
        let text = "# my config\nage_identity_path = \"/k\"\ndata_dir = \"/d\"\n# per-agent list\n[[agents]]\nagent = \"pi\"\nsession_dir = \"/s\"\n";
        let out = insert_cluster_path(text, Path::new("/c/cluster.toml"));
        assert!(
            out.starts_with("cluster_path = "),
            "must be a top-level key, before any table: {out}"
        );
        for line in text.lines() {
            assert!(out.contains(line), "line lost by rewrite: {line}\n{out}");
        }
        let cfg = Config::parse(&out).unwrap();
        assert_eq!(
            cfg.cluster_path.as_deref(),
            Some(Path::new("/c/cluster.toml"))
        );
        assert_eq!(cfg.agents.len(), 1);
        assert_eq!(cfg.data_dir, Path::new("/d"));
    }

    #[test]
    fn insert_cluster_path_escapes_quotes_and_backslashes() {
        let text = "age_identity_path = \"/k\"\ndata_dir = \"/d\"\n[[agents]]\nagent = \"pi\"\nsession_dir = \"/s\"\n";
        let path = Path::new("C:\\weird \"name\"\\cluster.toml");
        let out = insert_cluster_path(text, path);
        let cfg = Config::parse(&out).unwrap();
        assert_eq!(cfg.cluster_path.as_deref(), Some(path));
    }
}

#[cfg(test)]
mod pathmap_config_tests {
    use super::*;

    #[test]
    fn path_map_parses_expands_tilde_and_builds() {
        let toml_str = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            canonical_home = "/home/simon"
            [[path_map]]
            local = "~/work"
            canonical = "/home/simon/Projects"
            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        let cfg = Config::parse(toml_str).unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(cfg.path_map[0].local, home.join("work"));
        assert_eq!(
            cfg.canonical_home.as_deref(),
            Some(Path::new("/home/simon"))
        );
        let map = cfg.build_path_map().unwrap();
        let local = home.join("work/x").to_string_lossy().into_owned();
        assert_eq!(
            map.canonical_of(&local).unwrap().as_deref(),
            Some("/home/simon/Projects/x")
        );
    }

    #[test]
    fn path_map_validation_fails_at_parse() {
        // duplicate canonical prefixes must be a hard parse error, not a
        // daemon-time surprise (decision #46)
        let toml_str = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            [[path_map]]
            local = "/a"
            canonical = "/c"
            [[path_map]]
            local = "/b"
            canonical = "/c"
            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        assert!(Config::parse(toml_str).is_err());
        // canonical must be absolute — ~ is machine-dependent by design
        let tilde_canonical = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            [[path_map]]
            local = "/a"
            canonical = "~/Projects"
            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        assert!(Config::parse(tilde_canonical).is_err());
    }

    #[test]
    fn canonical_home_must_be_absolute() {
        let toml_str = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            canonical_home = "relative"
            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        assert!(Config::parse(toml_str).is_err());
    }

    #[test]
    fn absent_path_map_defaults_empty() {
        let cfg = Config::parse(
            r#"
            age_identity_path = "/k"
            data_dir = "/d"
            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#,
        )
        .unwrap();
        assert!(cfg.path_map.is_empty());
        assert!(cfg.canonical_home.is_none());
        assert!(cfg.build_path_map().unwrap().is_empty());
    }
}

#[cfg(test)]
mod pathmap_omp_tests {
    use super::*;

    #[test]
    fn omp_with_path_map_requires_canonical_home() {
        let toml_str = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            [[path_map]]
            local = "/a"
            canonical = "/c"
            [[agents]]
            agent = "omp"
            session_dir = "/s"
        "#;
        let err = Config::parse(toml_str).unwrap_err().to_string();
        assert!(err.contains("canonical_home"), "{err}");
        // pi-only maps need no home context
        let pi_only = toml_str.replace("agent = \"omp\"", "agent = \"pi\"");
        assert!(Config::parse(&pi_only).is_ok());
    }
}

#[cfg(test)]
mod relay_tests {
    use super::*;

    #[test]
    fn relay_parses_and_validates_at_parse_time() {
        let toml_str = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            relay = "https://relay.example.com"
            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        let cfg = Config::parse(toml_str).unwrap();
        assert_eq!(cfg.relay.as_deref(), Some("https://relay.example.com"));

        // a malformed url is a hard parse error, not a daemon-time surprise
        let bad = toml_str.replace("https://relay.example.com", "not a url");
        assert!(Config::parse(&bad).is_err());

        // absent = n0 public defaults (DECISIONS §6)
        let none = r#"
            age_identity_path = "/k"
            data_dir = "/d"
            [[agents]]
            agent = "pi"
            session_dir = "/s"
        "#;
        assert!(Config::parse(none).unwrap().relay.is_none());
    }
}
