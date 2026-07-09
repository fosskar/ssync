# Codex (OpenAI Codex CLI) session format notes

Investigated 2026-07-09 from source (openai/codex, `codex-rs/thread-store/
src/local/mod.rs`, `codex-rs/state/migrations/0001_threads.sql`). NOT yet
verified against a real session file — the local codex install has an empty
threads table and no sessions dir. Re-verify before relying on details.

## Layout: hybrid, JSONL is canonical

- Rollout JSONL files: `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<thread-uuid>.jsonl`
  (date-partitioned, one file per session). `<ts>` = `YYYY-MM-DDTHH-MM-SS`
  (seconds precision, no millis/zone suffix).
- SQLite (`~/.codex/state_5.sqlite`, `threads` table) is only a queryable
  metadata index over the JSONL files (id → rollout_path, cwd, title, ...).
  On startup a backfill scans rollout files and repairs the index — so a
  session file synced in by ssync gets indexed on the next codex start.
  ssync never touches the sqlite.
- Archived sessions move to `~/.codex/archived_sessions/YYYY/MM/DD/` — a MOVE,
  not an append. Outside the watched root, ssync sees it as a local deletion
  (tombstone). Acceptable, but worth knowing.

## Content

- Line 1: `SessionMeta` with thread uuid `id`, absolute `cwd`, model provider.
  The cwd lives ONLY in content (path encodes the date, not the project), so
  the matching-absolute-path requirement applies for resume-on-other-machine;
  sync itself is path-independent.

## Append-only status: source-supported, unverified on real data

Source opens rollouts with `OpenOptions::new().append(true).create(true)`;
compaction appends replacement_history items rather than rewriting. That is
stronger evidence than docs prose — but still not a diff of a real file across
compaction, and the store code notes metadata updates happen via sqlite, not
file rewrites. `CodexAdapter::append_only()` returns **false** (newest-wins)
until a real session file confirms strict append. Flip criteria in issue #7.

## Adapter mapping

- agent: `codex`, root: `~/.codex/sessions`
- session_id: trailing uuid of `rollout-<ts>-<uuid>` stem
- project_id: the `YYYY/MM/DD` date partition (codex has no project dir; the
  engine keys on agent + relative path, so this is grouping metadata only)
- session filter: `rollout-*.jsonl`; created_at: filename `<ts>`

## Non-adapters from the same investigation

- **OpenCode**: sqlite-only since 2026-02 (`~/.local/share/opencode/opencode.db`,
  drizzle `session`/`message`/`part` tables, row-mutable). No per-session file
  exists; a sync adapter needs the row-level extraction model (issue #20).
- **amp**: no local transcripts at all — threads live server-side on
  ampcode.com (local files are CLI prompt history + UI state). Nothing to
  sync; adapter declined (issue #9).
