# ssync vs. Syncthing

Syncthing is the obvious "why not just use Syncthing?" question for ssync. It is a mature,
trusted, p2p file synchronizer, and on paper it could move session files between your
machines. This document explains why it is *not sufficient* for ssync's problem, and where
its security model differs.

The short version: **Syncthing is a general-purpose file mover; ssync is a session-aware
tool.** The gap is not raw transport — it is everything that has to happen *around* the
bytes: filtering secrets, understanding session identity, handling append-only transcripts
correctly, and encrypting at rest with keys your peers cannot silently read.

---

## Summary table

| Axis | Syncthing | ssync |
| --- | --- | --- |
| Purpose | General folder sync | Agent **session-file** sync |
| Awareness of content | Format-blind | Session-aware (identity = `(agent, project, session_id)`) |
| Conflict handling | Generic `.sync-conflict-…` copies | Detect + keep-both; lossless append-only **merge** (pi), newest-wins fallback |
| Secret filtering | None | Adapter declares *where* sessions live; only those are synced |
| Encryption at rest | Optional, and untrusted-only ("untrusted device" mode) | **age, on by default, always** |
| Post-quantum | No | ML-KEM-768 + X25519 hybrid by default |
| Peers can read your data | Yes (trusted devices hold plaintext) | No (peers hold ciphertext unless they also hold the age key) |
| Transport | TLS over relays/direct | iroh QUIC, end-to-end encrypted, dial-by-public-key |
| Infra you run | None (public relays/discovery) | None (iroh public relays/discovery; LAN mDNS planned) |
| Agent integration | None | Watch-and-import boundary; writes back atomically |

---

## Why Syncthing is not enough

### 1. It is format-blind

Syncthing syncs *folders*. It has no concept of a "session", so it cannot:

- know that a file is an agent transcript versus junk in the same directory;
- select *only* the session files an agent produces, and leave the rest alone;
- key a session by its stable identity so "the same session" on desktop and laptop is
  treated as one thing rather than two paths that happen to collide.

ssync's core design is exactly this awareness: a session's identity is
`(agent, project, session_id)`, machine-independent, derived from the agent's own session
UUID. An adapter declares *where* an agent stores sessions and a couple of flags; ssync
syncs precisely those files and nothing else. (See `DECISIONS.md` §2–§3.)

### 2. Its conflict handling is actively wrong for transcripts

When two sides diverge, Syncthing writes `.sync-conflict-<date>-<device>.<ext>` copies and
leaves you to reconcile by hand. For append-only JSONL transcripts this is the *wrong*
behavior: it litters the agent's session directory with files the agent doesn't understand,
and it discards the fact that two divergent transcripts of the same session can often be
**losslessly merged** by unioning their lines in timestamp order.

ssync instead:

- detects genuine divergence via a CRDT index (iroh-docs), so nothing is silently
  corrupted;
- keeps both versions (they are content-addressed — nothing is lost);
- applies newest-timestamp-wins as the active copy as a safety net;
- and, per-adapter for formats confirmed append-only-safe (pi is), does a real lossless
  line-union **merge** — strictly better than newest-wins. (See `DECISIONS.md` §8.)

Syncthing can never do the merge, because it never looks inside the file.

### 3. It syncs the agent's live directory in place

Point Syncthing at `~/.pi/agent/sessions` and it *is* your store — it writes into the
directory the agent is actively appending to, and races the agent over the same bytes.

ssync uses a **watch-and-import** boundary (`DECISIONS.md` §10): it *reads* the agent's
directory and owns a separate copy; when a remote update arrives it writes back
**atomically** (temp + rename). The agent and the sync engine never fight over the same
bytes.

### 4. It cannot filter secrets

Session transcripts routinely contain API keys, tokens, and file contents. Syncthing gives
you folder-level include/exclude globs at best — it has no model of "this is a session, sync
it; that is a credential cache, don't." ssync only ever syncs the files an adapter declares
as sessions, and encrypts them (below).

---

## Is Syncthing secure enough?

Syncthing's transport security is genuinely good: device-to-device TLS, devices identified
by cryptographic ID, no account/cloud. That part is not the problem. The problems are
**at-rest** and **trust granularity**.

### At rest, your peers hold plaintext

In a normal Syncthing setup every trusted device stores the synced files **in plaintext on
disk**. Syncthing does have an "untrusted (encrypted) device" mode, but it is opt-in,
awkward, and designed for the *untrusted-relay-node* case — not "encrypt everything
everywhere by default." For files that routinely contain secrets, plaintext-at-rest on
every device is the wrong default.

ssync encrypts every session file with **age before it is handed to the transport**
(`threat-model.md`). Consequences:

- Any relay that carries traffic sees **ciphertext only** — like Syncthing, but ssync also
  means...
- ...a peer that holds a pairing ticket can write to the *index* but **cannot decrypt
  sessions** — decryption needs the separately-distributed shared age key. Write-access and
  read-access are decoupled. Syncthing has no equivalent split: a trusted device reads
  everything.
- Encryption is **on by default and never deferrable** — it is not a mode you remember to
  enable.

### Post-quantum

Syncthing offers no post-quantum protection. ssync uses **ML-KEM-768 + X25519 hybrid keys
by default** (native in `age` >= 1.3). Data stays confidential if *either* primitive is
broken — a future quantum computer against X25519, or a bug in the young ML-KEM. (See
`threat-model.md`.)

### Honest limits (the same physics apply to both)

ssync is not magic, and some of Syncthing's limits are shared because they are inherent to
p2p:

- **Offline handoff.** Two peers can only sync when both are online at once. If a machine
  does work and powers off before another comes on, they can't sync directly. This is true
  of Syncthing too — it's physics. The fix in both cases is to leave one machine on as an
  always-available peer; ssync keeps that peer an *equal* peer, not a privileged hub
  (`DECISIONS.md` §4).
- **Trusted machines.** ssync assumes one user's own set of machines. Every machine in the
  recipient set can decrypt everything — by design; per-machine keys (`recipients`) enable
  revocation, the shared-key mode does not. A compromised machine that is still in the set
  can read everything (`threat-model.md`). Syncthing's trusted devices are equivalent.
- **Metadata.** ssync hides session *contents*, not their existence, size, or relative paths
  from peers in your own namespace. Syncthing similarly exposes file names/sizes to trusted
  devices.

---

## When Syncthing is the right tool

If you want to sync arbitrary folders across many devices and people, with per-folder
sharing and a mature GUI, Syncthing is excellent and ssync is not a replacement for it.

ssync is narrow on purpose: it does **one** thing — keep coding-agent session files in sync
across your own machines, session-aware, secret-filtered, encrypted-by-default, with correct
handling of append-only transcripts — and it does that better than a general-purpose folder
syncer can. Use Syncthing for folders; use ssync for sessions.
