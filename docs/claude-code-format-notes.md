# Claude Code session format notes

Investigated 2026-07-09 from primary sources (anthropics/claude-code docs:
`docs/en/sessions.md`, `docs/en/agent-sdk/session-storage.md`,
`docs/en/how-claude-code-works.md`), then partially verified the same day
against real session files written by Claude Code 2.1.205 (unauthenticated
`claude -p` runs still write full transcripts). Compaction/rewind behavior
remains doc-claimed only — re-verify before flipping the merge gate.

## Layout

- Root: `~/.claude/projects/` (configurable via `CLAUDE_CONFIG_DIR`).
- One JSONL file per session: `<encoded-cwd>/<session-uuid>.jsonl`.
- `<encoded-cwd>` = absolute cwd with every non-alphanumeric char replaced by
  `-` (e.g. `/home/user/my-project` → `-home-user-my-project`). Lossier than
  pi's encoding (collisions possible); treat as opaque, never decode.
- Filename = bare session uuid (v4). No timestamp prefix, so `created_at`
  cannot come from the filename (unlike pi/codex).

## Content

- Line 1 in practice (2.1.205) is NOT always the documented system/init
  event — a real file started with `queue-operation` records. Identity must
  stay path-derived; never key on line-1 shape. The user event carries the
  absolute `cwd` verbatim, duplicated with the encoded path — the
  matching-absolute-path requirement applies exactly as for pi
  (docs/pi-format-notes.md).
- Following lines: one JSON object per message/tool event. Upstream explicitly
  warns the entry format is internal and changes between releases — store as
  opaque bytes (DECISIONS §2) is mandatory, not just preferred.
- The project dir can contain non-session entries (observed: a `memory/`
  subdir); the `*.jsonl` filter handles it, but don't assume the dir holds
  only session files.

## Append-only status: partially verified, merge still gated

Verified on a real file (2.1.205, 2026-07-09): resume (`claude -p --resume
<id>`) appended new records to the same file; the prior bytes stayed a
byte-identical prefix (`cmp -n` check). Layout and identity claims above were
confirmed against the same file.

Still unverified: `/compact` and rewind/fork file-level behavior — docs say
compaction appends a summary entry (no truncation) and the SDK store contract
is append/load, but neither was exercised (needs an authed session long
enough to compact). Those are precisely the risky operations for the
line-union merge, so `ClaudeCodeAdapter::append_only()` stays **false**
(newest-wins) until a real session file is diffed across a compaction and a
rewind. Flip criteria in issue #6.

## Adapter mapping

- agent: `claude-code`, root: `~/.claude/projects`
- session_id: filename stem; project_id: `<encoded-cwd>` dir
- session filter: `*.jsonl`; created_at: none; title: none known
