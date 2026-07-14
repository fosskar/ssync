# pi session format notes

Reference for the `PiAdapter`, from reading `badlogic/pi-mono` source and inspecting a real
session file. Re-verify against the installed pi version before relying on this.

## Location

```
~/.pi/agent/sessions/<encoded-cwd>/<timestamp>_<sessionId>.jsonl
```

- Config dir defaults to `~/.pi`; `getSessionsDir() = getAgentDir() + "/sessions"`.
- The omp fork uses `~/.omp/agent/` — same layout, synced through the same adapter
  (`"pi" | "omp"` share `PiAdapter`).

## Per-project subdirectory = encoded absolute cwd

pi computes the subdir from the absolute cwd:

```js
encodeCwd(cwd) = `--${cwd.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--`
```

e.g. `/home/simon/Projects/nixfiles` → `--home-simon-Projects-nixfiles--`.

**pi keys on the absolute cwd, not on git/repo identity.** By default a project must
live at the **same absolute path on every machine** (DECISIONS §3); the opt-in
`[[path_map]]` bridges differing paths by rewriting exactly the header `cwd` and the
encoded dir name (§3 amendment, map #42). Outside a mapped prefix, ssync writes blobs
back byte-for-byte and never touches the header.

**omp header caveat:** omp main session files start with a `{"type":"title",…}` record
on line 1; the `type:session` header carrying `cwd` sits on **line 2**. pi has it on
line 1. Anything reading "the header" must scan the first lines for the
`type:session` record, not assume line 1.

## File shape

- **One JSONL file per session.** Not a database.
- **First line is a header**, e.g. (real, from pi 0.80.2):

  ```json
  {"type":"session","version":3,"id":"019e539d-f6ab-71ac-be20-d3ae2b23ea4a","timestamp":"2026-05-23T06:55:21.771Z","cwd":"/home/simon/Projects/nixfiles"}
  ```

  Optional `parentSession` field may be present.
- Then one JSON object per line per entry (`message`, `compaction`, `branch_summary`,
  `model_change`, `custom`, `label`, `session_info`, …).

## Session id

- `id` is a **uuidv7**, present in BOTH the filename (after the last `_`) and the header
  line. Machine-independent. This is `session_id`.

## Append-only: YES

- `JsonlSessionStorage.appendEntry()` does `appendFile(path, JSON.stringify(entry) + "\n")`.
- There is **no truncating rewrite** of an existing session file. "Compaction" is itself
  just another appended entry of `type:"compaction"` (a summary marker), not an in-place
  rewrite.
- Therefore merge IS safe for pi (union lines, keep the header once, order by per-entry
  `timestamp`/`id`) — shipped as the lossless line-union merge (DECISIONS §8), live for
  pi and omp.

## Write pattern

- Appends to the live file (`appendFile`), not temp+rename. The watcher MUST debounce so it
  never imports a mid-append file. Re-import on each settle is fine (content is
  content-addressed, so it is cheap and idempotent).

## Adapter implications

- `PiAdapter` derives identity from the path alone (machine-independent), from the
  **second path component** — the file stem for a main session file, the artifact dir
  name for a nested artifact file; both are shaped `<ts>_<sessionId>`:
  - `session_id` = uuid after the last `_` in that component
  - `project_id` = the `<encoded-cwd>` first component
  - `relative_path` = path relative to the sessions root
- Reading the single header line (to cross-check `id`/`cwd`) is allowed metadata access, not
  transcript parsing. Header cross-validation is deferred to M1.
- `append_only = true` for pi.
- `is_session_file` = files with the `.jsonl` extension, at any depth under the root.
- omp keeps a per-session artifact dir `<encoded-cwd>/<ts>_<sessionId>/` next to the main
  file (subagent transcripts, `__advisor.jsonl`). These are part of the session
  (DECISIONS §9): they sync, inherit the session's `created_at` from the dir name, and
  read their title from the sibling main file. Artifact files are pi-format JSONL
  transcripts appended as their subagents run and are treated as append-only like main
  sessions, so the line-union merge (DECISIONS §8, `append_only = true`) covers them —
  not yet verified against omp's storage source; re-verify if a merge ever surfaces
  lines omp did not write. Plain pi has no artifact dirs.
