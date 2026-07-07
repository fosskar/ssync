# AGENTS.md — ssync

Repo-specific instructions and caveats. General personal conventions live in `~/AGENTS.md`;
this file only records what is specific to *this* repo. Read `docs/DECISIONS.md` for the
design rationale.

## Comments

Comment sparingly. Explain *why*, not *what* — never restate what the code plainly
does. No comment should be longer than the code it describes. Prefer a good name
over a comment. Keep doc comments to one line unless a non-obvious caveat needs
more. Delete redundant comments when you touch a file. If a diff adds more comment
lines than code lines, cut comments until it doesn't — one compressed sentence per
item, no multi-clause doc paragraphs, no comments narrating test steps.

## Hard rules

- **Store session files as opaque bytes.** No canonical schema, no transcript parsers for
  storage/sync. An adapter only declares *where* sessions live + a few flags. Reading a
  single header field for identity is fine; parsing entry lines is not.
- **Watch-and-import, never sync-in-place.** Never symlink/bind pi's real session dir into
  the engine. Read from it; write back atomically (temp + rename).
- **Encryption (age) is not optional and not deferrable.**
- **Leaderless.** No code path may assume a special/authority node.
- **User runs no server.** iroh public defaults + mDNS only; no required relay/VPS/hub.
- When unsure of an external crate's current API (iroh, iroh-docs, iroh-blobs, age): read
  its actual current docs/source. These crates move fast; do not code from memory.

## Never commit

- `.tmp/` — scratch/planning handed over by the user. Gitignored. Never track or push it.

## Versioning / releases

- Single source of truth: `[workspace.package] version` in the root `Cargo.toml`.
  `nix/package.nix` reads it via `lib.importTOML`; never hardcode a version elsewhere.
- Version format: semver `MAJOR.MINOR.PATCH` (e.g. `0.1.2`).
- Pre-1.0 bump rules: breaking change or new feature → bump **MINOR**; bugfix or internal
  change → bump **PATCH**. **MAJOR** stays 0 until the user declares 1.0.
- When you finish a task that changes user-visible behavior, bump the version in the same
  change set (once per task, not per commit). Pure refactors/docs/CI need no bump.
- After editing the version, run a cargo command (e.g. `cargo check`) so `Cargo.lock`
  picks up the new workspace version, and include the lockfile in the commit.
- Releases are manual: user tags `v0.x.y` and pushes. No CI bumping, no release tooling.

## VCS

- Colocated jj + git repo; **use jj**. Remote `origin` = `ssh://git@github.com/fosskar/ssync.git`.
- **Do not commit or push unless the user explicitly says so.** Stage work in the working
  copy and wait for the go-ahead.
- Commit messages: linux-kernel style, no trailers, repo terminology.

## TDD (test-driven development)

Follow TDD for new behavior in this repo:

1. Write a failing test first that captures the desired behavior (unit test next to the
   code, or an integration test under the crate's `tests/`).
2. Run it and confirm it fails for the expected reason (red).
3. Write the minimum code to make it pass (green).
4. Refactor with the test green; keep `cargo clippy -- -D warnings` and `cargo fmt` clean.
5. Never delete or weaken a test to make it pass; fix the code or the test's premise.
6. Prefer deterministic, in-process tests; reproduce any bug with a failing test first. For
   sync, use two in-memory iroh nodes in one test process rather than real networking.

## Build / verify

direnv (`.envrc` = `use flake`) auto-loads the devShell in this repo: the
locked toolchain (cargo, rustc, clippy, rustfmt) plus `age` (needed by
ssync-crypto's tests) are already on PATH — run `cargo …` directly:

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Without direnv (CI, bare shells), prefix with `nix develop -c`. Format with
`nix fmt` (treefmt, nix/treefmt.nix: rustfmt, nixfmt, taplo, deadnix, statix)
— not `cargo fmt` or ad-hoc nixfmt. A useful formatter/linter missing from
treefmt? Add it to nix/treefmt.nix rather than running it ad hoc. Never
`nix shell nixpkgs#cargo …`: that resolves nixpkgs from the registry, not
flake.lock, so the toolchain drifts from the devShell.
Package build: `nix build .#default` (→ `./result/bin/ssync`).

## Crates

- `ssync` — binary: CLI + daemon wiring.
- `ssync-core` — sync engine (reconcile pass), index model, conflict logic.
- `ssync-crypto` — age encrypt/decrypt, identity handling.
- `ssync-net` — iroh endpoint/router/docs/blobs setup, peering.
- `ssync-adapters` — `Adapter` trait + `PiAdapter`.

## Nix conventions

- Flake tracks **nixpkgs-unstable** (always unstable).
- Package and devShell live in `nix/` (`package.nix`, `devshell.nix`), wired via
  `callPackage` from `flake.nix` — not in the repo root.
- Add home-manager / NixOS / clan modules only once the daemon exposes real config options.
  Don't scaffold empty modules (no speculative code).

## pi session format (caveat)

pi stores sessions at `~/.pi/agent/sessions/<encoded-cwd>/<ts>_<sessionId>.jsonl`, one
append-only JSONL file per session, header on line 1 (`version:3`, `id` uuidv7, `cwd`). pi
keys on the **absolute cwd**, so a synced project must live at the **same absolute path on
every machine** — this is a v1 requirement, not a bug. Derive `PiAdapter` identity from the
path/filename alone; do not parse the transcript. Full reference:
`docs/pi-format-notes.md` (re-verify against the installed pi version before relying on it).

omp (oh-my-pi, a pi fork) uses the same layout at `~/.omp/agent/sessions`; only the
cwd-encoding of the project dir name differs, and the adapter treats that as opaque. pi
and omp both use `PiAdapter` via `ssync_adapters::adapter_for` (one arm mapping both names
to it). The engine holds a `Vec<Box<dyn Adapter>>`, so a new agent is one `impl Adapter`
plus one `adapter_for` arm: same layout → reuse `PiAdapter`; different layout → new impl.
