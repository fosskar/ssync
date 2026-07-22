//! `ssync cluster` — membership as one distributable artifact (issue #23).
//! Every command edits or adopts the cluster file; the daemon consumes it via
//! `Config::cluster_path`. Distribution stays the user's secret channel
//! (DECISIONS §4/§6: no coordinator).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use ssync_core::Config;
use ssync_core::cluster::ClusterFile;

use crate::{
    ensure_no_recipients, load_or_bootstrap_config, load_or_generate_identity, read_secret_text,
    write_secret,
};

/// Default artifact location: `cluster.toml` next to the config file.
fn default_artifact_path(config_path: &Path) -> PathBuf {
    config_path.with_file_name("cluster.toml")
}

fn load_artifact(config: &Config) -> Result<(PathBuf, ClusterFile)> {
    let path = config
        .cluster_path
        .clone()
        .ok_or_else(|| anyhow!("no cluster_path in config — run `ssync cluster init` first"))?;
    let file = ClusterFile::parse(&read_secret_text(&path)?)
        .with_context(|| format!("cluster file {}", path.display()))?;
    Ok((path, file))
}

fn print_distribute_hint(path: &Path) {
    println!(
        "distribute {} to every machine and restart the daemons",
        path.display()
    );
    println!("(new machines adopt it with `ssync cluster join <file>`)");
}

/// Top-level key insert only (F3): preserves the config's comments and
/// portable `~/` paths — see `ssync_core::insert_cluster_path` — and needs no
/// second `Config::save` reserialize on top of whatever wrote the file.
fn insert_cluster_path_on_disk(config_path: &Path, cluster_path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config {}", config_path.display()))?;
    let new_text = ssync_core::insert_cluster_path(&text, cluster_path);
    let tmp = config_path.with_extension("tmp");
    std::fs::write(&tmp, &new_text)
        .with_context(|| format!("writing config {}", config_path.display()))?;
    std::fs::rename(&tmp, config_path)
        .with_context(|| format!("writing config {}", config_path.display()))
}

/// Create the artifact with this machine as first member and point the config
/// at it. Generates the age identity and node key if missing.
pub async fn cmd_init(config_path: &Path, path: Option<PathBuf>) -> Result<()> {
    let (config, _pre_existed) = load_or_bootstrap_config(config_path)?;
    if let Some(existing) = &config.cluster_path {
        return Err(anyhow!(
            "config already points at cluster file {} — use `ssync cluster add/rm`",
            existing.display()
        ));
    }
    ensure_no_recipients(&config)?;

    let identity = load_or_generate_identity(&config).await?;
    let node_key_path = config.node_key_file();
    let secret = ssync_net::load_or_create_secret_key(&node_key_path).await?;
    let node_id = secret.public().to_string();

    let mut cluster = ClusterFile::new(ssync_net::generate_key_bytes());
    cluster.add(&identity.recipient_string(), Some(node_id.clone()))?;

    let artifact_path = path.unwrap_or_else(|| default_artifact_path(config_path));
    write_secret(&artifact_path, &cluster.to_toml()?)?;
    insert_cluster_path_on_disk(config_path, &artifact_path)?;

    println!("cluster initialized at {}", artifact_path.display());
    println!(
        "namespace: {}",
        ssync_net::namespace_id_of(&cluster.namespace_secret())
    );
    println!(
        "this machine: recipient {}, node-id {node_id}",
        identity.recipient_string()
    );
    println!("next: run `ssync init` on each other machine (prints its recipient and node-id),");
    println!("add it here with `ssync cluster add <recipient> --node-id <id>`, then");
    print_distribute_hint(&artifact_path);
    Ok(())
}

/// Adopt a received cluster file on this machine: copy it to the default
/// location (0600) and point the config at it.
pub fn cmd_join(config_path: &Path, file: &Path) -> Result<()> {
    let (config, _pre_existed) = load_or_bootstrap_config(config_path)?;
    ensure_no_recipients(&config)?;
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("reading cluster file {}", file.display()))?;
    let cluster =
        ClusterFile::parse(&text).with_context(|| format!("cluster file {}", file.display()))?;

    let had_cluster_path = config.cluster_path.is_some();
    let dest = config
        .cluster_path
        .clone()
        .unwrap_or_else(|| default_artifact_path(config_path));
    write_secret(&dest, &text)?;

    // config.cluster_path already pointed here — nothing changed, so skip
    // rewriting the config entirely (F3).
    if !had_cluster_path {
        insert_cluster_path_on_disk(config_path, &dest)?;
    }

    println!("adopted cluster file at {}", dest.display());
    println!(
        "namespace: {}",
        ssync_net::namespace_id_of(&cluster.namespace_secret())
    );
    println!("(re)start the daemon to sync");
    Ok(())
}

/// Add a machine by its age recipient (from `ssync init` on that machine).
pub fn cmd_add(config_path: &Path, recipient: &str, node_id: Option<String>) -> Result<()> {
    let config = Config::load(config_path)?;
    let (path, mut cluster) = load_artifact(&config)?;
    cluster.add(recipient, node_id)?;
    write_secret(&path, &cluster.to_toml()?)?;
    println!("added {recipient} ({} machines)", cluster.machines().len());
    print_distribute_hint(&path);
    Ok(())
}

/// Remove a machine and rotate the namespace secret inside the artifact.
pub fn cmd_rm(config_path: &Path, recipient: &str) -> Result<()> {
    let config = Config::load(config_path)?;
    let (path, mut cluster) = load_artifact(&config)?;
    cluster.remove(recipient, ssync_net::generate_key_bytes())?;
    write_secret(&path, &cluster.to_toml()?)?;
    println!("removed {recipient}; namespace secret rotated",);
    println!(
        "new namespace: {}",
        ssync_net::namespace_id_of(&cluster.namespace_secret())
    );
    println!("the removed machine keeps the OLD namespace until every remaining");
    println!("machine runs the new file — distribute it promptly:");
    print_distribute_hint(&path);
    Ok(())
}

/// List members and the namespace the current secret derives.
pub fn cmd_show(config_path: &Path) -> Result<()> {
    let config = Config::load(config_path)?;
    let (path, cluster) = load_artifact(&config)?;
    println!("cluster file: {}", path.display());
    println!(
        "namespace:    {}",
        ssync_net::namespace_id_of(&cluster.namespace_secret())
    );
    for m in cluster.machines() {
        match &m.node_id {
            Some(id) => println!("machine:      {} node-id {id}", m.recipient),
            None => println!("machine:      {}", m.recipient),
        }
    }
    Ok(())
}

/// Non-interactive artifact assembly for clan.vars: the namespace secret
/// (32 raw bytes from `secret_file`, e.g. `ssync keygen-namespace` output;
/// fresh when absent) plus the given `recipient[:node-id]` members, through
/// the same serializer as the interactive commands (one format, one source
/// of truth). Deterministic given the same secret and member list, so every
/// clan machine assembles the identical artifact at service start.
pub fn cmd_render(out: &Path, secret_file: Option<&Path>, members: &[String]) -> Result<()> {
    let secret = match secret_file {
        Some(p) => read_key_bytes(p)?,
        None => ssync_net::generate_key_bytes(),
    };
    let mut cluster = ClusterFile::new(secret);
    for m in members {
        match m.split_once(':') {
            Some((recipient, node_id)) => cluster.add(recipient, Some(node_id.to_string()))?,
            None => cluster.add(m, None)?,
        }
    }
    write_secret(out, &cluster.to_toml()?)?;
    Ok(())
}

/// Read a 32-byte key/secret file (the `keygen-namespace` format).
fn read_key_bytes(path: &Path) -> Result<[u8; 32]> {
    let bytes = std::fs::read(path).with_context(|| format!("reading key {}", path.display()))?;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("{} must be exactly 32 bytes", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch config + artifact pair; add/rm/show never touch key material.
    fn scratch(tag: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("ssync-cluster-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        let artifact = dir.join("cluster.toml");
        let mut cluster = ClusterFile::new([7; 32]);
        cluster.add("age1aaa", Some("node-a".into())).unwrap();
        cluster.add("age1bbb", None).unwrap();
        write_secret(&artifact, &cluster.to_toml().unwrap()).unwrap();
        std::fs::write(
            &config_path,
            format!(
                "age_identity_path = \"/k\"\ndata_dir = \"/d\"\ncluster_path = \"{}\"\n[[agents]]\nagent = \"pi\"\nsession_dir = \"/s\"\n",
                artifact.display()
            ),
        )
        .unwrap();
        (config_path, artifact)
    }

    #[test]
    fn add_appends_machine_to_artifact() {
        let (config, artifact) = scratch("add");
        cmd_add(&config, "age1ccc", Some("node-c".into())).unwrap();
        let cluster = ClusterFile::parse(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
        assert_eq!(cluster.machines().len(), 3);
        assert_eq!(cluster.peer_node_ids("node-a"), ["node-c"]);
        // secret untouched by add
        assert_eq!(cluster.namespace_secret(), [7; 32]);
    }

    #[test]
    fn rm_drops_machine_and_rotates_secret_on_disk() {
        let (config, artifact) = scratch("rm");
        cmd_rm(&config, "age1bbb").unwrap();
        let cluster = ClusterFile::parse(&std::fs::read_to_string(&artifact).unwrap()).unwrap();
        assert_eq!(cluster.machines().len(), 1);
        assert_ne!(cluster.namespace_secret(), [7; 32], "rm must rotate");
    }

    #[test]
    fn rm_unknown_recipient_leaves_artifact_untouched() {
        let (config, artifact) = scratch("rm-unknown");
        let before = std::fs::read_to_string(&artifact).unwrap();
        assert!(cmd_rm(&config, "age1zzz").is_err());
        assert_eq!(std::fs::read_to_string(&artifact).unwrap(), before);
    }

    #[test]
    fn add_refuses_world_readable_artifact() {
        use std::os::unix::fs::PermissionsExt;
        let (config, artifact) = scratch("perm");
        std::fs::set_permissions(&artifact, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = cmd_add(&config, "age1ccc", None).unwrap_err().to_string();
        assert!(err.contains("group/world"), "{err}");
    }

    #[test]
    fn join_adopts_file_and_sets_config() {
        let dir = std::env::temp_dir().join(format!("ssync-cluster-{}-join", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            "age_identity_path = \"/k\"\ndata_dir = \"/d\"\n[[agents]]\nagent = \"pi\"\nsession_dir = \"/s\"\n",
        )
        .unwrap();
        let received = dir.join("received.toml");
        std::fs::write(&received, ClusterFile::new([3; 32]).to_toml().unwrap()).unwrap();

        cmd_join(&config_path, &received).unwrap();

        let config = Config::load(&config_path).unwrap();
        let dest = config.cluster_path.expect("cluster_path set");
        assert_eq!(dest, dir.join("cluster.toml"));
        let adopted = ClusterFile::parse(&read_secret_text(&dest).unwrap()).unwrap();
        assert_eq!(adopted.namespace_secret(), [3; 32]);
    }

    #[test]
    fn join_preserves_config_text_when_cluster_path_already_set() {
        // F3: `dest` is already `config.cluster_path`, so re-adopting must
        // not touch the config file at all — only the artifact changes.
        let (config_path, artifact) = scratch("join-noop");
        let before = std::fs::read_to_string(&config_path).unwrap();

        let received = config_path.with_file_name("received.toml");
        std::fs::write(&received, ClusterFile::new([9; 32]).to_toml().unwrap()).unwrap();
        cmd_join(&config_path, &received).unwrap();

        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            before,
            "config text must be byte-identical when cluster_path was already set"
        );
        let adopted = ClusterFile::parse(&read_secret_text(&artifact).unwrap()).unwrap();
        assert_eq!(adopted.namespace_secret(), [9; 32], "artifact must update");
    }

    #[tokio::test]
    async fn init_keeps_config_comments_and_agents_when_pre_existing() {
        // F3: cmd_init must textually insert cluster_path, not reserialize —
        // a pre-existing config's comments and tables survive verbatim.
        let dir =
            std::env::temp_dir().join(format!("ssync-cluster-{}-init-text", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        let identity = ssync_crypto::AgeIdentity::generate().await.unwrap();
        write_secret(&dir.join("age.key"), &identity.to_secret_string()).unwrap();
        let original = format!(
            "# hand-written config, keep me\nage_identity_path = \"{}\"\ndata_dir = \"{}\"\n# agents to sync\n[[agents]]\nagent = \"pi\"\nsession_dir = \"{}\"\n",
            dir.join("age.key").display(),
            dir.join("data").display(),
            dir.join("sessions").display(),
        );
        std::fs::write(&config_path, &original).unwrap();

        cmd_init(&config_path, None).await.unwrap();

        let after = std::fs::read_to_string(&config_path).unwrap();
        for line in original.lines() {
            assert!(
                after.contains(line),
                "line lost by cmd_init: {line}\n{after}"
            );
        }
        let config = Config::load(&config_path).unwrap();
        assert_eq!(config.cluster_path, Some(dir.join("cluster.toml")));
        assert_eq!(config.agents.len(), 1);
    }

    #[test]
    fn render_writes_parseable_artifact_with_members() {
        let dir = std::env::temp_dir().join(format!("ssync-cluster-{}-render", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("cluster.toml");
        cmd_render(
            &out,
            None,
            &["age1aaa:node-a".to_string(), "age1bbb".to_string()],
        )
        .unwrap();
        let c = ClusterFile::parse(&read_secret_text(&out).unwrap()).unwrap();
        assert_eq!(c.recipients(), ["age1aaa", "age1bbb"]);
        assert_eq!(c.peer_node_ids("elsewhere"), ["node-a"]);

        // without --secret-file each render draws a fresh secret
        let first = c.namespace_secret();
        cmd_render(&out, None, &["age1aaa:node-a".to_string()]).unwrap();
        let again = ClusterFile::parse(&read_secret_text(&out).unwrap()).unwrap();
        assert_ne!(again.namespace_secret(), first);
    }

    #[test]
    fn render_with_secret_file_is_deterministic() {
        // clan assembles the artifact on every service start; same secret +
        // same members must yield the identical file on every machine.
        let dir =
            std::env::temp_dir().join(format!("ssync-cluster-{}-render-det", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let secret = dir.join("ns.secret");
        std::fs::write(&secret, [5u8; 32]).unwrap();
        let members = ["age1aaa:node-a".to_string(), "age1bbb:node-b".to_string()];

        let out1 = dir.join("one.toml");
        let out2 = dir.join("two.toml");
        cmd_render(&out1, Some(&secret), &members).unwrap();
        cmd_render(&out2, Some(&secret), &members).unwrap();
        assert_eq!(
            std::fs::read_to_string(&out1).unwrap(),
            std::fs::read_to_string(&out2).unwrap()
        );
        let c = ClusterFile::parse(&read_secret_text(&out1).unwrap()).unwrap();
        assert_eq!(c.namespace_secret(), [5u8; 32]);

        // a truncated secret file is a hard error, not a silent fresh secret
        std::fs::write(&secret, [5u8; 16]).unwrap();
        assert!(cmd_render(&out1, Some(&secret), &members).is_err());
    }
}
