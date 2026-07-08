# Repository Guidelines

Repo-specific instructions. Personal conventions live in `~/AGENTS.md`; rationale lives in
`docs/DECISIONS.md` (12 ADRs — read it before design changes).

## Project Overview

ssync syncs AI-agent session files (pi, omp) between a user's machines: leaderless p2p via
iroh, age-encrypted at rest, no server to run. Rust workspace, single binary + daemon.

## Architecture & Data Flow

Layered workspace with a pure functional core:

- `ssync` (bin) wires config → age identity → iroh `Node` → adapters → `Engine.run`.
- `Engine` (ssync-core) runs one `tokio::select!` loop: notify fs events + iroh-docs
  LiveEvents (drained on a dedicated task, relayed over an **unbounded** channel — bounded
  subscriber channels deadlocked the iroh-docs actor in 0.2.0) + 15s rescan + 60s resync,
  all funneled into a 400ms-debounced tick.
- Each tick: `local_snapshot` + `index_snapshot` → pure `reconcile()`
  (`crates/ssync-core/src/reconcile.rs` — no IO, no clock, deterministic) →
  `Vec<Action>` {Import, WriteFile, DeleteLocal, Tombstone, Merge} → execute →
  settle into `SyncState` (atomic temp+rename to `data_dir/state.toml`).
- Import path: file → age-encrypt (subprocess) → `Node::publish` (blob + iroh-docs index
  entry keyed `{agent}/{relative_path}`; the ONLY write path — temp-tag prevents GC race).
- Export path: remote entry → `get_blob` (miss → `fetch_blob` from peers, 30s bound;
  works around iroh-docs#88) → decrypt → atomic temp+rename write-back.
- Conflicts: winner = newest entry; append-only formats (pi/omp) get deterministic
  lossless line-union merge (`merge_lines`), others newest-wins; tombstones guarded
  against empty-dir wipes and post-deletion recreates.
- Per-key errors are logged (`eprintln!("ssync: …")`) and retried next tick; the daemon
  never dies on one bad key.

### Hard rules (from `docs/DECISIONS.md`)

- **Store session files as opaque bytes.** No canonical schema, no transcript parsers for
  storage/sync. Reading a single header field for identity is fine; parsing entry lines is not.
- **Watch-and-import, never sync-in-place.** Never symlink/bind the agent's real session
  dir into the engine. Read from it; write back atomically (temp + rename).
- **Encryption (age) is not optional and not deferrable.**
- **Leaderless.** No code path may assume a special/authority node.
- **User runs no server.** iroh public defaults + mDNS only; no required relay/VPS/hub.
- When unsure of an external crate's current API (iroh, iroh-docs, iroh-blobs, age): read
  its actual current docs/source. These crates move fast; do not code from memory.

## Key Directories

- `crates/ssync/` — binary: clap CLI (`init/daemon/ticket/join/status/conflicts/keygen-*`)
  + daemon wiring, single `src/main.rs`.
- `crates/ssync-core/` — `Config`, `Engine`, `StatusReport`; `src/reconcile.rs` holds the
  pure decision core and `SyncState`.
- `crates/ssync-net/` — iroh endpoint/router/docs/blobs/gossip setup, `Node`,
  `IndexRecord` (the only place winner selection happens), blob GC.
- `crates/ssync-crypto/` — `AgeIdentity`: PQ-hybrid keys via `age`/`age-keygen` CLI
  (≥1.3); Rust `age`-crate backend disabled behind feature `rust-age` until it gains ML-KEM.
- `crates/ssync-adapters/` — `Adapter` trait + `PiAdapter` + `adapter_for` factory. New
  agent = one `impl Adapter` + one `adapter_for` arm (`"pi" | "omp"` share `PiAdapter`).
- `nix/` — package, devshell, treefmt, checks (incl. two NixOS VM tests), NixOS/HM/clan modules.
- `docs/` — DECISIONS.md, pi-format-notes.md, identity.md, setup.md, pairing.md, threat-model.md.
- `.tmp/` — scratch handed over by the user; gitignored, **never commit or push it**.

## Development Commands

direnv (`.envrc` = `use flake`) auto-loads the devshell: locked toolchain + `age` CLI
(needed by ssync-crypto tests) are on PATH. Without direnv, prefix `nix develop -c`.

```bash
cargo clippy --workspace --all-targets -- -D warnings   # lint
cargo test --workspace                                  # tests
nix fmt                                                 # format (treefmt); verify: nix fmt -- --ci
nix build .#default                                     # package (→ ./result/bin/ssync; checkPhase runs tests)
nix flake check                                         # full CI-equivalent gate (incl. VM tests)
```

Nix builds run tests serialized (`--test-threads=1`); mimic with
`cargo test -- --test-threads=1` when in-process two-node tests flake.
Never `nix shell nixpkgs#cargo …` — registry nixpkgs drifts from flake.lock.

## Code Conventions & Common Patterns

- **Errors:** `anyhow` everywhere (`Context` with file paths, `bail!`, `ensure!`); no
  `thiserror`. Skip/reset malformed inputs instead of failing the daemon.
- **Async:** tokio (full); one `select!` event loop; `mpsc::unbounded_channel` for event
  relay (deliberate — see deadlock note above); `futures_lite::StreamExt`; sync `std::fs`
  fine in CLI/state code.
- **DI:** constructor injection only (`Engine::with_adapters(Vec<Box<dyn Adapter>>, AgeIdentity, Node)`);
  no globals/singletons. Test seams are explicit methods (`set_resync_interval`,
  `spawn_with_gc`, `disable_auto_download`), not mocks.
- **Naming:** `cmd_*` CLI handlers, `settle_*` state transitions, `*_snapshot` inputs,
  `*_of`/`*_of_key` lookups, `Dto` suffix for serde on-disk forms. Flat crates: one
  `lib.rs`, at most one submodule.
- **Comments:** sparse, *why* not *what*, one-line doc comments; cite `DECISIONS §N` or
  upstream issues for non-obvious invariants. If a diff adds more comment lines than code
  lines, cut comments. Delete redundant comments when touching a file.
- **Secrets:** key files written 0600 via `write_secret`; `SecretFile` for temp keys in
  `$XDG_RUNTIME_DIR`. Never commit keys (`*.key`, `age.key` gitignored).
- **State files** under `data_dir`: `state.toml` (SyncState), `status.toml` (mtime =
  daemon liveness), `node.key`, `namespace`/`ticket`/`remote-ticket`, `blobs/`, `docs/`.

### Versioning / releases

- Single source of truth: `[workspace.package] version` in root `Cargo.toml`;
  `nix/package.nix` reads it via `lib.importTOML` — never hardcode elsewhere.
- Pre-1.0: feature/breaking → MINOR bump; bugfix/internal → PATCH; MAJOR stays 0 until
  the user declares 1.0. Bump once per user-visible-behavior task; pure refactor/docs/CI
  need no bump. After bumping, run `cargo check` so `Cargo.lock` updates; commit the lockfile.
- Releases manual: user tags `v0.x.y` and pushes. No release tooling.

### VCS

- Colocated jj + git; **use jj**. Remote `origin` = `ssh://git@github.com/fosskar/ssync.git`.
- **Do not commit or push unless the user explicitly says so.** Stage in the working copy
  and wait for the go-ahead.
- Commit messages: linux-kernel style, area prefixes (`core:`, `net:`, `nix:`, `cargo:`),
  no trailers, repo terminology.

## Important Files

- `crates/ssync/src/main.rs` — CLI + daemon wiring, entry point.
- `crates/ssync-core/src/reconcile.rs` — pure sync decision core; most invariants live here.
- `crates/ssync-core/src/lib.rs` — `Config` (`$XDG_CONFIG_HOME/ssync/config.toml`),
  `Engine`, `merge_lines`.
- `crates/ssync-net/src/lib.rs` — `Node`, `publish`, `fetch_blob`, `IndexRecord`.
- `crates/ssync-adapters/src/lib.rs` — `Adapter` trait + `adapter_for` (extensibility point).
- `Cargo.toml` (root) — workspace version + all dep versions (`workspace = true` inheritance).
- `flake.nix` + `nix/` — the whole build/CI surface (no GitHub Actions; CI = nixbot/buildbot
  building flake checks).

## Runtime/Tooling Preferences

- Toolchain from nixpkgs-unstable via the devshell; no `rust-toolchain` file. Edition 2024.
- Format with `nix fmt` (treefmt: rustfmt, nixfmt, taplo, deadnix, statix) — not ad-hoc
  `cargo fmt`/nixfmt. A missing formatter/linter belongs in `nix/treefmt.nix`, not ad hoc.
- `age`/`age-keygen` ≥1.3 required on PATH at runtime and in tests (PQ-hybrid keys); the
  devshell and the nix package wrapper both provide it.
- Nix conventions: flake tracks nixpkgs-unstable; package/devshell live in `nix/` via
  `callPackage`, not repo root; don't scaffold speculative modules.

## Testing & QA

TDD is mandatory for new behavior:

1. Write a failing test first (unit test next to the code, or integration test under the
   crate's `tests/`); confirm it fails for the expected reason.
2. Minimum code to green; refactor with clippy `-D warnings` and fmt clean.
3. Never delete or weaken a test to make it pass.
4. Prefer deterministic in-process tests; reproduce any bug with a failing test first.
   For sync, use two in-memory iroh nodes in one test process — no real networking.

Layout and patterns:

- Plain `cargo test` harness, zero third-party test deps (no tempfile/nextest/proptest).
- `crates/ssync-core/tests/two_node_sync.rs` — the main integration suite: two in-process
  iroh nodes through the production `Engine` path; helpers `scratch()`, `spawn_node()`,
  `pi_engine()`, `eventually()` (poll 500ms, no fixed sleeps).
- `crates/ssync-core/tests/event_flood.rs` — 1500-session deadlock regression
  (multi_thread flavor).
- Failure injection via production hooks: `disable_auto_download()`
  (`DownloadPolicy::NothingExcept([])` simulates missed downloads), bogus peer addrs,
  `set_resync_interval`.
- Pure `reconcile()` tests in `reconcile.rs` cover import/tombstone/recreate/echo
  invariants — extend these first for sync-logic changes.
- Temp dirs: hand-rolled `std::env::temp_dir().join(format!("ssync-<tag>-{}", pid))`.
- `nix flake check` additionally runs two NixOS VM tests (`vm-sync` e2e over virtual LAN,
  `vm-module` service test).

## pi session format (caveat)

pi stores sessions at `~/.pi/agent/sessions/<encoded-cwd>/<ts>_<sessionId>.jsonl`, one
append-only JSONL file per session, header on line 1 (`version:3`, `id` uuidv7, `cwd`). pi
keys on the **absolute cwd**, so a synced project must live at the **same absolute path on
every machine** — a v1 requirement, not a bug. Derive `PiAdapter` identity from the
path/filename alone; do not parse the transcript. omp uses the same layout at
`~/.omp/agent/sessions`; only the cwd-encoding differs and the adapter treats it as
opaque. Full reference: `docs/pi-format-notes.md` (re-verify against the installed pi
version before relying on it).
