# Claude Code session format notes

Investigated 2026-07-09 from primary sources (anthropics/claude-code docs:
`docs/en/sessions.md`, `docs/en/agent-sdk/session-storage.md`,
`docs/en/how-claude-code-works.md`) plus corroborating third-party format
writeups. NOT yet verified against a real session file — Claude Code is not
installed on the investigating machine. Re-verify before relying on details.

## Layout

- Root: `~/.claude/projects/` (configurable via `CLAUDE_CONFIG_DIR`).
- One JSONL file per session: `<encoded-cwd>/<session-uuid>.jsonl`.
- `<encoded-cwd>` = absolute cwd with every non-alphanumeric char replaced by
  `-` (e.g. `/home/user/my-project` → `-home-user-my-project`). Lossier than
  pi's encoding (collisions possible); treat as opaque, never decode.
- Filename = bare session uuid (v4). No timestamp prefix, so `created_at`
  cannot come from the filename (unlike pi/codex).

## Content

- Line 1: system init event (`type: "system"`, `subtype: "init"`) carrying
  `session_id`, absolute `cwd`, model, tools. The cwd is duplicated in path
  (encoded) and content (verbatim) — the matching-absolute-path requirement
  applies exactly as for pi (docs/pi-format-notes.md).
- Following lines: one JSON object per message/tool event. Upstream explicitly
  warns the entry format is internal and changes between releases — store as
  opaque bytes (DECISIONS §2) is mandatory, not just preferred.

## Append-only status: claimed, unverified

Docs describe the file as append-only: session progress appends lines,
`/compact` appends a summary entry (no truncation), resume/rename append. The
SDK session-store contract is append/load ordered entries.

However: no real session file has been inspected, and "rewind/fork" features
are documented without their file-level behavior being specified. A wrong
append-only classification scrambles content on conflict (line-union merge),
so `ClaudeCodeAdapter::append_only()` returns **false** (newest-wins) until
someone diffs a real session file across a compaction/rewind cycle and
confirms strict append. Flip criteria in issue #6.

## Adapter mapping

- agent: `claude-code`, root: `~/.claude/projects`
- session_id: filename stem; project_id: `<encoded-cwd>` dir
- session filter: `*.jsonl`; created_at: none; title: none known
