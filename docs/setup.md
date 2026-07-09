# Setup

How to install ssync, set up keys, and pair two machines. ssync is
leaderless: every machine runs the same daemon as an equal peer.

## Prerequisites

- A synced project must live at the **same absolute path on every machine**. pi
  keys sessions on the absolute working directory, so `~/projects/foo` on one
  machine and `~/code/foo` on another are, to pi, different sessions. See
  `identity.md`.
- Age keys, one of two modes:
  - **Per-machine keypairs** (recommended): each machine keeps its own age key and
    lists the other machines' recipients in the config's `recipients`. Enables
    per-device revocation. The clan service sets this up automatically.
  - **Shared identity**: the same private key on every machine (`recipients`
    empty). Provision it out of band (e.g. sops-nix or a manual secure copy).

## Install

### From source (plain binary)

No nix required: a plain Rust workspace. The daemon shells out to `age`/`age-keygen`
≥ 1.3 (post-quantum support), which must be on `PATH` at runtime:

```bash
cargo build --release
./target/release/ssync --help
```

With nix (flakes), the package wraps the `age` dependency for you:

```bash
nix run github:fosskar/ssync -- --help      # try it
nix profile install github:fosskar/ssync   # imperative install
```

Declaratively, add ssync as a flake input:

```nix
inputs.ssync.url = "github:fosskar/ssync";
```

and use `inputs.ssync.packages.<system>.default` — or better, one of the modules
below (they need the flake input anyway).

### home-manager module (recommended — sessions are per-user)

```nix
{
  imports = [ inputs.ssync.homeManagerModules.default ];
  services.ssync.enable = true;
  # agents defaults to pi and omp at the user's home; the age key is auto-generated.
  # For multi-machine: either share one key via ageIdentityFile, or keep the
  # auto-generated per-machine key and list the peers' recipients:
  # services.ssync.recipients = [ "age1pq1..." ];
  # To also sync Claude Code sessions (newest-wins on conflict):
  # services.ssync.agents = [
  #   { agent = "pi";  sessionDir = "${config.home.homeDirectory}/.pi/agent/sessions"; }
  #   { agent = "omp"; sessionDir = "${config.home.homeDirectory}/.omp/agent/sessions"; }
  #   { agent = "claude-code"; sessionDir = "${config.home.homeDirectory}/.claude/projects"; }
  # ];
}
```

### NixOS module

```nix
{
  imports = [ inputs.ssync.nixosModules.default ];
  services.ssync = {
    enable = true;
    user = "alice";
    # agents defaults to pi and omp at the configured user's home; the age key
    # is auto-generated.
  };
}
```

`user` is required (no default): the daemon runs as that user, who must own the
agents' session dirs. It is not a cross-user bridge — for projects under `$HOME`
the username is part of the session key, so use the *same* username on every
machine (see `identity.md`).

### clan service

A clan service (`clan.modules.ssync`, single `peer` role) is exposed for clan
users. It wraps the NixOS module and uses `clan.vars` to generate and distribute
**everything** — a per-machine age key (peers encrypt to each other's recipients),
a shared namespace secret, and each machine's
node-id — so peers **auto-connect with no manual pairing** and you configure
nothing but the peer list:

```nix
# in your clan inventory
instances.ssync = {
  module = {
    name = "ssync";
    input = "ssync";
  }; # resolves from the ssync flake input
  roles.peer.machines = {
    laptop.settings.user = "alice";
    desktop.settings.user = "alice";
  };
};
```

That's the whole setup for clan — no `ssync ticket` / `ssync join`. The manual
pairing below is only for the non-clan (standalone) modules.

## Configuration

`ssync` reads `$XDG_CONFIG_HOME/ssync/config.toml` (override with `--config`):

```toml
# a leading ~/ expands to the local home directory, so one config file
# works across machines with different usernames.
age_identity_path = "~/.config/ssync/age.key"
data_dir = "~/.local/share/ssync"

[[agents]]
agent = "pi"
session_dir = "~/.pi/agent/sessions"

# optional: sync omp (oh-my-pi) sessions side by side
[[agents]]
agent = "omp"
session_dir = "~/.omp/agent/sessions"

# optional: Claude Code and Codex (newest-wins on conflict until their
# formats are verified append-only; see docs/*-format-notes.md)
[[agents]]
agent = "claude-code"
session_dir = "~/.claude/projects"

[[agents]]
agent = "codex"
session_dir = "~/.codex/sessions"
```

The Nix modules generate this file for you from their options. `ssync init`
writes a default config listing every known agent whose session directory
exists on the machine (pi, omp, claude-code, codex).

## First machine

```bash
ssync init          # writes config.toml and generates the age key if missing
ssync daemon        # creates a sync namespace and starts syncing
```

Either copy the generated age key (`age_identity_path`) to your other machines
(shared mode), or keep one key per machine and add each peer's recipient (printed
by `ssync init`) to every other machine's `recipients` list.

## Pairing a second machine

On the second machine, install the same age key (shared mode) or exchange
recipients (per-machine mode), then:

```bash
ssync init
```

On the **first** machine (daemon running), print its pairing ticket:

```bash
ssync ticket
```

On the second machine, stage that ticket and start the daemon:

```bash
ssync join '<ticket-from-first-machine>'
ssync daemon
```

The second machine joins the first machine's namespace and begins syncing. See
`pairing.md` for what the ticket contains.

## Everyday use

Run `ssync daemon` on each machine (the Nix modules do this as a service). Work
with pi as usual; sessions are imported, encrypted, and synced automatically, and
incoming sessions are written back into pi's directory so you can `pi --resume`
them on any machine.

```bash
ssync status      # namespace, session count, conflict count
ssync conflicts   # list sessions that diverged across machines
```
