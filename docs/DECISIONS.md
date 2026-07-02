# ssync — Decisions

This document records **what** we decided and **why**. Read this first.

ssync is a tool that keeps **coding-agent session files** continuously in sync across
a person's own machines, peer-to-peer, with no central server they have to run. The
goal is: do work with an agent on one machine, walk away, and resume the _same_ session
on another machine — automatically, with no manual commands.

---

## 0. The problem, precisely

Coding agents (Claude Code, pi, omp, Codex, OpenCode, …) store each conversation as
**session files** on local disk. These files live only on the machine where the agent
ran. If you start a session on your desktop and later want to continue it on your
laptop, the session isn't there — it's stranded on the desktop.

Existing tools don't solve this:

- **Live orchestrators** (cctl, ccmanager, codeg, the whole `awesome-agent-orchestrators`
  list) manage _running_ sessions on one machine. They are dashboards, not sync.
- **cctl specifically** is Claude-Code-only by design — it depends on Claude Code's hook
  system to receive lifecycle events. It cannot see pi/omp/Codex sessions.
- **Claude Sync** (the Medium/CLI project) syncs `~/.claude` via encrypted cloud storage
  — but it's single-agent and routed through cloud storage, not p2p.
- **Syncthing** could sync the folders, but it's format-blind: it can't filter secrets,
  can't be agent-aware, and its generic `.sync-conflict-…` handling is actively wrong for
  append-only transcripts.

So there's a real gap: **an agent-agnostic, p2p, automatic synchronizer for session
files.** That's ssync.

---

## 1. Scope: collect-and-sync, NOT live-resume-across-machines, NOT orchestration

**Decision:** ssync synchronizes session _files at rest_. It does **not** stream a live
running session between machines, and it is **not** an orchestrator/dashboard.

**Why:** Live cross-machine _resume of a running process_ (machine A's live agent process
continued on machine B) is an unsolved, hard problem — the official Claude Code feature
request (issue #47926) lays out exactly why (process state, tool permissions, cwd, paths
are all machine-local). We deliberately do not attempt it. We sync the _files_; the user
re-launches the agent's own `--resume` on the other machine, which reads the now-present
session file. That gives 95% of the value with none of the swamp.

---

## 2. Store session files AS-IS — no normalization, no parsing for storage

**Decision:** ssync stores session files as opaque blobs, byte-for-byte. It defines **no**
canonical session schema and writes **no** per-agent format parsers for the purpose of
storage or sync.

**Why:** The tempting-but-fatal version of this project is "define a universal session
format and write adapters that convert each agent into it." That creates a permanent
maintenance treadmill, because these agents change their formats constantly (omp ships
releases almost daily; Claude Code's transcript shape shifts between versions). By storing
blobs untouched, ssync **cannot break when an agent changes format** — it never looked
inside. An "adapter" is therefore just a declaration of _where_ an agent's session files
live, not code that understands them.

Parsing, if ever needed (for search/preview), is an **optional, swappable, best-effort**
layer that can degrade without affecting storage or sync. Not in v1.

---

## 3. Identity vs. provenance — the key design that makes "the same session" coherent

**Decision:** A session's identity is `(agent, project, session_id)` — all machine-
independent. `session_id` is the UUID the agent itself stamps on the session. The
originating **machine is metadata (provenance), not part of identity.**

**Why:** An earlier draft keyed sessions by `(machine, agent, project, session)`. That was
wrong: it would make "the same session" on desktop and laptop look like two different
things, fragmenting into per-machine silos. By keying on the agent's own session UUID,
one session has one identity everywhere. The machine field becomes a provenance/audit
column ("this version was last written on `desktop`"), used only as a tiebreaker when the
_same_ session id is genuinely written on two machines independently.

**Verified pi constraint (June 2026, from pi-mono source):** pi stores each session under a
directory named after the **absolute cwd** and also embeds the absolute `cwd` in the session
header. It keys purely on absolute path, not git/repo identity. This means **the same
project must live at the same absolute path on every machine** for sessions to line up — a
hard v1 requirement (see docs/pi-format-notes.md). Rewriting paths to relax this is possible but
violates store-as-is, so it is deferred to TODO.

---

## 4. Topology: leaderless p2p, every node an equal peer

**Decision:** ssync is **leaderless**. Every machine runs the same daemon and is an equal
peer. There is no designated hub, server, or authority node.

**Why we considered a hub and rejected it:** A hub (one always-on box as the authority)
would make one thing easy — a clean single-writer lease via a central SQLite row. But it
costs: asymmetric client/server code, a hard dependency (hub down → nothing syncs at all),
and it's the one centralized thing in an otherwise federated, self-owned setup. The only
benefit it bought — bulletproof mutual exclusion — guards a scenario (two machines writing
the _same_ session in the _same_ moment) that essentially never happens in real use, where
the workflow is _sequential handoff_ (work on A, stop, later resume on B), not simultaneous
writing.

**The offline-handoff reality:** Pure p2p has one inherent limitation — two peers can only
sync when both are online at once. If desktop does work and powers off _before_ laptop ever
comes on, the laptop can't get it directly. This is true of Syncthing too; it's physics,
not a bug. The standard fix is an always-on peer that bridges the time gap. We do NOT
mandate one (see §6) — but a user who wants reliable offline handoff can simply leave any
one machine on, and it acts as that bridge _as an equal peer_, not as a privileged hub.

---

## 5. Sync mechanism: iroh + iroh-docs (CRDT index) + iroh-blobs (file transfer)

**Decision:** Build on **iroh** (p2p QUIC transport, dial-by-public-key, NAT traversal),
using **iroh-docs** as the synchronized session index and **iroh-blobs** to transfer the
session file contents.

**Why iroh:** It's a _library_, not a network overlay. Alternatives the user floated —
Yggdrasil, Mycelium, NetBird — are network layers (you'd stand up a mesh, then still build
the app on top). iroh is exactly one level higher: the binary embeds it and gets dial-by-
key (peers identified by cryptographic public key, stable across network changes),
automatic NAT hole-punching (~90% success), and end-to-end encrypted QUIC transport, with
no overlay to manage.

**Why iroh-docs:** Because we chose leaderless (§4), there is no authority to answer
"which copy of session X is newest." A CRDT answers that _without a coordinator_: every
entry carries version metadata (author + timestamp + content hash), and every node
converges to the same state on its own via range-based set reconciliation. iroh-docs is
precisely an "eventually-consistent key-value store of iroh-blobs blobs" — key =
`(agent, project, session_id)`, value = the session file's BLAKE3 hash + metadata. It
pairs natively with iroh-blobs, which moves the actual file content (content-addressed,
deduplicated, resumable).

**The mapping:**

- `iroh` (core, v1.0) → transport, discovery, NAT traversal.
- `iroh-blobs` (first-class) → content-addressed transfer of the actual session files.
- `iroh-docs` (see status note below) → the synced index + conflict metadata.

**Status note (verified June 2026):** iroh core is at **v1.0.0-rc.1** (stable, 8.7k
stars, 563 dependents). iroh-blobs is first-class and healthy. **iroh-docs is maintained
and tracks core releases (latest v0.100.0, same day as the 1.0 RC) but is a pre-1.0,
minor crate (56 stars).** We accept this: its model fits exactly and it's the lowest-code
path. The risk is occasional breaking changes across its 0.x releases and a small
community. If iroh-docs is ever abandoned, the architecture is unchanged — the CRDT-index
box could be refilled with automerge or yrs over iroh transport+blobs. **We are choosing
iroh-docs deliberately, with eyes open.**

---

## 6. No relay the user has to run; NAT bootstrap via iroh's free public infrastructure

**Decision:** The user runs **no server of any kind** — no relay, no VPS, no hub.

- **On a LAN** (both machines on the same network, e.g. at home): peers discover each
  other via **mDNS local discovery**. Genuinely zero infrastructure, no internet involved.
- **Across the internet** (laptop on mobile data, desktop at home): discovery and NAT-
  traversal bootstrap go through **iroh's built-in public discovery + relay infrastructure
  (run for free by n0/number0)**. The user hosts and configures nothing.

**Why a bootstrap helper is unavoidable across the internet (and why this is still p2p):**
Two machines behind home routers / mobile NATs _cannot_ discover each other or hole-punch
without some third party both can reach during the initial handshake. This is how the
internet works, not an iroh limitation. The relay is **only** a bootstrap coordinator: once
a direct connection is punched, ~95% of data flows peer-to-peer directly; the relay carries
traffic only as a fallback when hole-punching fails. Critically, **all traffic is end-to-
end encrypted — the relay only ever sees ciphertext, never session contents.** Using n0's
free public relays is not "running a server"; it's like using public DNS. So this remains
genuinely p2p; the user simply isn't required to operate the introducer.

**Optional, documented, never required:** a power user who distrusts n0's public infra can
point ssync at their own relay. This is a config option in docs, not a default and not a
dependency.

---

## 7. Security: age encryption at rest, on by default, never deferred

**Decision:** Session files are encrypted at rest with **age** (https://github.com/FiloSottile/age).
Encryption is **on by default in v1.** Security is never a "v2 feature."

**Why:** Agent session transcripts routinely contain API keys, tokens, file contents, and
other secrets. Storing them in plaintext — even on the user's own central store or in
transit over a relay — is unacceptable. age is the right choice: modern, FOSS, widely
trusted, already idiomatic in this user's ecosystem (sops-nix etc.).

**Post-quantum:** ssync uses **post-quantum hybrid keys by default** (ML-KEM-768 + X25519),
native in `age` >= 1.3 via `age-keygen -pq` (recipients `age1pq1…`). Hybrid means data stays
safe if *either* primitive breaks — if ML-KEM (young) has a bug, X25519 still protects it; if
a quantum computer breaks X25519, ML-KEM still protects it. PQ-only is deliberately **not**
used: age offers no such mode, and it would discard the classical fallback. Because the Rust
`age` crate is X25519-only, `ssync-crypto` shells out to the `age` CLI to get native hybrid;
the in-process crate backend is kept disabled (feature `rust-age`) for when it gains ML-KEM.

**Key management:** One **shared age identity distributed across the user's machines**
(each machine holds the same age private key, distributed once via the user's existing
secret-management, e.g. sops-nix). This is the simplest correct choice: every machine can
decrypt what any other machine encrypted. (Per-machine keypairs with multi-recipient
encryption are a possible future refinement, not v1.)

---

## 8. Conflict handling in v1: detect, keep both, newest-wins as safety net

**Decision:** v1 does **not** implement a write-lease. When iroh-docs surfaces a genuine
concurrent divergence (the same session id written on two machines that hadn't seen each
other), ssync **keeps both blob versions** (they're content-addressed, so nothing is lost)
and applies **newest-timestamp-wins** as the active version, while retaining the loser and
surfacing a warning.

**Why no lease in v1:** A lease only matters for _truly simultaneous_ writes to the _same_
session — which the sequential-handoff workflow essentially never produces. Building lease
coordination now is solving a problem the user doesn't have. iroh-docs detects divergence
rather than silently corrupting, and store-as-is means both versions survive. So the v1
policy is safe: rare, detectable, recoverable.

**Merge (implemented for pi):** Append-only transcripts can be losslessly merged by
unioning their lines — strictly better than newest-wins. This is only safe if the format
is genuinely append-only; some agents _compact/rewrite_ their session files, where naive
line-merge would corrupt. pi was verified append-only (docs/pi-format-notes.md), so the
engine merges divergent pi sessions: common prefix kept in chronological order, each
fork's remaining lines appended, deduped across (not within) versions. The merge is
pure line-set arithmetic — no entry parsing, store-as-is holds — and content-derived
ordering makes every peer compute the identical result, so all nodes converge. A future
non-append-only adapter falls back to detect + keep-both + newest-wins.

**Deletion (any participant):** the winning index entry per key is resolved *including*
tombstones, so the newest tombstone from any author deletes the session on every peer —
not just the author's. Resurrection is guarded by comparing the tombstone timestamp
against the local file's mtime on import; a write after the deletion is a genuine
recreate. Safety valve: a transiently empty session dir never propagates deletions (the
empty-dir wipe guard), at the cost that deleting the very last session does not sync.

---

## 9. Agent scope: pi + omp, more via plug-in adapters

**Decision:** ship the **pi** adapter (`badlogic/pi-mono`) and reuse it for **omp**
(oh-my-pi), a pi fork with the same on-disk layout. A node syncs one or more agents side
by side: config lists `[[agents]]` (name + session dir), the engine holds a
`Vec<Box<dyn Adapter>>` sharing one namespace, partitioned by the `{agent}/` key prefix.
Adding an agent is one `impl Adapter` plus one `adapter_for` match arm.

**Why:** each adapter is just a "where do sessions live + how to identify a file +
is-append-only" declaration (DECISIONS §2), so agents that share pi's layout (pi, omp)
reuse `PiAdapter`, and an agent with a different layout (Claude Code, Codex, …) drops in
as its own boxed `impl Adapter` in the same engine — genuinely plug-in, no engine change.
pi/omp format is known (verified from source): per-session append-only JSONL at
`<root>/<encoded-cwd>/<ts>_<id>.jsonl`, session id = uuidv7 in the filename, append-only
confirmed (so merge is safe — §8). The cwd-encoding differs between pi and omp but identity
never decodes it (the dir name is opaque). Remaining agents stay in the repo TODO. See
docs/pi-format-notes.md.

**Superseded:** an earlier draft scoped v1 to **pi only** to keep the pipeline and test
surface small; omp was trivial to add (identical layout) and proved the multi-agent seam,
so it landed too.

---

## 10. Architecture: watch-and-import, never sync-in-place

**Decision:** ssync **watches** the agent's real session directory (read) and imports
changes into its own managed store. It does **not** make the agent's live session dir _be_
ssync's store (no symlink/bind of pi's actual directory into the sync engine).

**Why:** Safety. The agent must never be at risk of ssync corrupting, locking, or
partially-writing its live session files. Watch-and-import keeps a clean boundary: ssync
reads the agent's files and owns its own copy; when a remote update arrives, ssync writes
the session file back into the agent's dir atomically. The agent and the sync engine never
fight over the same bytes.

---

## 11. Language & packaging

**Decision:** **Rust**, single binary. Packaged in tiers, each usable without the next:

1. **Plain binary** — `cargo install` / downloadable static binary + a config file. No Nix,
   no anything. The baseline everyone can use.
2. **Nix flake** — exposes `packages.default`, a **home-manager module** (runs the user
   daemon), and a **plain NixOS module**. Covers all Nix users. **No clan dependency.**
3. **clan service** — a thin _optional_ wrapper over the NixOS module, in its own flake
   output, for clan users only. If you don't use clan you never import it.

**Why Rust:** iroh-docs is Rust-native (its API is first-class only in Rust; FFI bindings
don't cover docs well). Rust also gives the single static binary and clean cross-compile we
want, and fits the user's stack (well-supported in nixpkgs).

**Why the packaging ladder:** clan is the _user's_ convenience, not anyone else's
dependency. The core must be a plain binary + config file that works with zero Nix. Nix and
clan are strictly opt-in layers on top.

---

## 12. systemd hardening

**Decision:** the NixOS module runs the daemon under a strict systemd sandbox
(`ProtectSystem=strict`, `ProtectHome=read-only`, `NoNewPrivileges`, empty
`CapabilityBoundingSet`, `MemoryDenyWriteExecute`, `SystemCallFilter=@system-service
~@privileged ~@resources`, `ProtectProc=invisible`/`ProcSubset=pid`, `PrivateTmp`,
`PrivateDevices`, the `ProtectKernel*`/`ProtectClock`/`ProtectControlGroups` set,
`RestrictNamespaces`/`RestrictRealtime`/`RestrictSUIDSGID`/`LockPersonality`/`RemoveIPC`).

**Allow-list (what the sandbox must keep open, and why):**

- `ReadWritePaths = [ sessionDir ]` — watch-and-import needs to write imported sessions
  back atomically. A `systemd.tmpfiles` rule pre-creates `sessionDir` (owner = the run
  user, `0700`) so the bind succeeds on first boot before the agent has created it.
- `StateDirectory=ssync` — the only other writable path (`/var/lib/ssync`: node key, blobs,
  docs, index, status).
- `RestrictAddressFamilies = AF_INET AF_INET6 AF_UNIX AF_NETLINK` — iroh needs QUIC/UDP
  over IPv4/IPv6 and `AF_NETLINK` to enumerate local interfaces for address discovery.
- secrets (`/run/secrets/…`) and the Nix store stay readable via `ProtectSystem=strict`
  (read-only, not hidden), so the age key / namespace secret / node key are reachable.

**Verified:** the daemon starts under the full profile (`vm-module` check), and
`age-keygen -pq` plus PQ-hybrid encrypt/decrypt round-trip cleanly under the same
`MemoryDenyWriteExecute` + syscall filter — the `age` CLI is a Go binary spawned per
encrypt/decrypt (DECISIONS §7), so that combination was the real risk and it holds.

---

## Summary table

| Axis                | Decision                                                                                     |
| ------------------- | -------------------------------------------------------------------------------------------- |
| Name                | **ssync**                                                                                    |
| What it does        | Continuous p2p sync of agent **session files** across one's own machines                     |
| What it is NOT      | Not live-resume of running processes; not an orchestrator/dashboard                          |
| Storage             | Session files stored **as-is** (opaque blobs); no schema, no format parsers                  |
| Identity            | `(agent, project, session_id)`; machine is provenance metadata only                          |
| Topology            | **Leaderless p2p**, all peers equal; no hub/server                                           |
| Sync stack          | **iroh** (transport) + **iroh-docs** (CRDT index) + **iroh-blobs** (file transfer)           |
| Infra the user runs | **None.** LAN via mDNS; internet via iroh's free public relay (ciphertext only)              |
| Encryption          | **age, on by default**, PQ-hybrid if a mature plugin exists; shared identity across machines |
| Conflicts (v1)      | No lease; detect + keep-both; **lossless line-union merge** for pi (newest-wins fallback)    |
| agents              | **pi + omp** (share pi layout); more via boxed plug-in `Adapter`s, one per agent             |
| Boundary            | **Watch-and-import**, never sync-in-place                                                    |
| Language            | **Rust**, single binary                                                                      |
| Packaging           | plain binary → Nix flake (HM + NixOS modules) → optional clan wrapper                        |
