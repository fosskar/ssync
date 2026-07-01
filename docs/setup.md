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
  services.ssync = {
    enable = true;
    # sessionDir defaults to ~/.pi/agent/sessions
    ageIdentityFile = config.sops.secrets.ssync-age.path; # your shared key
  };
}
```

### NixOS module

```nix
{
  imports = [ inputs.ssync.nixosModules.default ];
  services.ssync = {
    enable = true;
    user = "alice";
    sessionDir = "/home/alice/.pi/agent/sessions";
    ageIdentityFile = "/run/secrets/ssync-age";
  };
}
```

A `clanModules.default` clan service (single `peer` role) is also exposed for
clan users; it is a thin wrapper over the NixOS module and is entirely optional.

## Configuration

`ssync` reads `$XDG_CONFIG_HOME/ssync/config.toml` (override with `--config`):

```toml
agent = "pi"
session_dir = "/home/alice/.pi/agent/sessions"
age_identity_path = "/home/alice/.config/ssync/age.key"
data_dir = "/home/alice/.local/share/ssync"
```

The Nix modules generate this file for you from their options.

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
