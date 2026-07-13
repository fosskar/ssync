//! The cluster membership artifact (issue #23): one distributable secret file
//! holding the shared namespace secret, every machine's age recipient, and
//! optionally its node-id. Any membership change is an edit to this file plus
//! redistribution; removal rotates the namespace secret so the evicted machine
//! loses index write access (DECISIONS §4/§6 — no coordinator, the user's own
//! secret channel distributes it). Pure parse/serialize/edit core; all IO and
//! secret generation stay in the CLI shell.

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

/// One machine in the cluster: its age recipient (identity for encryption and
/// for `add`/`rm` addressing) and optionally its iroh node-id (seeds resync).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Machine {
    pub recipient: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
}

/// The parsed artifact. Field invariants (valid hex secret, unique non-empty
/// recipients) hold from construction: `parse` validates, edits preserve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterFile {
    version: u32,
    /// Hex-encoded 32-byte iroh-docs namespace secret; rotated on `remove`.
    namespace_secret: String,
    #[serde(default, rename = "machine")]
    machines: Vec<Machine>,
}

const VERSION: u32 = 1;

impl ClusterFile {
    pub fn new(namespace_secret: [u8; 32]) -> Self {
        Self {
            version: VERSION,
            namespace_secret: hex_encode(&namespace_secret),
            machines: Vec::new(),
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: Self = toml::from_str(text).context("parsing cluster file")?;
        ensure!(
            file.version == VERSION,
            "unsupported cluster file version {} (this ssync knows {VERSION})",
            file.version
        );
        hex_decode(&file.namespace_secret).context("cluster namespace_secret")?;
        for m in &file.machines {
            ensure!(!m.recipient.is_empty(), "empty recipient in cluster file");
        }
        for (i, m) in file.machines.iter().enumerate() {
            if file.machines[..i]
                .iter()
                .any(|o| o.recipient == m.recipient)
            {
                bail!("duplicate recipient {} in cluster file", m.recipient);
            }
        }
        Ok(file)
    }

    pub fn to_toml(&self) -> Result<String> {
        Ok(toml::to_string_pretty(self)?)
    }

    pub fn namespace_secret(&self) -> [u8; 32] {
        // invariant: validated at construction, edits never write invalid hex
        hex_decode(&self.namespace_secret).expect("cluster secret invariant")
    }

    pub fn machines(&self) -> &[Machine] {
        &self.machines
    }

    /// Add a machine; a recipient can only be listed once.
    pub fn add(&mut self, recipient: &str, node_id: Option<String>) -> Result<()> {
        ensure!(!recipient.is_empty(), "recipient must not be empty");
        if self.machines.iter().any(|m| m.recipient == recipient) {
            bail!("recipient {recipient} is already in the cluster");
        }
        self.machines.push(Machine {
            recipient: recipient.to_string(),
            node_id,
        });
        Ok(())
    }

    /// Remove a machine and rotate the namespace secret (the removed machine
    /// still knows the old one — write access dies only with a new secret).
    /// The caller generates `new_secret` so this core stays deterministic.
    pub fn remove(&mut self, recipient: &str, new_secret: [u8; 32]) -> Result<()> {
        let before = self.machines.len();
        self.machines.retain(|m| m.recipient != recipient);
        ensure!(
            self.machines.len() < before,
            "recipient {recipient} is not in the cluster"
        );
        self.namespace_secret = hex_encode(&new_secret);
        Ok(())
    }

    /// Every machine's recipient (multi-recipient encryption; the daemon's
    /// own recipient being listed is harmless — `add_recipients` dedupes).
    pub fn recipients(&self) -> Vec<String> {
        self.machines.iter().map(|m| m.recipient.clone()).collect()
    }

    /// Node-ids to seed resync with, excluding this machine's own.
    pub fn peer_node_ids(&self, own_node_id: &str) -> Vec<String> {
        self.machines
            .iter()
            .filter_map(|m| m.node_id.clone())
            .filter(|id| id != own_node_id)
            .collect()
    }
}

fn hex_encode(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Result<[u8; 32]> {
    ensure!(s.len() == 64, "must be 64 hex chars (32 bytes)");
    let mut out = [0u8; 32];
    for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16);
        let lo = (chunk[1] as char).to_digit(16);
        match (hi, lo) {
            (Some(h), Some(l)) => out[i] = (h * 16 + l) as u8,
            _ => bail!("invalid hex"),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: [u8; 32] = [7; 32];
    const ROTATED: [u8; 32] = [9; 32];

    fn three_machines() -> ClusterFile {
        let mut c = ClusterFile::new(SECRET);
        c.add("age1aaa", Some("node-a".into())).unwrap();
        c.add("age1bbb", None).unwrap();
        c.add("age1ccc", Some("node-c".into())).unwrap();
        c
    }

    #[test]
    fn roundtrips_through_toml() {
        let c = three_machines();
        let text = c.to_toml().unwrap();
        assert!(text.contains("[[machine]]"), "toml shape: {text}");
        let back = ClusterFile::parse(&text).unwrap();
        assert_eq!(back, c);
        assert_eq!(back.namespace_secret(), SECRET);
    }

    #[test]
    fn parse_rejects_unknown_version() {
        let text = three_machines()
            .to_toml()
            .unwrap()
            .replace("version = 1", "version = 2");
        assert!(
            ClusterFile::parse(&text)
                .unwrap_err()
                .to_string()
                .contains("version 2")
        );
    }

    #[test]
    fn parse_rejects_bad_secret() {
        let good = three_machines().to_toml().unwrap();
        let short = good.replace(&hex_encode(&SECRET), "abcd");
        assert!(ClusterFile::parse(&short).is_err());
        let nonhex = good.replace(&hex_encode(&SECRET), &"zz".repeat(32));
        assert!(ClusterFile::parse(&nonhex).is_err());
    }

    #[test]
    fn parse_rejects_duplicate_recipient() {
        let text = "version = 1\nnamespace_secret = \"".to_string()
            + &hex_encode(&SECRET)
            + "\"\n[[machine]]\nrecipient = \"age1x\"\n[[machine]]\nrecipient = \"age1x\"\n";
        let err = ClusterFile::parse(&text).unwrap_err().to_string();
        assert!(err.contains("duplicate recipient age1x"), "{err}");
    }

    #[test]
    fn parse_rejects_empty_recipient() {
        let text = "version = 1\nnamespace_secret = \"".to_string()
            + &hex_encode(&SECRET)
            + "\"\n[[machine]]\nrecipient = \"\"\n";
        assert!(ClusterFile::parse(&text).is_err());
    }

    #[test]
    fn add_rejects_duplicate_and_empty() {
        let mut c = three_machines();
        assert!(c.add("age1bbb", None).is_err());
        assert!(c.add("", None).is_err());
        assert_eq!(c.machines().len(), 3);
    }

    #[test]
    fn remove_drops_machine_and_rotates_secret() {
        let mut c = three_machines();
        c.remove("age1bbb", ROTATED).unwrap();
        assert_eq!(c.machines().len(), 2);
        assert!(c.machines().iter().all(|m| m.recipient != "age1bbb"));
        assert_eq!(c.namespace_secret(), ROTATED);
    }

    #[test]
    fn remove_unknown_recipient_errors_and_keeps_secret() {
        let mut c = three_machines();
        assert!(c.remove("age1zzz", ROTATED).is_err());
        assert_eq!(c.namespace_secret(), SECRET, "no rotation on failed rm");
        assert_eq!(c.machines().len(), 3);
    }

    #[test]
    fn peer_node_ids_skip_own_and_absent() {
        let c = three_machines();
        assert_eq!(c.peer_node_ids("node-a"), ["node-c"]);
        assert_eq!(c.peer_node_ids("elsewhere"), ["node-a", "node-c"]);
    }

    #[test]
    fn recipients_lists_every_machine() {
        assert_eq!(
            three_machines().recipients(),
            ["age1aaa", "age1bbb", "age1ccc"]
        );
    }
}
