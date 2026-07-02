# Setup

How to install ssync, generate the shared key, and pair two machines. ssync is
leaderless: every machine runs the same daemon as an equal peer.

## Prerequisites

- A synced project must live at the **same absolute path on every machine**. pi
  keys sessions on the absolute working directory, so `~/projects/foo` on one
  machine and `~/code/foo` on another are, to pi, different sessions. See
  `identity.md`.
- One **shared age identity**, distributed to every machine (same private key
  everywhere). Provision it out of band (e.g. sops-nix or a manual secure copy).

## Install

### Plain binary (Nix)

```bash
nix build git+https://codeberg.org/fosskar/ssync
./result/bin/ssync --help
```

### home-manager module (recommended — sessions are per-user)

```nix
{
  imports = [ inputs.ssync.homeManagerModules.default ];
  services.ssync.enable = true;
  # agents defaults to pi at ~/.pi/agent/sessions; the age key is auto-generated.
  # For multi-machine, point ageIdentityFile at a key you share across machines.
  # To also sync omp (oh-my-pi) sessions:
  # services.ssync.agents = [
  #   { agent = "pi";  sessionDir = "${config.home.homeDirectory}/.pi/agent/sessions"; }
  #   { agent = "omp"; sessionDir = "${config.home.homeDirectory}/.omp/agent/sessions"; }
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
    # agents defaults to pi at alice's ~/.pi/agent/sessions; age key auto-generated.
  };
}
```

### clan service

A clan service (`clan.modules.ssync`, single `peer` role) is exposed for clan
users. It wraps the NixOS module and uses `clan.vars` to generate and distribute
**everything** — the shared age key, a shared namespace secret, and each machine's
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
age_identity_path = "/home/alice/.config/ssync/age.key"
data_dir = "/home/alice/.local/share/ssync"

[[agents]]
agent = "pi"
session_dir = "/home/alice/.pi/agent/sessions"

# optional: sync omp (oh-my-pi) sessions side by side
[[agents]]
agent = "omp"
session_dir = "/home/alice/.omp/agent/sessions"
```

The Nix modules generate this file for you from their options. `ssync init`
writes a default config listing every known agent whose session directory
exists on the machine (pi, omp).

## First machine

```bash
ssync init          # writes config.toml and generates the age key if missing
ssync daemon        # creates a sync namespace and starts syncing
```

Copy the generated age key (`age_identity_path`) to your other machines — the
**same key must be present on all of them**.

## Pairing a second machine

On the second machine, install the same age key, then:

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
