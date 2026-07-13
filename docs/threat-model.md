# Threat model

What ssync protects, what it exposes, and the assumptions it makes.

## What ssync protects

- **Session contents at rest and in transit to peers.** Every session file is
  age-encrypted before it is handed to iroh-blobs. Peers, and any relay that
  carries traffic, only ever see ciphertext. Sessions routinely contain secrets
  (API keys, tokens, file contents), so this is on by default and not optional.
- **Transport.** iroh connections are end-to-end encrypted (QUIC/TLS) independent
  of age. age is at-rest protection and defense-in-depth on top of that.

## What is exposed

- **To a relay** (only when a direct connection can't be punched): ciphertext
  blobs and encrypted transport metadata. Never plaintext session contents. Using
  iroh's public relays is like using public DNS — you run no server, and the
  relay cannot read your data.
- **To a holder of a pairing ticket:** write access to the synced *index*
  (see `pairing.md`). It does **not** grant the ability to decrypt sessions; that
  needs an age identity in the recipient set, provisioned separately.
- **Index metadata:** the index maps `{agent}/{relative_path}` to a blob hash.
  The relative path (which encodes the project's absolute directory and the
  session id) is visible to anyone in your namespace — i.e. your own machines.

## Trust assumptions

- **Your machines are trusted.** ssync is designed for one user's own set of
  machines. Every machine in the recipient set can decrypt every synced session —
  multi-recipient encryption is about *revocation*, not compartmentalization.
  With the cluster file (the recommended mode), removing a machine —
  `ssync cluster rm <recipient>` — rotates the namespace secret inside the
  artifact; the removed machine still knows the old secret (= index write
  access), so that rotation is what actually evicts it (automatic with the
  clan service on the next deploy). Every remaining machine also detects the
  changed recipient set and re-encrypts/re-publishes all sessions under it
  (the recipient set is fingerprinted in the sync state; superseded
  ciphertext is garbage-collected; see issue #22), then opens the new
  namespace and drops the abandoned replica. In ticket mode (the ad-hoc
  alternative, no cluster file), eviction means editing each machine's
  `recipients` by hand and distributing a new shared secret manually.
  Revocation is forward-only either way: blobs the removed machine already
  fetched stay readable.
- **Age keys are the crown jewels.** Any key in the recipient set decrypts all
  synced sessions. Store keys `0600` (the daemon refuses to run on a
  group/world-readable key). In shared mode, provision the one key over a
  trusted channel and manage it with your existing secret tooling (e.g.
  sops-nix); in per-machine mode keys never leave their machine.
- **Pairing tickets and the cluster file are secrets.** A ticket grants write
  access to your index; the cluster file additionally holds the namespace
  secret (same index write access) alongside recipients — public keys — but
  the secret inside makes the whole file secret. Share either only with your
  own machines, over a channel you trust.

## Non-goals

- ssync does not defend against a compromised machine whose key is still in the
  recipient set — such a machine can read everything by design (revoke it, then
  the damage stops at what it already saw).
- ssync does not hide the *existence* or *size* of sessions from peers in your
  namespace, nor the relative paths. It hides their contents.

## Post-quantum

ssync uses post-quantum hybrid keys by default: ML-KEM-768 + X25519, native in
`age` >= 1.3 (`age-keygen -pq`, recipients `age1pq1…`). Your sessions stay
confidential if either primitive is broken (a future quantum computer, or a bug in
the young ML-KEM). PQ-only is intentionally not used — it would drop the classical
fallback with no security gain.
