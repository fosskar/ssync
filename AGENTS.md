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
  watcher, `Node::signals` wake pings (iroh-docs LiveEvents are drained behind the net
  seam on a dedicated task — bounded subscriber channels deadlock the iroh-docs actor —
  and peers learned on the live stream are recorded by the node itself), 15s rescan,
  60s resync. FS and
  index events funnel into a 400ms-debounced tick; the rescan ticks directly, the
  resync only re-initiates peer sync.
- Each tick: `local_snapshot` + `index_snapshot` → pure `reconcile()` → `Vec<Action>`
  {Import, WriteFile, DeleteLocal, Tombstone, Merge} → execute → settle into `SyncState`
  (atomic temp+rename to `data_dir/state.toml`).
- `SessionFilesystem` (ssync-core `session_filesystem.rs`) owns adapters, bounded discovery,
  wire-key ↔ local-path translation, path-map resolution, freeze verdicts, and contained
  read/write/delete. Engine never touches adapters or filesystem policy directly; tests cover
  the interface without iroh.
- Import path: file → age-encrypt (subprocess) → `Node::publish` (blob + index entry keyed
  `{agent}/{relative_path}`; the ONLY write path — temp-tag prevents the GC race).
- Export path: index entry → `Node::blob` (local miss → bounded fetch from peers behind
  the net seam; iroh-docs never retries a missed download, upstream iroh-docs#88) →
  decrypt → atomic temp+rename write-back.
- Conflicts: winner = newest entry, selected once behind the net seam (`IndexRecord`);
  append-only formats (pi/omp) get the deterministic lossless line-union merge, others
  newest-wins. The merge verdict is all-or-skip: a partial version set never merges.
- Per-key errors are logged (`eprintln!("ssync: …")`) and retried next tick; one bad key
  never kills the daemon.
- Pairing is two mechanisms: the cluster artifact (`ssync cluster
  init/join/add/rm/show`; one secret file = namespace secret + recipients + node-ids,
  `cluster_path` in config — the recommended mode; clan assembles the same file at
  service start via `ssync cluster render`) and ad-hoc ticket exchange (`ssync
  ticket`/`join`; the ticket embeds direct addresses).
  `recipients` set (ticket mode only) = per-machine age identities with
  multi-recipient encryption; empty = one shared identity. Recipient-set or namespace
  changes re-publish/evict (issue #22).

### Hard rules (pointers into docs/DECISIONS.md — don't re-litigate)

- **Store session files as opaque bytes** (§2). Reading a single header field is fine;
  parsing entry lines is not.
- **Watch-and-import, never sync-in-place** (§10). Read from the agent's dir; write back
  atomically (temp + rename). Never symlink/bind it into the engine.
- **Encryption (age) is not optional and not deferrable** (§7).
- **Leaderless** (§4): no code path may assume a special/authority node. **User runs no
  server** (§6): iroh public defaults plus mDNS local discovery (LAN peers dialable
  by node-id alone; tickets additionally embed direct addresses).
- iroh, iroh-docs, iroh-blobs, and age move fast: when unsure of a current API, read the
  crate's actual docs/source — never code from memory.

## Key Directories

- `crates/ssync/` — binary: clap CLI (`init/daemon/cluster/ticket/join/status/conflicts/`
  `search/cleanup/service/cleanup-timer/keygen-node/keygen-namespace`) + daemon wiring;
  `src/main.rs` dispatches. `systemd.rs` owns the secure unit lifecycle shared by the
  explicit renderers in `service.rs` and `cleanup_timer.rs`; cluster artifact commands
  live in `cluster.rs`.
- `crates/ssync-core/` — `Config`, `Engine`, `StatusReport`; pure decision cores live in
  their own submodules.
- `crates/ssync-net/` — iroh endpoint/router/docs/blobs/gossip setup, `Node`,
  `IndexRecord` (the only place winner selection happens), blob GC.
- `crates/ssync-crypto/` — `AgeIdentity`: PQ-hybrid keys via the `age`/`age-keygen` CLI;
  Rust `age`-crate backend disabled behind feature `rust-age` until it gains ML-KEM.
- `crates/ssync-adapters/` — `Adapter` trait + `adapter_for` factory. New agent = one
  `impl Adapter` + one `adapter_for` arm (`"pi" | "omp"` share `PiAdapter`; claude-code
  and codex have their own impls, newest-wins permanently by policy — DECISIONS §8, #25).
- `nix/` — package, devshell, treefmt, checks (incl. three NixOS VM tests), NixOS/HM/clan
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
nix flake check                                         # full CI gate (package, formatting, vm-sync, vm-mdns, vm-module)
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
- **DI:** constructor injection only (`SessionFilesystem::new(SessionFsConfig)` →
  `Engine::with_filesystem(SessionFilesystem, AgeIdentity, Node)`); no globals or singletons.
  Test seams are named production methods
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
  Release notes are `--generate-notes`: only merged PRs between tags appear, direct
  pushes to main are invisible — route user-visible work through PRs.

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
- `crates/ssync-net/src/lib.rs` — `Node`, `publish`, `blob`, `signals`, `IndexRecord`.
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
- `nix flake check` additionally runs three NixOS VM tests (`vm-sync` e2e over virtual
  LAN, `vm-mdns` node-id-only pairing via mDNS, `vm-module` service/hardening test).

## pi session format (caveat)

pi stores sessions at `~/.pi/agent/sessions/<encoded-cwd>/<ts>_<sessionId>.jsonl`, one
append-only JSONL file per session, header record (`version:3`, `id` uuidv7, `cwd`) on
line 1 (omp: line 2, after a `type:title` record). pi keys on the **absolute cwd**, so by
default a synced project must live at the **same absolute path on every machine**; the
opt-in `[[path_map]]` bridges differing paths by rewriting exactly the header `cwd` and
the encoded dir name (DECISIONS §3 amendment, map #42). Derive `PiAdapter` identity from
the path alone; treat `<encoded-cwd>` as opaque (pi and omp encode differently — the
path-map encode/decode in `pi.rs` is the sanctioned exception). omp uses the
same layout at `~/.omp/agent/sessions` and adds a per-session artifact dir
(`<encoded-cwd>/<ts>_<sessionId>/`: subagent transcripts, `__advisor.jsonl`) whose files
are part of the session and sync with it (identity from the artifact dir name — DECISIONS
§9). pi appends in place (no temp+rename), so
imports must stay debounced. Full reference: `docs/pi-format-notes.md` (re-verify against
the installed pi version before relying on it).
