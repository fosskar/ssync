# ssync

**Continuous, peer-to-peer sync of coding-agent session files across your own machines.**

Work with an AI coding agent on your desktop, walk away, and resume the _same_ session on
your laptop — automatically, no manual commands, no server to run.

ssync watches where your agent stores its sessions, keeps them encrypted and in sync across
all your machines over a direct peer-to-peer connection (via [iroh](https://iroh.computer)),
and writes incoming sessions back so the agent can `--resume` them anywhere.

## Status

Early. v1 targets the **pi** agent only and is under active construction. See
`docs/DECISIONS.md` for the design rationale.

## What it is / isn't

- ✅ Syncs session **files** across your machines, peer-to-peer, automatically.
- ✅ Encrypted at rest by default ([age](https://github.com/FiloSottile/age)).
- ✅ No central server, no VPS, no relay you have to run. LAN sync needs zero
  infrastructure; internet sync uses iroh's free public discovery (which only ever sees
  ciphertext).
- ❌ Not a live "continue a running process on another machine" tool.
- ❌ Not an agent orchestrator or dashboard (see cctl, ccmanager, etc. for that).

## How it works (one paragraph)

Every machine runs the same daemon — there is no hub. Each daemon watches the agent's
session directory, encrypts changed sessions with age, and publishes them into a shared,
self-converging index ([iroh-docs](https://github.com/n0-computer/iroh-docs)) with the file
contents moved as content-addressed blobs ([iroh-blobs](https://github.com/n0-computer/iroh-blobs)).
Sessions are identified by the agent's own session id, so "the same session" is coherent on
every machine. Peers find each other on a LAN via mDNS, or across the internet via iroh's
public discovery + NAT hole-punching — then sync directly.

## Quick start

ssync needs one **shared age key** on all your machines, and each synced project
must live at the **same absolute path** everywhere (see `docs/identity.md`).

```bash
# first machine
ssync init          # writes config.toml, generates the age key
ssync daemon        # creates a namespace and starts syncing
ssync ticket        # prints this machine's pairing ticket

# second machine (after copying the same age key over)
ssync init
ssync join '<ticket-from-first-machine>'
ssync daemon        # joins the namespace and syncs
```

Run `ssync daemon` on each machine (the Nix modules do this as a service). Full
instructions: `docs/setup.md`. Pairing details: `docs/pairing.md`.

```bash
ssync status        # namespace, session count, conflicts
ssync conflicts     # sessions that diverged across machines
```

## Security

Sessions often contain secrets (API keys, file contents). ssync encrypts every session at
rest with age before it ever leaves the machine, so peers and any relay see only ciphertext.
See `docs/threat-model.md`. Sessions are encrypted with post-quantum hybrid keys by default
(ML-KEM-768 + X25519, via `age-keygen -pq`).

## License

MIT. See `LICENSE`.
