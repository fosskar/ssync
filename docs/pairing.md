# Pairing

How machines are paired and what the trust boundary is. Two mechanisms exist:
the **cluster file** (recommended manual mode; `ssync cluster`, see `setup.md`)
and ad-hoc **tickets**. The clan service assembles the cluster file on each
machine at service start (`ssync cluster render`, secret from clan.vars).

## The namespace

All of a user's machines share one **iroh-docs namespace** — the synchronized
index of sessions. The first machine's daemon creates it; every other machine
joins it. The namespace id and the joined state are persisted under `data_dir`
(`data_dir/namespace`), so a machine reopens the same namespace across restarts.

## The cluster file

`ssync cluster init` derives the namespace deterministically from a shared
32-byte secret stored in one distributable artifact together with every
machine's age recipient and (optionally) node-id. All peers holding the same
file open the same namespace — no ticket exchange; the node-ids seed the
resync peer list and mDNS/public discovery makes them dialable. Membership
changes are edits to this one file (`ssync cluster add/rm`); removal rotates
the secret, which is what evicts the removed machine (it keeps the old
namespace, the remaining machines abandon it).

## Tickets (ad-hoc alternative)

Pairing uses an iroh-docs **`DocTicket`**. It carries:

- the namespace capability (write access to the shared index), and
- the issuing node's direct addresses and relay URL, so the joiner can dial it.

The daemon writes its current ticket to `data_dir/ticket` on every start (with
fresh addresses; in cluster mode no ticket file is written).
`ssync ticket` prints that file. `ssync join <ticket>` stages the ticket at
`data_dir/remote-ticket`; the daemon consumes it on the next start,
joins the namespace, and deletes the staged file.

This file-based flow avoids two processes opening the single-writer store at once
(the daemon owns it; the `ticket`/`join`/`status` commands only touch small files).

## Trust boundary

- A ticket or the cluster file's namespace secret grants **write access to the
  index**. Treat both as secrets; share them only with your own machines, over
  a channel you trust. (The cluster file additionally lists recipients — public
  keys — but the secret inside makes the whole file secret.)
- Session **contents** are age-encrypted before they ever leave a machine, so a
  peer (or any relay) only sees ciphertext. Holding the index capability does
  not grant the ability to decrypt sessions — that requires an age identity in
  the recipient
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
  from the joined ticket (or the cluster file's node-ids)
  and then extended at runtime from live sync events: every gossip neighbor and
  successfully synced peer is remembered. So a ticket issuer, which starts with
  no peers at all, learns its joiners as soon as they connect. Learned peers
  are in-memory only; a restart falls back to the seeded list.

## Discovery and connectivity

- On a LAN, peers find each other via mDNS local discovery: any peer named by
  node-id (the cluster file's members, or a ticket whose embedded addresses went
  stale) is dialable without internet or relay.
  Caveat: the underlying swarm-discovery announces only on the default-route
  interface, so a multi-homed machine whose default route is not the shared
  LAN won't be discovered over that LAN (relay/DNS still cover it).
- Across the internet, iroh's public discovery + relay infrastructure bootstraps
  the connection; once a direct path is punched, data flows peer-to-peer. The
  relay only ever carries ciphertext, and only as a fallback.
- Because tickets embed direct addresses, peers on the same network connect
  directly without needing any relay at all.
