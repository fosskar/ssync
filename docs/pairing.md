# Pairing

How machines are paired and what the trust boundary is.

## The namespace

All of a user's machines share one **iroh-docs namespace** — the synchronized
index of sessions. The first machine's daemon creates it; every other machine
joins it. The namespace id and the joined state are persisted under `data_dir`
(`data_dir/namespace`), so a machine reopens the same namespace across restarts.

## Tickets

Pairing uses an iroh-docs **`DocTicket`**. It carries:

- the namespace capability (write access to the shared index), and
- the issuing node's direct addresses and relay URL, so the joiner can dial it.

The daemon writes its current ticket to `data_dir/ticket` on every start (with
fresh addresses; in shared-namespace mode no ticket file is written).
`ssync ticket` prints that file. `ssync join <ticket>` stages the ticket at
`data_dir/remote-ticket`; the daemon consumes it on the next start,
joins the namespace, and deletes the staged file.

This file-based flow avoids two processes opening the single-writer store at once
(the daemon owns it; the `ticket`/`join`/`status` commands only touch small files).

## Trust boundary

- A ticket grants **write access to the index**. Treat it as a secret; share it
  only with your own machines, over a channel you trust.
- Session **contents** are age-encrypted before they ever leave a machine, so a
  peer (or any relay) only sees ciphertext. Holding a ticket does not grant the
  ability to decrypt sessions — that requires an age identity in the recipient
  set, provisioned separately (see `setup.md`).
- iroh's transport is end-to-end encrypted independently of age; age is at-rest
  and defense-in-depth on top.

## Topology (3+ machines)

Two layers behave differently; both verified against the pinned crates
(iroh-docs/iroh-gossip 0.101, live.rs `NeighborUp` → `sync_with_peer`):

- **Live index sync self-meshes.** Gossip membership is HyParView: joins are
  forwarded and peers shuffled, so machines that never exchanged tickets still
  become direct neighbors, and iroh-docs syncs with every new neighbor. A chain
  of tickets (B and C both joined via A) keeps converging while A is offline,
  provided B and C are already in the swarm — gossip views live in memory, so
  a daemon restart only re-dials explicitly known peers (see below).
- **ssync's own peer list starts narrower but grows.** `Node::peers` — what the
  missed-blob recovery (`fetch_blob`) and the periodic resync dial — is seeded
  from the joined ticket (or `peers` in config for the shared-namespace mode)
  and then extended at runtime from live sync events: every gossip neighbor and
  successfully synced peer is remembered. So a ticket issuer, which starts with
  no peers at all, learns its joiners as soon as they connect. Learned peers
  are in-memory only; a restart falls back to the seeded list.

## Discovery and connectivity

- On a LAN, peers will find each other via mDNS (not yet implemented — see
  TODO.md; today LAN connectivity rides the ticket's embedded direct addresses).
- Across the internet, iroh's public discovery + relay infrastructure bootstraps
  the connection; once a direct path is punched, data flows peer-to-peer. The
  relay only ever carries ciphertext, and only as a fallback.
- Because tickets embed direct addresses, peers on the same network connect
  directly without needing any relay at all.
