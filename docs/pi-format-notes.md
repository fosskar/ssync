# pi session format notes

Reference for the `PiAdapter`, from reading `badlogic/pi-mono` source and inspecting a real
session file. Re-verify against the installed pi version before relying on this.

## Location

```
~/.pi/agent/sessions/<encoded-cwd>/<timestamp>_<sessionId>.jsonl
```

- Config dir defaults to `~/.pi`; `getSessionsDir() = getAgentDir() + "/sessions"`.
- The omp fork uses `~/.omp/agent/` — out of scope for v1.

## Per-project subdirectory = encoded absolute cwd

pi computes the subdir from the absolute cwd:

```js
encodeCwd(cwd) = `--${cwd.replace(/^[/\\]/, "").replace(/[/\\:]/g, "-")}--`
```

e.g. `/home/simon/Projects/nixfiles` → `--home-simon-Projects-nixfiles--`.

**pi keys on the absolute cwd, not on git/repo identity.** This drives the hard v1
requirement that a project must live at the **same absolute path on every machine**
(DECISIONS §3). ssync writes blobs back byte-for-byte; it does not rewrite the header.

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
  `timestamp`/`id`). v1 still ships newest-wins (DECISIONS §8); merge stays a TODO.

## Write pattern

- Appends to the live file (`appendFile`), not temp+rename. The watcher MUST debounce so it
  never imports a mid-append file. Re-import on each settle is fine (content is
  content-addressed, so it is cheap and idempotent).

## Adapter implications

- `PiAdapter` derives identity from path/filename alone (both machine-independent):
  - `session_id` = uuid after the last `_` in the filename stem
  - `project_id` = the `<encoded-cwd>` parent directory name
  - `relative_path` = path relative to the sessions root
- Reading the single header line (to cross-check `id`/`cwd`) is allowed metadata access, not
  transcript parsing. Header cross-validation is deferred to M1.
- `append_only = true` for pi.
- `is_session_file` = files with the `.jsonl` extension.
