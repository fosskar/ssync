# Setup

How to install ssync, set up keys, and pair two machines. ssync is
leaderless: every machine runs the same daemon as an equal peer.

## Prerequisites

- A synced project must live at the **same absolute path on every machine**. pi
  keys sessions on the absolute working directory, so `~/projects/foo` on one
  machine and `~/code/foo` on another are, to pi, different sessions. See
  `identity.md`.
- Age keys, one of two modes:
  - **Per-machine keypairs** (recommended): each machine keeps its own age key;
    the other machines learn its recipient from the cluster file (or, in ticket
    mode, from the config's `recipients`). Enables per-device revocation. The
    clan service sets this up automatically.
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

### systemd service (non-nix)

For plain-binary deployments, `ssync service install` writes a hardened unit
and enables it (`ssync init` first — install reads the config to open the
sandbox for exactly the session dirs and `data_dir`, creating them if missing):

- as a regular user: a user unit at `~/.config/systemd/user/ssync.service`,
  enabled and started immediately. Install also enables lingering (best
  effort) so the daemon survives logout and starts at boot; if that fails,
  run `loginctl enable-linger` yourself.
- as root: a system unit at `/etc/systemd/system/ssync.service`. Pass
  `--user <name>` (sessions, keys, and watched dirs are per-user, so the daemon
  runs as that user) and an explicit `--config` whose paths are absolute (`~/`
  would expand differently for root and the service user). Install verifies the
  service user can actually reach the binary, config, `age`, and every write
  path — a root-only location (`/root/.nix-profile`, a 0700 home) fails the
  install instead of crash-looping the unit.

`age`/`age-keygen` are resolved at install time and pinned in the unit's
`PATH`, so a key setup that works in your shell keeps working under systemd.
Re-running install after a config or binary change rewrites the unit and
restarts the daemon. `ssync service uninstall` stops and disables the unit,
removes it, and reloads systemd.

The unit carries the same hardening set as the nix modules below. Sandboxing in
user units needs unprivileged user namespaces (default on most distros; some
kernels restrict them — if the service fails with a namespace error, check
`sysctl kernel.unprivileged_userns_clone` on Debian-family kernels, or
`user.max_user_namespaces` / AppArmor restrictions elsewhere). Not on systemd?
The daemon is a plain foreground process; a cron `@reboot` line or any
supervisor works.

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
users. It wraps the NixOS module: `clan.vars` generates a per-machine age key
(peers encrypt to each other's recipients), a shared namespace secret, and each
machine's node-id; every machine then assembles the same `cluster.toml` at
service start. Peers **auto-connect with no manual pairing** and you configure
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

That's the whole setup for clan — no `ssync ticket` / `ssync join`. Machines
outside the clan can join too: list them in `roles.peer.settings.extraMembers`
(`"recipient:node-id"`), copy `/run/ssync/cluster.toml` from any clan machine
to them, and run `ssync cluster join` there (re-copy after clan membership
changes — they rotate the namespace secret). The manual pairing below is for
fully standalone setups.

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
# optional: withhold matching sessions from sync (`*`-glob against the
# path under session_dir, so a project dir name works). A matching
# session is frozen on every machine: never published, never
# materialized, never deleted.
exclude = ["*client-x*"]

# optional: sync omp (oh-my-pi) sessions side by side
[[agents]]
agent = "omp"
session_dir = "~/.omp/agent/sessions"

# optional: Claude Code and Codex (newest-wins on conflict by policy; see
# docs/*-format-notes.md)
[[agents]]
agent = "claude-code"
session_dir = "~/.claude/projects"

[[agents]]
agent = "codex"
session_dir = "~/.codex/sessions"
```

The Nix modules generate this file for you from their options (`exclude` is
per agent entry there too). `ssync init` writes a default config listing every
known agent whose session directory exists on the machine (pi, omp,
claude-code, codex).

**Disabling an agent** needs no switch: an agent syncs only while it has an
`[[agents]]` entry, so remove the entry (nix: drop it from
`services.ssync.agents`) and restart. Already-synced sessions freeze — peers
keep their copies, nothing is deleted — and the daemon ignores index entries
for agents it has no adapter for. Machines that never had the agent installed
simply materialize its sessions if it is configured, or skip them if not.

## First machine

```bash
ssync init          # writes config.toml, generates the age key, prints recipient + node-id
ssync cluster init  # creates the cluster file and points the config at it
ssync daemon
```

The cluster file (`cluster.toml` next to the config, 0600) is the whole
membership: the shared namespace secret, every machine's age recipient, and
optionally its node-id. Treat it like a key — distribute it only over a channel
you already trust for secrets (scp, sops-nix, USB).

## Adding a machine

On the new machine:

```bash
ssync init          # prints this machine's recipient and node-id
```

On any existing machine, add it and redistribute:

```bash
ssync cluster add <recipient> --node-id <node-id>
scp ~/.config/ssync/cluster.toml newmachine:...   # your secret channel
```

On the new machine, adopt the file and start:

```bash
ssync cluster join <file>
ssync daemon
```

Restart the daemons on the other machines so they encrypt to the new recipient.
`ssync cluster show` prints the members and the namespace the secret derives —
the same on every machine means they agree.

## Removing a machine

```bash
ssync cluster rm <recipient>
```

This drops the machine and **rotates the namespace secret inside the file** —
the removed machine still knows the old secret (= index write access), so the
rotation is what actually evicts it. Distribute the updated file to every
remaining machine and restart the daemons: they detect the changed recipient
set (re-encrypt and re-publish everything), open the new namespace, and drop
the abandoned replica (see `threat-model.md` for the two revocation layers).

The clan service manages all of this automatically via clan.vars: the peer set
is part of the namespace generator's identity, so membership changes rotate
the shared secret on the next deploy.

## Ticket pairing (alternative)

Without a cluster file, machines can pair ad hoc: `ssync ticket` on a running
daemon prints a pairing ticket, `ssync join '<ticket>'` + `ssync daemon` on the
other machine joins it (see `pairing.md`). Tickets carry no recipient list —
either copy one shared age key everywhere, or maintain each machine's
`recipients` in the config by hand. Membership changes are manual per machine;
the cluster file is the recommended mode.

## Everyday use

Run `ssync daemon` on each machine (the Nix modules do this as a service). Work
with pi as usual; sessions are imported, encrypted, and synced automatically, and
incoming sessions are written back into pi's directory so you can `pi --resume`
them on any machine.

```bash
ssync status      # namespace, session count, conflict count
ssync conflicts   # list sessions that diverged across machines
```

## Automatic cleanup

`ssync cleanup` prunes old sessions but is manual. To schedule it, either use
the CLI (plain-binary deployments) or the nix module option.

**Deletions propagate mesh-wide.** Cleanup deletes local files; the daemon
tombstones them and every peer deletes its copy. A timer on *one* machine
prunes *all* machines — that is the feature. One machine with a timer is
enough; enabling it on several is harmless (selection is idempotent) but
redundant. The wipe guard still applies: a run that would delete *all* of an
agent's sessions (e.g. an empty or unmounted session dir) refuses instead.

With the CLI, `cleanup-timer enable` installs and starts a systemd
timer/service pair running `ssync cleanup --apply` (user units; system units
as root with `--user`/`--config`, same rules as `ssync service install`):

```bash
ssync cleanup-timer enable --every weekly              # delete sessions older than 90d
ssync cleanup-timer enable --every 2d --keep 30d       # every 2 days, 30d retention
ssync cleanup-timer enable --every weekly --unnamed    # only untitled sessions, any age
ssync cleanup-timer status
ssync cleanup-timer disable
```

`--every` accepts `2d`/`7d` style periods, or a raw systemd calendar
expression (`weekly`, `*-*-* 03:00:00`). Calendar schedules catch up after
downtime (`Persistent=true`); day periods that `OnCalendar` cannot express run
on unit uptime instead. `--keep` defaults to `90d` unless `--unnamed` is the
only selector. Non-systemd platforms: the cleanup command is non-interactive,
so a cron line works — `0 3 * * 0 ssync cleanup --keep 90d --apply`.

With the NixOS or home-manager module:

```nix
services.ssync.autoCleanup = {
  enable = true;
  schedule = "weekly";  # systemd OnCalendar expression
  keep = "90d";         # null to select by `unnamed` only
  unnamed = false;      # also delete untitled sessions
};
```
