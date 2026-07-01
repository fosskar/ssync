# ssync — TODO

Deferred work, captured so v1 stays small. See DECISIONS.md for rationale on each.

## Conflict handling

- [ ] **Merge-based resolution for append-only formats.** Union timestamped lines from
      divergent versions instead of newest-wins. Per-adapter, gated on `append_only = true`
      _verified_ for that agent. **pi is already verified append-only** (PLAN section 1), so
      pi merge is unblocked whenever you choose to implement it — union entry lines, keep the
      single header, order by per-entry timestamp/id. (DECISIONS §8)
- [ ] **Advisory soft-lease.** Optional synced "machine X is actively editing session Y"
      flag to warn before simultaneous edits. Not load-bearing consensus. (DECISIONS §4/§8)

## More agents (each = one Adapter impl + append-only determination)

- [ ] Claude Code (`~/.claude/projects/**`)
- [ ] omp (`~/.omp/agent/`)
- [ ] Codex
- [ ] OpenCode
- [ ] Gemini CLI

## Security

- [ ] **Per-machine age keypairs** with multi-recipient encryption (replaces shared
      identity), enabling per-device revocation. (DECISIONS §7)
- [ ] Confirm/adopt a mature **post-quantum hybrid age plugin** as default when available.

## Infrastructure (all optional, never required)

- [ ] Document **self-hosted relay** setup for users who distrust n0 public infra.
      (DECISIONS §6)

## Features

- [ ] **Search/preview** layer: optional, sandboxed, best-effort transcript parsing for
      `ssync search`. Must never affect storage/sync. (DECISIONS §2)
- [ ] **Selective sync:** per-project include/exclude.
- [ ] **SQLite-backed agents:** row-level extraction if any agent stores sessions in one DB
      rather than per-session files.

## Identity / paths

- [ ] **Path-rewriting (Option B)** so projects at _different_ absolute paths across machines
      can sync: a user-configured path map that rewrites the encoded-cwd dir name AND the
      `cwd` header field on import/export. Knowingly crosses store-as-is, so opt-in only.
      (PLAN section 3.3; v1 requires matching absolute paths instead.)

## Investigations

- [x] Determine pi's session format. DONE (PLAN section 1): per-session append-only JSONL,
      `~/.pi/agent/sessions/<encoded-cwd>/<ts>_<id>.jsonl`, id = uuidv7 in filename+header,
      append-only confirmed, cwd-keyed (drives the matching-path requirement).
