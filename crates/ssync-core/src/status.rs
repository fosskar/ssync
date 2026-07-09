//! The status snapshot contract between the daemon and the CLI.

use serde::{Deserialize, Serialize};

/// A snapshot the daemon writes to `data_dir/status.toml` so `ssync status` /
/// `ssync conflicts` can report without opening the (single-process) store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusReport {
    pub namespace: Option<String>,
    pub sessions: usize,
    pub conflicts: Vec<String>,
    /// Known peers and the transport path each connection uses (direct /
    /// relay / mixed / unknown) — the evidence for the cross-network check.
    /// Defaulted so a status.toml from an older daemon still parses.
    #[serde(default)]
    pub peers: Vec<PeerStatus>,
}

/// One peer as shown by `ssync status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerStatus {
    pub id: String,
    pub path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_report_parses_without_peers_field() {
        // status.toml written by an older daemon has no peers table
        let report: StatusReport = toml::from_str(
            r#"
            sessions = 3
            conflicts = []
            "#,
        )
        .unwrap();
        assert!(report.peers.is_empty());
    }
}
