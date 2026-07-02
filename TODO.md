# ssync — TODO

Deferred work, captured so v1 stays small. See DECISIONS.md for rationale on each.

## Conflict handling

- [x] **Merge-based resolution for append-only formats.** Done for pi: divergent
      versions converge to a lossless line union — common prefix kept in order, each
      fork's remaining lines appended, deduped across (not within) versions. No entry
      parsing (store-as-is holds); ordering is prefix-based rather than per-entry
      timestamp, so fork suffixes are concatenated, not interleaved. (DECISIONS §8)
- [ ] **Advisory soft-lease.** Optional synced "machine X is actively editing session Y"
      flag to warn before simultaneous edits. Not load-bearing consensus. (DECISIONS §4/§8)

## Deletion

- [ ] **Deletion by any participant, not just the author.** Today deleting a session only
      removes this node's own author entry (`index_delete` tombstones one author), so a
      session deleted on a machine other than the one that created it survives (its author's
      live entry remains and re-syncs). Make any participating machine able to delete a shared
      session for everyone — e.g. delete/tombstone all author entries for the key, or a
      separate per-key deletion marker that overrides live entries and is guarded against
      resurrection.

## More agents (each = one Adapter impl + append-only determination)

- [ ] Claude Code (`~/.claude/projects/**`)
- [ ] omp (`~/.omp/agent/`)
- [ ] Codex
- [ ] OpenCode
- [ ] Gemini CLI

## Security

- [ ] **Per-machine age keypairs** with multi-recipient encryption (replaces shared
      identity), enabling per-device revocation. (DECISIONS §7)
- [x] **Post-quantum hybrid encryption** by default (ML-KEM-768 + X25519 via
      `age-keygen -pq`). Done by shelling out to the `age` CLI.
- [ ] Switch `ssync-crypto` back to the **in-process Rust `age` crate** (drop the
      `age` CLI runtime dependency) once that crate supports ML-KEM. The disabled
      backend is already in place behind the `rust-age` feature.

## Infrastructure (all optional, never required)

- [ ] Document **self-hosted relay** setup for users who distrust n0 public infra.
      (DECISIONS §6)

## Features

- [ ] **Search/preview** layer: optional, sandboxed, best-effort transcript parsing for
      `ssync search`. Must never affect storage/sync. (DECISIONS §2)
- [ ] **Selective sync:** per-project include/exclude.
- [ ] **Agents that store sessions in a single database** (not ssync's storage —
      ssync uses iroh-docs + iroh-blobs): if a future target agent keeps all
      sessions in one SQLite/DB file instead of per-session files, its adapter
      needs a way to extract/identify individual sessions (e.g. row-level), since
      the current model assumes one file per session.

## Identity / paths

- [ ] **Path-rewriting (Option B)** so projects at _different_ absolute paths across machines
      can sync: a user-configured path map that rewrites the encoded-cwd dir name AND the
      `cwd` header field on import/export. Knowingly crosses store-as-is, so opt-in only.
      (v1 requires matching absolute paths instead; see docs/pi-format-notes.md.)

## CLI / config

- [ ] **Reconcile peer-management UX.** Current model is namespace tickets
      (`ssync ticket` / `ssync join <ticket>`); the original plan envisioned
      `ssync peer add/list/remove` over a config `peers` list. Pick one and align
      docs.
- [ ] **Surface config knobs** that are currently hardcoded: `discovery`
      (default vs lan-only), `relay` override, `[conflict] strategy`. Today it is
      n0 defaults + newest-wins only.

## Discovery

- [ ] **mDNS local discovery** so LAN peers connect without a ticket that carries
      direct addresses (iroh `address-lookup-mdns`). Connectivity currently relies
      on the ticket's embedded addresses.

## Verification

- [ ] **Manual cross-network (M3) e2e:** two nodes on different networks (e.g. one
      tethered), confirm sync works via the n0 relay and that the relay only ever
      sees ciphertext. Can't run in the CI sandbox; the two-VM test covers the
      LAN/direct path only.

## Investigations

- [x] Determine pi's session format. DONE (docs/pi-format-notes.md): per-session append-only JSONL,
      `~/.pi/agent/sessions/<encoded-cwd>/<ts>_<id>.jsonl`, id = uuidv7 in filename+header,
      append-only confirmed, cwd-keyed (drives the matching-path requirement).
