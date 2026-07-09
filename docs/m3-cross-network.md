# M3: manual cross-network verification

One-time check that sync works between two machines on *different* networks —
the relay-bootstrapped path that CI cannot exercise (the two-VM test covers
LAN/direct only). Everything here is observation; no configuration changes.

## Setup

- Two machines already paired (`ssync ticket` / `ssync join`), daemon running
  on both, known-good on the same LAN.
- Move one machine to a different network: phone tether, another site, or any
  connection that is not the home LAN. A VPN back into the home network defeats
  the purpose — turn it off.

## Steps

1. On the remote machine, confirm the daemon is up:
   `ssync status` — `updated:` must be recent (status.toml mtime is the
   liveness signal).
2. Check the connection path on either side:

   ```
   $ ssync status
   namespace: …
   sessions:  …
   conflicts: 0
   peer:      <node-id> (relay)
   ```

   Path meanings:
   - `relay` — traffic rides the n0 relay; proves the bootstrap path works.
   - `direct` — holepunching succeeded; even better (relay was still the
     bootstrap unless both machines share a LAN).
   - `mixed` — both address kinds active, typically mid-migration; fine.
   - `unknown` — no active address: not connected (yet). If it stays
     `unknown` past a resync interval (60s), the cross-network path is broken —
     capture daemon output and file an issue.
3. Create a session on one machine (run any pi/omp session) and watch it appear
   on the other: `ssync status` session count increments, file lands under the
   agent's sessions dir. Repeat in the other direction.
4. Optional latency sanity check: touch-append to an existing session; the
   change should propagate within a few debounce intervals (seconds, not
   minutes).

## What this does NOT need to verify

The relay seeing only ciphertext is by construction, not by observation:
session content is age-encrypted before publish (the only write path), and
iroh traffic is additionally QUIC-encrypted end-to-end between the node keys.
The relay never holds a decryption key. See docs/threat-model.md.

## Result

Record the outcome (date, networks used, observed path kinds) in TODO.md and
tick the M3 item.
