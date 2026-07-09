# ssync — TODO

Deferred work, captured so v1 stays small. See DECISIONS.md for rationale on each.
Open items are ranked highest expected user-facing value (reliability,
connectivity, UX) first.

## Open

1. [ ] **Manual cross-network (M3) e2e:** two nodes on different networks (e.g. one
       tethered), confirm sync works via the n0 relay and that the relay only ever
       sees ciphertext. The relay path is the one core promise never exercised in
       prod; the two-VM test covers the LAN/direct path only. Can't run in the CI
       sandbox.
2. [ ] **More agents** — investigated 2026-07 (issues #6–#9); adapters landed for the
       two file-backed agents, merge gated off until append-only is verified against
       a real session file:
       - [x] Claude Code (`~/.claude/projects/<encoded-cwd>/<uuid>.jsonl`) — adapter
             `claude-code`; newest-wins until verified (#6, docs/claude-code-format-notes.md)
       - [x] Codex (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, sqlite is only a
             metadata index) — adapter `codex`; newest-wins until verified (#7,
             docs/codex-format-notes.md)
       - [ ] OpenCode — sqlite-only since 2026-02, row-mutable; blocked on the
             DB-extraction adapter model (#8 → #20)
       - amp — declined: no local transcripts, threads live on ampcode.com (#9)
3. [ ] **mDNS local discovery** so LAN peers connect without a ticket that carries
       direct addresses (iroh `address-lookup-mdns`). Connectivity currently relies
       on the ticket's embedded addresses. Priority raised: DECISIONS §6, pairing.md,
       and AGENTS.md already promise mDNS as the LAN story — the code is behind the
       documented design.
4. [ ] **Surface config knobs** that are currently hardcoded: `discovery`
       (default vs lan-only), `relay` override, `[conflict] strategy`, and the
       peer re-sync interval (60s). Today it is n0 defaults + newest-wins only.
5. [ ] **Reconcile peer-management UX.** Both mechanisms are now live: namespace
       tickets (`ssync ticket` / `ssync join <ticket>`) do pairing, while the config
       `peers` node-id list feeds `sync_with_peers` resync via discovery. Decide the
       user-facing story (likely: tickets stay the pairing UX, `peers` becomes
       plumbing that `join` populates, plus `ssync peer list/remove` for hygiene)
       and align docs.
6. [ ] **Path-rewriting (Option B)** so projects at _different_ absolute paths across
       machines can sync: a user-configured path map that rewrites the encoded-cwd
       dir name AND the `cwd` header field on import/export. Knowingly crosses
       store-as-is, so opt-in only. (v1 requires matching absolute paths instead;
       see docs/pi-format-notes.md.)
7. [ ] **Selective sync:** per-project include/exclude.
8. [ ] **Drop the explicit `fetch_blob` workaround** once iroh-docs retries missed
       content downloads itself (upstream PR n0-computer/iroh-docs#88, still open;
       2026-07-01 update added per-(namespace, peer) debounce + per-blob backoff to
       address the maintainer's overhead concern — watching, no action until merged
       and the pin bumps). The engine's on-write peer fetch stays correct either
       way, but becomes dead weight.
9. [ ] Switch `ssync-crypto` back to the **in-process Rust `age` crate** (drop the
       `age` CLI runtime dependency) once that crate supports ML-KEM. The disabled
       backend is already in place behind the `rust-age` feature.
10. [ ] **Advisory soft-lease.** Optional synced "machine X is actively editing
        session Y" flag to warn before simultaneous edits. Not load-bearing
        consensus. (DECISIONS §4/§8)
11. [ ] **Search/preview** layer: optional, sandboxed, best-effort transcript parsing
        for `ssync search`. Must never affect storage/sync. (DECISIONS §2)
12. [ ] Document **self-hosted relay** setup for users who distrust n0 public infra.
        (DECISIONS §6)
13. [ ] **Agents that store sessions in a single database** (not ssync's storage —
        ssync uses iroh-docs + iroh-blobs): if a future target agent keeps all
        sessions in one SQLite/DB file instead of per-session files, its adapter
        needs a way to extract/identify individual sessions (e.g. row-level), since
        the current model assumes one file per session.

## Won't do

- **Report the subscriber-channel deadlock upstream.** Decided against (2026-07).
  For the record: iroh-docs awaits subscriber sends on bounded channels inside its
  actor; a subscriber that stops reading while awaiting a docs RPC wedges the whole
  store (repro: ssync's `event_flood` test before the drain-task fix; related closed
  issue n0-computer/iroh-docs#81 removed `blocking_send` but kept the awaited bounded
  send). ssync's drain-task + unbounded-channel workaround is therefore permanent,
  not transitional.

## Done

- [x] **Merge-based resolution for append-only formats.** Done for pi: divergent
      versions converge to a lossless line union — common prefix kept in order, each
      fork's remaining lines appended, deduped across (not within) versions. No entry
      parsing (store-as-is holds); ordering is prefix-based rather than per-entry
      timestamp, so fork suffixes are concatenated, not interleaved. (DECISIONS §8)
- [x] **Deletion by any participant, not just the author.** The winning entry per
      key is resolved _including_ tombstones (`include_empty`), so the newest
      tombstone from any author deletes the session everywhere. Resurrection is guarded
      by comparing the tombstone timestamp against the local file's mtime on import
      (a write after the deletion is a genuine recreate and imports normally).
      Known limit: deleting the _last_ session in the dir does not propagate (the
      empty-dir wipe guard intentionally suppresses it).
- [x] Adapters: pi (`~/.pi/agent/`) and omp (`~/.omp/agent/`, reuses `PiAdapter` —
      same on-disk layout as pi).
- [x] **Per-machine age keypairs** with multi-recipient encryption (replaces shared
      identity as the recommended mode; shared identity still supported via
      `recipients = []`), enabling per-device revocation. (DECISIONS §7)
- [x] **Post-quantum hybrid encryption** by default (ML-KEM-768 + X25519 via
      `age-keygen -pq`). Done by shelling out to the `age` CLI.
- [x] Determine pi's session format. DONE (docs/pi-format-notes.md): per-session append-only JSONL,
      `~/.pi/agent/sessions/<encoded-cwd>/<ts>_<id>.jsonl`, id = uuidv7 in filename+header,
      append-only confirmed, cwd-keyed (drives the matching-path requirement).
