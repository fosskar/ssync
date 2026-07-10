# Identity

How ssync decides that two files on different machines are "the same session",
and the hard requirement that follows.

## Index key

Each session is keyed in the synced index by:

```
{agent}/{relative_path}
```

- `agent` — e.g. `pi`.
- `relative_path` — the session file's path relative to the agent's session root,
  i.e. `<encoded-cwd>/<timestamp>_<sessionId>.jsonl` for pi.

`relative_path` is machine-independent *iff* the project lives at the same
absolute path on every machine (see below). It is used as the key because it is
both stable across machines and carries the write-back location, so the exporter
can reconstruct exactly where the file belongs on any peer.

The `PiAdapter` derives all of this from the path and filename alone — it never
parses the session transcript (see `../docs/pi-format-notes.md` and DECISIONS §2).

## The same-absolute-path requirement (v1)

pi stores each session under a directory named after the **absolute working
directory** (`encodeCwd(cwd)`) and also embeds the absolute `cwd` in the session
header. pi keys purely on that absolute path, not on git/repo identity.

Consequence: the same repository checked out at `/home/alice/code/foo` on one
machine and `/home/alice/projects/foo` on another is, to pi, two different
sessions in two different directories. There is no way around this without
rewriting pi's own data.

**v1 requires that a synced project live at the same absolute path on every
machine.** For a single user controlling all their machines this is easy — always
place projects at, e.g., `~/projects/<repo>` everywhere. ssync writes session
files back byte-for-byte and never rewrites the header, keeping the store-as-is
rule intact.

### The username is part of the path

Because the encoded cwd includes the home directory, a project under `$HOME`
embeds the **username** in the key: `/home/alice/projects/foo` and
`/home/bob/projects/foo` are different sessions. So for home-based projects the
**OS username must be the same on every machine**. The NixOS module's `user`
option is not a cross-user bridge — it only selects which user the daemon runs as;
set the *same* username everywhere. Usernames may differ only for projects placed
at a user-independent absolute path (e.g. `/srv/foo`), where no `$HOME` appears in
the cwd.

## Provenance vs identity

The originating machine is **metadata (provenance)**, not part of identity. A
session has one identity everywhere; the machine that last wrote a given version
is recorded (as the iroh-docs author) only to break ties when the same session is
written independently on two machines — a conflict (see DECISIONS §3, §8).

## Deferred: path rewriting

Supporting projects at *different* absolute paths across machines would require a
user-configured path map that rewrites both the encoded-cwd directory name and the
`cwd` header field on import/export. That knowingly crosses the store-as-is rule,
so it is opt-in and deferred (issue #13).
