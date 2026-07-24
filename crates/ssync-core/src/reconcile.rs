//! The pure sync decision. Given a snapshot of the local session dir, a snapshot
//! of the synced index, and what we last did per key, decide the [`Action`]s to
//! take — no iroh, no filesystem, no clock. Every echo-loop / resurrection /
//! deletion / divergence invariant lives here, so it is exercised by cheap,
//! deterministic unit tests instead of only through two-node network tests.
//!
//! The engine's loop is a thin shell: snapshot both sides, call [`reconcile`],
//! execute the returned actions, and record what it did back into [`SyncState`].
//! That carried state is what dissolves the old `seen` / `exported` / `Deleted`
//! structures: a self-write never echoes because the next snapshot matches the
//! state we recorded when we made it.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use ssync_net::iroh_blobs::Hash;

/// `(mtime_micros, len)` — cheap change detector for a file, on the iroh-docs
/// timestamp scale so it compares directly against index timestamps.
pub type Stamp = (u64, u64);

/// A session file present under the session root at snapshot time.
#[derive(Debug, Clone)]
pub struct LocalFile {
    /// Index key: `{agent}/{relative_path}`.
    pub key: String,
    pub stamp: Stamp,
}

impl LocalFile {
    fn mtime(&self) -> u64 {
        self.stamp.0
    }
}

/// The winning index entry for a key (newest across all authors); `hash = None`
/// is a deletion tombstone.
#[derive(Debug, Clone, Copy)]
pub struct IndexHead {
    pub timestamp: u64,
    pub hash: Option<Hash>,
}

/// Per-key index snapshot: the winner plus how many distinct live (non-tombstone)
/// content hashes exist across authors (`> 1` ⇒ genuinely diverged).
#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub head: IndexHead,
    pub distinct_live: usize,
    /// `Adapter::append_only` — line-merging a compacting format would
    /// corrupt it, so divergence falls back to newest-wins (DECISIONS §8).
    pub merge_allowed: bool,
}

/// What the engine last materialised for a key. Carried between passes so
/// decisions are idempotent and self-writes never bounce back.
#[derive(Debug, Clone, Default)]
pub struct KeyState {
    /// Stamp of the file the last time we imported it or wrote it back.
    pub import_stamp: Option<Stamp>,
    /// Last winner we materialised: `Some(Some(h))` wrote blob `h`,
    /// `Some(None)` applied a tombstone, `None` never acted.
    pub export_hash: Option<Option<Hash>>,
}

/// All carried per-key state. Owned by the single run loop; never shared.
#[derive(Debug, Default)]
pub struct SyncState {
    pub keys: HashMap<String, KeyState>,
    /// Fingerprint of the recipient set the published blobs were encrypted to.
    /// A mismatch with the configured set forces a full re-publish (issue #22:
    /// plaintext dedup alone would keep old ciphertext readable by a removed
    /// key and unreadable by an added one).
    pub recipients: Option<String>,
}

impl SyncState {
    fn get(&self, key: &str) -> Option<&KeyState> {
        self.keys.get(key)
    }

    /// The file at `stamp` now matches blob `hash` (published or deduped).
    pub fn settle_import(&mut self, key: &str, stamp: Stamp, hash: Hash) {
        let ks = self.keys.entry(key.to_string()).or_default();
        ks.import_stamp = Some(stamp);
        ks.export_hash = Some(Some(hash));
    }

    /// Blob `hash` was written back; `stamp` keeps it from re-importing.
    pub fn settle_write(&mut self, key: &str, hash: Hash, stamp: Option<Stamp>) {
        let ks = self.keys.entry(key.to_string()).or_default();
        ks.export_hash = Some(Some(hash));
        ks.import_stamp = stamp;
    }

    /// The key was deleted here (local remove or index tombstone).
    pub fn settle_delete(&mut self, key: &str) {
        let ks = self.keys.entry(key.to_string()).or_default();
        ks.export_hash = Some(None);
        ks.import_stamp = None;
    }

    /// Write the carried state atomically (temp + rename), so a restart
    /// resumes instead of re-verifying every session. Small: one row per key.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let dto = StateFile {
            recipients: self.recipients.clone(),
            keys: self
                .keys
                .iter()
                .map(|(k, s)| (k.clone(), KeyStateDto::from(s)))
                .collect(),
        };
        let text = toml::to_string(&dto)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Load persisted state; absent or unreadable file ⇒ fresh state (the
    /// engine then re-verifies, which is always safe — just slower).
    pub fn load(path: &Path) -> Self {
        let Some(dto) = std::fs::read_to_string(path)
            .ok()
            .and_then(|text| toml::from_str::<StateFile>(&text).ok())
        else {
            return Self::default();
        };
        Self {
            recipients: dto.recipients,
            keys: dto
                .keys
                .into_iter()
                .filter_map(|(k, s)| Some((k, s.try_into().ok()?)))
                .collect(),
        }
    }
}

/// On-disk form of [`SyncState`] (`data_dir/state.toml`).
#[derive(Serialize, Deserialize)]
struct StateFile {
    // scalar first: TOML requires values before tables (the keys map).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recipients: Option<String>,
    keys: HashMap<String, KeyStateDto>,
}

/// [`KeyState`] with the hash as hex; `export = "deleted"` is a tombstone.
#[derive(Serialize, Deserialize)]
struct KeyStateDto {
    #[serde(skip_serializing_if = "Option::is_none")]
    import_stamp: Option<Stamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    export: Option<String>,
}

const DELETED: &str = "deleted";

impl From<&KeyState> for KeyStateDto {
    fn from(s: &KeyState) -> Self {
        Self {
            import_stamp: s.import_stamp,
            export: s.export_hash.map(|h| match h {
                Some(h) => h.to_string(),
                None => DELETED.to_string(),
            }),
        }
    }
}

impl TryFrom<KeyStateDto> for KeyState {
    type Error = anyhow::Error;

    fn try_from(dto: KeyStateDto) -> anyhow::Result<Self> {
        let export_hash = match dto.export.as_deref() {
            None => None,
            Some(DELETED) => Some(None),
            Some(hex) => Some(Some(hex.parse()?)),
        };
        Ok(Self {
            import_stamp: dto.import_stamp,
            export_hash,
        })
    }
}

/// A single side effect for the engine shell to execute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Encrypt the file and upsert the index entry. `winner` is the current
    /// index hash (if any), so a content-identical no-op still settles state.
    Import {
        key: String,
        stamp: Stamp,
        winner: Option<Hash>,
    },
    /// Decrypt the winning blob and write it into the session dir.
    WriteFile { key: String, hash: Hash },
    /// Remove the local file — a peer's deletion won.
    DeleteLocal { key: String },
    /// Tombstone the key in the index — the local file was deleted here.
    Tombstone { key: String },
    /// Recompute the lossless union for a diverged key and republish if changed.
    Merge { key: String },
}

/// Decide the actions for one pass. Pure: same inputs ⇒ same output.
pub fn reconcile(
    state: &SyncState,
    local: &[LocalFile],
    index: &HashMap<String, IndexEntry>,
) -> Vec<Action> {
    let local_by_key: HashMap<&str, &LocalFile> =
        local.iter().map(|f| (f.key.as_str(), f)).collect();
    // Guard: a transiently empty/unmounted dir must never wipe peers, so no
    // tombstones are emitted while the whole dir reads as empty.
    let dir_empty = local.is_empty();

    let mut keys: HashSet<&str> = HashSet::new();
    keys.extend(local_by_key.keys().copied());
    keys.extend(index.keys().map(String::as_str));
    // deterministic order (helps tests and reproducible logs)
    let mut keys: Vec<&str> = keys.into_iter().collect();
    keys.sort_unstable();

    let mut actions = Vec::new();
    for key in keys {
        let local = local_by_key.get(key).copied();
        let entry = index.get(key);
        let ks = state.get(key);

        match (local, entry) {
            (Some(f), Some(e)) => match e.head.hash {
                Some(winner) => {
                    let unchanged = ks.and_then(|k| k.import_stamp) == Some(f.stamp);
                    if !unchanged {
                        actions.push(Action::Import {
                            key: key.to_string(),
                            stamp: f.stamp,
                            winner: Some(winner),
                        });
                    } else if ks.and_then(|k| k.export_hash.flatten()) != Some(winner) {
                        // file untouched here, but a peer pushed a newer version
                        actions.push(Action::WriteFile {
                            key: key.to_string(),
                            hash: winner,
                        });
                    }
                }
                None => {
                    // tombstone wins but the file is present locally
                    if f.mtime() <= e.head.timestamp {
                        actions.push(Action::DeleteLocal {
                            key: key.to_string(),
                        });
                    } else {
                        // written after the deletion ⇒ genuine recreate
                        actions.push(Action::Import {
                            key: key.to_string(),
                            stamp: f.stamp,
                            winner: None,
                        });
                    }
                }
            },
            (Some(f), None) => actions.push(Action::Import {
                key: key.to_string(),
                stamp: f.stamp,
                winner: None,
            }),
            // a live index entry with no local file (tombstone + absent is a no-op)
            (None, Some(e)) => {
                if let Some(hash) = e.head.hash {
                    let had_locally = ks
                        .map(|k| k.import_stamp.is_some() || matches!(k.export_hash, Some(Some(_))))
                        .unwrap_or(false);
                    if had_locally {
                        // we materialised this file and it is now gone ⇒ deleted here
                        if !dir_empty {
                            actions.push(Action::Tombstone {
                                key: key.to_string(),
                            });
                        }
                    } else {
                        actions.push(Action::WriteFile {
                            key: key.to_string(),
                            hash,
                        });
                    }
                }
            }
            (None, None) => {}
        }

        // Divergence is orthogonal to the presence decision above.
        if let Some(e) = entry
            && e.head.hash.is_some()
            && e.distinct_live > 1
            && e.merge_allowed
        {
            actions.push(Action::Merge {
                key: key.to_string(),
            });
        }
    }
    actions
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(b: &[u8]) -> Hash {
        Hash::new(b)
    }

    fn local(key: &str, stamp: Stamp) -> LocalFile {
        LocalFile {
            key: key.to_string(),
            stamp,
        }
    }

    fn live(hash: Hash, ts: u64, distinct_live: usize) -> IndexEntry {
        IndexEntry {
            head: IndexHead {
                timestamp: ts,
                hash: Some(hash),
            },
            distinct_live,
            merge_allowed: true,
        }
    }

    fn tombstone(ts: u64) -> IndexEntry {
        IndexEntry {
            head: IndexHead {
                timestamp: ts,
                hash: None,
            },
            distinct_live: 0,
            merge_allowed: true,
        }
    }

    fn index(pairs: &[(&str, IndexEntry)]) -> HashMap<String, IndexEntry> {
        pairs
            .iter()
            .map(|(k, e)| (k.to_string(), e.clone()))
            .collect()
    }

    fn state(pairs: &[(&str, KeyState)]) -> SyncState {
        SyncState {
            keys: pairs
                .iter()
                .map(|(k, s)| (k.to_string(), s.clone()))
                .collect(),
            recipients: None,
        }
    }

    #[test]
    fn new_local_file_is_imported() {
        let a = reconcile(
            &SyncState::default(),
            &[local("pi/p/s", (5, 10))],
            &HashMap::new(),
        );
        assert_eq!(
            a,
            vec![Action::Import {
                key: "pi/p/s".into(),
                stamp: (5, 10),
                winner: None,
            }]
        );
    }

    #[test]
    fn unchanged_settled_file_produces_nothing() {
        let h = hash(b"v1");
        let st = state(&[(
            "pi/p/s",
            KeyState {
                import_stamp: Some((5, 10)),
                export_hash: Some(Some(h)),
            },
        )]);
        let a = reconcile(
            &st,
            &[local("pi/p/s", (5, 10))],
            &index(&[("pi/p/s", live(h, 5, 1))]),
        );
        assert!(a.is_empty(), "settled file must be a no-op, got {a:?}");
    }

    #[test]
    fn remote_only_session_is_written() {
        let h = hash(b"v1");
        let a = reconcile(
            &SyncState::default(),
            &[],
            &index(&[("pi/p/s", live(h, 5, 1))]),
        );
        assert_eq!(
            a,
            vec![Action::WriteFile {
                key: "pi/p/s".into(),
                hash: h
            }]
        );
    }

    #[test]
    fn peer_update_writes_over_unchanged_local_file() {
        let h1 = hash(b"v1");
        let h2 = hash(b"v2");
        let st = state(&[(
            "pi/p/s",
            KeyState {
                import_stamp: Some((5, 10)),
                export_hash: Some(Some(h1)),
            },
        )]);
        let a = reconcile(
            &st,
            &[local("pi/p/s", (5, 10))],
            &index(&[("pi/p/s", live(h2, 9, 1))]),
        );
        assert_eq!(
            a,
            vec![Action::WriteFile {
                key: "pi/p/s".into(),
                hash: h2
            }]
        );
    }

    #[test]
    fn local_deletion_is_tombstoned_when_dir_not_empty() {
        let hk = hash(b"k");
        let ho = hash(b"o");
        let st = state(&[
            (
                "pi/p/k",
                KeyState {
                    import_stamp: Some((5, 10)),
                    export_hash: Some(Some(hk)),
                },
            ),
            (
                "pi/p/o",
                KeyState {
                    import_stamp: Some((5, 10)),
                    export_hash: Some(Some(ho)),
                },
            ),
        ]);
        // k vanished; o still present ⇒ dir not empty
        let a = reconcile(
            &st,
            &[local("pi/p/o", (5, 10))],
            &index(&[("pi/p/k", live(hk, 5, 1)), ("pi/p/o", live(ho, 5, 1))]),
        );
        assert_eq!(
            a,
            vec![Action::Tombstone {
                key: "pi/p/k".into()
            }]
        );
    }

    #[test]
    fn empty_dir_never_tombstones() {
        let hk = hash(b"k");
        let st = state(&[(
            "pi/p/k",
            KeyState {
                import_stamp: Some((5, 10)),
                export_hash: Some(Some(hk)),
            },
        )]);
        // whole dir empty ⇒ suppress deletion propagation
        let a = reconcile(&st, &[], &index(&[("pi/p/k", live(hk, 5, 1))]));
        assert!(a.is_empty(), "empty dir must not wipe peers, got {a:?}");
    }

    #[test]
    fn remote_tombstone_deletes_local_file() {
        let a = reconcile(
            &SyncState::default(),
            &[local("pi/p/s", (5, 10))],
            &index(&[("pi/p/s", tombstone(20))]),
        );
        assert_eq!(
            a,
            vec![Action::DeleteLocal {
                key: "pi/p/s".into()
            }]
        );
    }

    #[test]
    fn write_after_deletion_is_a_recreate() {
        // file mtime newer than the tombstone ⇒ import, do not delete
        let a = reconcile(
            &SyncState::default(),
            &[local("pi/p/s", (30, 10))],
            &index(&[("pi/p/s", tombstone(20))]),
        );
        assert_eq!(
            a,
            vec![Action::Import {
                key: "pi/p/s".into(),
                stamp: (30, 10),
                winner: None,
            }]
        );
    }

    #[test]
    fn absent_and_tombstoned_is_a_noop() {
        let st = state(&[(
            "pi/p/s",
            KeyState {
                import_stamp: None,
                export_hash: Some(None),
            },
        )]);
        let a = reconcile(&st, &[], &index(&[("pi/p/s", tombstone(20))]));
        assert!(a.is_empty(), "already deleted everywhere, got {a:?}");
    }

    #[test]
    fn divergence_emits_merge() {
        let h = hash(b"winner");
        let st = state(&[(
            "pi/p/s",
            KeyState {
                import_stamp: Some((5, 10)),
                export_hash: Some(Some(h)),
            },
        )]);
        // settled file, but two distinct live hashes across authors ⇒ merge
        let a = reconcile(
            &st,
            &[local("pi/p/s", (5, 10))],
            &index(&[("pi/p/s", live(h, 5, 2))]),
        );
        assert_eq!(
            a,
            vec![Action::Merge {
                key: "pi/p/s".into()
            }]
        );
    }

    #[test]
    fn divergence_without_merge_allowed_emits_no_merge() {
        let h = hash(b"winner");
        let st = state(&[(
            "x/p/s",
            KeyState {
                import_stamp: Some((5, 10)),
                export_hash: Some(Some(h)),
            },
        )]);
        let entry = IndexEntry {
            merge_allowed: false,
            ..live(h, 5, 2)
        };
        let a = reconcile(&st, &[local("x/p/s", (5, 10))], &index(&[("x/p/s", entry)]));
        assert!(a.is_empty(), "non-append-only key was merged: {a:?}");
    }

    #[test]
    fn state_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("ssync-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.toml");

        let h = hash(b"blob");
        let mut st = SyncState::default();
        st.settle_import("pi/p/imported", (5, 10), h);
        st.settle_delete("pi/p/deleted");
        st.keys.insert("pi/p/untouched".into(), KeyState::default());

        st.save(&path).unwrap();
        let loaded = SyncState::load(&path);

        assert_eq!(
            loaded.keys.get("pi/p/imported").unwrap().import_stamp,
            Some((5, 10))
        );
        assert_eq!(
            loaded.keys.get("pi/p/imported").unwrap().export_hash,
            Some(Some(h))
        );
        assert_eq!(
            loaded.keys.get("pi/p/deleted").unwrap().export_hash,
            Some(None)
        );
        assert_eq!(loaded.keys.get("pi/p/untouched").unwrap().export_hash, None);

        // a corrupt/absent file must never kill the daemon: fresh state
        std::fs::write(&path, "not toml [").unwrap();
        assert!(SyncState::load(&path).keys.is_empty());
        assert!(SyncState::load(&dir.join("missing.toml")).keys.is_empty());
    }

    // settle_*: an unchanged snapshot must produce no actions (no echo),
    // while a real peer update still acts.

    #[test]
    fn settled_import_does_not_echo() {
        let h = hash(b"v1");
        let mut st = SyncState::default();
        st.settle_import("pi/p/s", (5, 10), h);
        let a = reconcile(
            &st,
            &[local("pi/p/s", (5, 10))],
            &index(&[("pi/p/s", live(h, 5, 1))]),
        );
        assert!(a.is_empty(), "self-import echoed: {a:?}");
    }

    #[test]
    fn settled_write_does_not_reimport_own_writeback() {
        let h = hash(b"remote");
        let mut st = SyncState::default();
        st.settle_write("pi/p/s", h, Some((9, 20)));
        let a = reconcile(
            &st,
            &[local("pi/p/s", (9, 20))],
            &index(&[("pi/p/s", live(h, 9, 1))]),
        );
        assert!(a.is_empty(), "own write-back echoed as import: {a:?}");
    }

    #[test]
    fn settled_write_still_accepts_newer_peer_version() {
        let h1 = hash(b"v1");
        let h2 = hash(b"v2");
        let mut st = SyncState::default();
        st.settle_write("pi/p/s", h1, Some((9, 20)));
        let a = reconcile(
            &st,
            &[local("pi/p/s", (9, 20))],
            &index(&[("pi/p/s", live(h2, 12, 1))]),
        );
        assert_eq!(
            a,
            vec![Action::WriteFile {
                key: "pi/p/s".into(),
                hash: h2
            }]
        );
    }

    #[test]
    fn settled_delete_with_tombstone_winner_is_a_noop() {
        // a LIVE winner after settle_delete would be a peer recreate —
        // rematerialising it is then correct.
        let mut st = SyncState::default();
        st.settle_delete("pi/p/s");
        let a = reconcile(&st, &[], &index(&[("pi/p/s", tombstone(20))]));
        assert!(a.is_empty(), "settled deletion echoed: {a:?}");
    }

    #[test]
    fn settled_write_then_vanished_file_is_a_local_redelete() {
        let h = hash(b"recreated");
        let ho = hash(b"other");
        let mut st = SyncState::default();
        st.settle_delete("pi/p/s");
        st.settle_write("pi/p/s", h, None);
        st.settle_import("pi/p/o", (5, 10), ho);
        let a = reconcile(
            &st,
            &[local("pi/p/o", (5, 10))],
            &index(&[("pi/p/s", live(h, 30, 1)), ("pi/p/o", live(ho, 5, 1))]),
        );
        assert_eq!(
            a,
            vec![Action::Tombstone {
                key: "pi/p/s".into()
            }]
        );
    }
}
