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
fresh addresses). `ssync ticket` prints that file. `ssync join <ticket>` stages
the ticket at `data_dir/remote-ticket`; the daemon consumes it on the next start,
joins the namespace, and deletes the staged file.

This file-based flow avoids two processes opening the single-writer store at once
(the daemon owns it; the `ticket`/`join`/`status` commands only touch small files).

## Trust boundary

- A ticket grants **write access to the index**. Treat it as a secret; share it
  only with your own machines, over a channel you trust.
- Session **contents** are age-encrypted before they ever leave a machine, so a
  peer (or any relay) only sees ciphertext. Holding a ticket does not grant the
  ability to decrypt sessions — that requires the shared age identity, which is
  distributed separately (see `setup.md`).
- iroh's transport is end-to-end encrypted independently of age; age is at-rest
  and defense-in-depth on top.

## Discovery and connectivity

- On a LAN, peers can find each other via mDNS.
- Across the internet, iroh's public discovery + relay infrastructure bootstraps
  the connection; once a direct path is punched, data flows peer-to-peer. The
  relay only ever carries ciphertext, and only as a fallback.
- Because tickets embed direct addresses, peers on the same network connect
  directly without needing any relay at all.
