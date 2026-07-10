# Repository Guidelines

Repo-specific instructions. Personal conventions live in `~/AGENTS.md`. Design rationale
lives in `docs/DECISIONS.md` (§0–§12) — cite it by section, never restate it here.

## Project Overview

ssync syncs AI-agent session files (pi, omp; claude-code and codex newest-wins) between a
user's machines: leaderless p2p via iroh, age-encrypted at rest, no server to run. Rust
workspace (edition 2024), single binary + daemon.

## Architecture & Data Flow

Impure shell around pure decision cores:

- `ssync` (bin) is the composition root: config → age identity → iroh `Node` → adapters →
  `Engine::run`.
- `Engine::run` (ssync-core) is one `tokio::select!` loop. Event sources: notify fs
  watcher, iroh-docs LiveEvents (drained on a dedicated task, forwarded as a `DocSignal`
  — relevance ping or learned peer id — over an **unbounded** channel; bounded
  subscriber channels deadlock the iroh-docs actor), 15s rescan, 60s resync. Everything
  funnels into a 400ms-debounced tick.
- Each tick: `local_snapshot` + `index_snapshot` → pure `reconcile()` → `Vec<Action>`
  {Import, WriteFile, DeleteLocal, Tombstone, Merge} → execute → settle into `SyncState`
  (atomic temp+rename to `data_dir/state.toml`).
- Import path: file → age-encrypt (subprocess) → `Node::publish` (blob + index entry keyed
  `{agent}/{relative_path}`; the ONLY write path — temp-tag prevents the GC race).
- Export path: index entry → `get_blob` (miss → `fetch_blob` from peers, 30s bound;
  iroh-docs never retries a missed download, upstream iroh-docs#88) → decrypt → atomic
  temp+rename write-back.
- Conflicts: winner = newest entry, selected once behind the net seam (`IndexRecord`);
  append-only formats (pi/omp) get the deterministic lossless line-union merge, others
  newest-wins. The merge verdict is all-or-skip: a partial version set never merges.
- Per-key errors are logged (`eprintln!("ssync: …")`) and retried next tick; one bad key
  never kills the daemon.
- Pairing is two modes: ticket exchange (`ssync ticket`/`join`; the ticket embeds direct
  addresses) or shared-namespace mode (clan: `namespace_secret_path` derives the same
  namespace on every peer, `peers` node-ids resolve via iroh discovery — no ticket).
  `recipients` set = per-machine age identities with multi-recipient encryption; empty =
  one shared identity. Recipient-set or namespace changes re-publish/evict (issue #22).

### Hard rules (pointers into docs/DECISIONS.md — don't re-litigate)

- **Store session files as opaque bytes** (§2). Reading a single header field is fine;
  parsing entry lines is not.
- **Watch-and-import, never sync-in-place** (§10). Read from the agent's dir; write back
  atomically (temp + rename). Never symlink/bind it into the engine.
- **Encryption (age) is not optional and not deferrable** (§7).
- **Leaderless** (§4): no code path may assume a special/authority node. **User runs no
  server** (§6): iroh public defaults only (mDNS local discovery is still TODO; today
  LAN connectivity rides the ticket's embedded addresses).
- iroh, iroh-docs, iroh-blobs, and age move fast: when unsure of a current API, read the
  crate's actual docs/source — never code from memory.

## Key Directories

- `crates/ssync/` — binary: clap CLI (`init/daemon/ticket/join/status/conflicts/cleanup/`
  `keygen-node/keygen-namespace`) + daemon wiring, single `src/main.rs`.
- `crates/ssync-core/` — `Config`, `Engine`, `StatusReport`; pure decision cores live in
  their own submodules.
- `crates/ssync-net/` — iroh endpoint/router/docs/blobs/gossip setup, `Node`,
  `IndexRecord` (the only place winner selection happens), blob GC.
- `crates/ssync-crypto/` — `AgeIdentity`: PQ-hybrid keys via the `age`/`age-keygen` CLI;
  Rust `age`-crate backend disabled behind feature `rust-age` until it gains ML-KEM.
- `crates/ssync-adapters/` — `Adapter` trait + `adapter_for` factory. New agent = one
  `impl Adapter` + one `adapter_for` arm (`"pi" | "omp"` share `PiAdapter`; claude-code
  and codex have their own impls, merge gated off until append-only is verified).
- `nix/` — package, devshell, treefmt, checks (incl. two NixOS VM tests), NixOS/HM/clan
  modules, nixbot effects.
- `docs/` — DECISIONS.md, identity.md, pairing.md, setup.md, threat-model.md,
  pi-format-notes.md, claude-code-format-notes.md, codex-format-notes.md,
  m3-cross-network.md, vs-syncthing.md, logos/.
- Deferred work lives in the GitHub issue tracker, not a repo file; DECISIONS.md
  carries the rationale per item.
- `.tmp/` — gitignored scratch handed over by the user; **never commit or push it**.
  (Tests do not use it; they create dirs under the system temp dir.)

## Development Commands

direnv (`.envrc` = `use flake`) auto-loads the devshell: locked toolchain + `age` CLI
(needed by ssync-crypto tests) on PATH. Without direnv, prefix `nix develop -c`.

```bash
cargo clippy --workspace --all-targets -- -D warnings   # lint
cargo test --workspace                                  # tests
nix fmt                                                 # format (treefmt); verify: nix fmt -- --ci
nix build .#default                                     # package (→ ./result/bin/ssync; checkPhase runs tests)
nix flake check                                         # full CI gate (package, formatting, vm-sync, vm-module)
```

The nix build serializes tests (`--test-threads=1`; parallel in-process iroh nodes starve
each other under CI load) — mimic with `cargo test -- --test-threads=1` when two-node
tests flake locally. Never `nix shell nixpkgs#cargo …` — registry nixpkgs drifts from
flake.lock. CI is nixbot building the flake checks (plus the nixbot effects in
`nix/effects.nix`); there are no GitHub Actions.

## Code Conventions & Common Patterns

- **Errors:** `anyhow` everywhere (`Context` with file paths, `bail!`, `ensure!`); no
  `thiserror` — nothing matches on error variants. Daemon paths skip/reset malformed
  inputs instead of failing; per-key errors are logged and retried next tick.
- **Async:** tokio (full); one `select!` event loop; `futures_lite::StreamExt`. The
  unbounded event channels are a documented iroh-docs workaround, not a general
  preference. Blocking `std::fs` is fine in CLI, pure-core, and state code.
- **DI:** constructor injection only (`Engine::with_adapters(Vec<Box<dyn Adapter>>,
  AgeIdentity, Node)`); no globals or singletons. Test seams are named production methods
  (`set_resync_interval`, `spawn_with_gc`, `disable_auto_download`), not mocks.
- **Modules:** split by concept — a pure decision core gets its own submodule with its own
  unit tests; no `util`/`helpers` grab-bags.
- **Naming:** `cmd_*` CLI handlers, `settle_*` state transitions, `*_snapshot` reconcile
  inputs, `*_of` derived-view lookups.
- **Comments:** *why* not *what*; rustdoc summary first line. Rationale is a pointer, not
  prose: cite `DECISIONS §N` or the upstream issue for non-obvious invariants. If a diff
  adds more comment lines than code lines, cut comments; delete redundant comments when
  touching a file.
- **Secrets:** age identity and CLI-generated keys are written 0600 (`write_secret`), and
  loading refuses group/world-readable key files; `SecretFile` puts temp key material in
  `$XDG_RUNTIME_DIR` (tmpfs) and removes it on drop. Never commit keys (`*.key`,
  `age.key`, `node.key` gitignored).
- **State files** under `data_dir`: `state.toml` (SyncState), `status.toml` (mtime =
  daemon liveness), `node.key`, `namespace`/`ticket`/`remote-ticket`, `blobs/`, `docs/`.

### Versioning / releases

- Single source of truth: `[workspace.package] version` in root `Cargo.toml`;
  `nix/package.nix` reads it via `lib.importTOML` — never hardcode elsewhere. All crates
  and dep versions inherit from the workspace tables (`workspace = true`); the iroh stack
  is deliberately pinned (0.x iroh-* crates minor-pinned; `iroh` itself is 1.x, major-pinned).
- Pre-1.0: feature/breaking → MINOR bump; bugfix/internal → PATCH; MAJOR stays 0 until
  the user declares 1.0. Bump once per user-visible-behavior task; pure refactor/docs/CI
  need no bump. After bumping, run `cargo check` so `Cargo.lock` updates; commit the lockfile.
- Version bumps are manual; releases are not: the `release` push effect in
  `nix/effects.nix` runs after nixbot's build goes green on a main push and tags
  `v0.x.y` + creates the GitHub release if the workspace version has no tag yet.
  Never tag by hand.

### VCS

- Colocated jj + git; **use jj**. Remote `origin` = `ssh://git@github.com/fosskar/ssync.git`.
- **Do not commit or push unless the user explicitly says so.** Stage in the working copy
  and wait for the go-ahead.
- Commit messages: linux-kernel style, area prefixes (`core:`, `net:`, `nix:`, `cargo:`),
  no trailers, repo terminology.

## Important Files

- `crates/ssync/src/main.rs` — CLI + daemon wiring, entry point.
- `crates/ssync-core/src/reconcile.rs` — pure sync decision core; most invariants live here.
- `crates/ssync-core/src/lib.rs` — `Engine`, the event loop.
- `crates/ssync-core/src/config.rs` — `Config` (`$XDG_CONFIG_HOME/ssync/config.toml`, `~/`
  expanded per-machine).
- `crates/ssync-net/src/lib.rs` — `Node`, `publish`, `fetch_blob`, `IndexRecord`.
- `crates/ssync-adapters/src/lib.rs` — `Adapter` trait + `adapter_for` (extensibility point).
- `Cargo.toml` (root) — workspace version + all dep versions.
- `flake.nix` + `nix/` — the whole build/CI surface.

## Runtime/Tooling Preferences

- Toolchain from nixpkgs-unstable via the devshell (plain nixpkgs rust, no
  `rust-toolchain` file). Edition 2024.
- Format with `nix fmt` (treefmt: rustfmt, nixfmt, taplo, deadnix, statix) — not ad-hoc
  `cargo fmt`/nixfmt. A missing formatter/linter belongs in `nix/treefmt.nix`, not ad hoc.
- `age`/`age-keygen` with PQ support (`-pq`, age ≥1.3) required on PATH at runtime and in
  tests; the devshell and the nix package wrapper both provide it.
- Nix conventions: flake tracks nixpkgs-unstable; package/devshell live in `nix/` via
  `callPackage`, not repo root; don't scaffold speculative modules.

## Testing & QA

TDD is mandatory for new behavior:

1. Write a failing test first; confirm it fails for the expected reason.
2. Minimum code to green; refactor with clippy `-D warnings` and fmt clean.
3. Never delete or weaken a test to make it pass.
4. Prefer deterministic in-process tests; reproduce any bug with a failing test first.
   For sync, use in-process iroh nodes wired by direct addresses in one test process —
   no real networking, no relay.

Layout and patterns:

- Plain `cargo test` harness (`#[test]`/`#[tokio::test]`); zero third-party test deps.
  Scratch dirs are hand-rolled under `std::env::temp_dir()`, scoped by pid + tag.
- Unit tests live inline (`#[cfg(test)] mod tests`) next to the code; a `tests/` dir only
  where cross-crate or network integration is needed.
- `crates/ssync-core/tests/multi_node_sync.rs` — the main integration suite: two/three
  in-process nodes through the production `Engine` path. Await convergence by polling
  (`eventually()`, 500ms steps), never fixed sleeps; drive all mutation through
  `tick_once()`/`run()`.
- `crates/ssync-core/tests/event_flood.rs` — 1500-session deadlock regression.
- `crates/ssync-core/tests/recipient_rotation.rs` — issue #22 regressions: recipient-set
  change re-publishes every session; namespace rotation drops the stale replica.
- Failure injection via production hooks: `disable_auto_download()` (simulates missed
  downloads), bogus peer addrs, `set_resync_interval`.
- Pure-core changes: extend the inline unit tests (`reconcile`, divergence/merge,
  cleanup selection) first — they cover the invariants cheaply; two-node tests are for
  the wiring.
- `nix flake check` additionally runs two NixOS VM tests (`vm-sync` e2e over virtual LAN,
  `vm-module` service/hardening test).

## pi session format (caveat)

pi stores sessions at `~/.pi/agent/sessions/<encoded-cwd>/<ts>_<sessionId>.jsonl`, one
append-only JSONL file per session, header on line 1 (`version:3`, `id` uuidv7, `cwd`). pi
keys on the **absolute cwd**, so a synced project must live at the **same absolute path on
every machine** — a v1 requirement, not a bug. Derive `PiAdapter` identity from the
path alone; treat `<encoded-cwd>` as opaque (pi and omp encode differently). omp uses the
same layout at `~/.omp/agent/sessions` and adds a per-session artifact dir
(`<encoded-cwd>/<ts>_<sessionId>/`: subagent transcripts, `__advisor.jsonl`) whose files
are part of the session and sync with it (identity from the artifact dir name — DECISIONS
§9). pi appends in place (no temp+rename), so
imports must stay debounced. Full reference: `docs/pi-format-notes.md` (re-verify against
the installed pi version before relying on it).
