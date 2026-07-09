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
  With per-machine keypairs (the recommended mode), removing a machine's
  recipient and regenerating only affects sessions published *after* the change:
  imports dedup on plaintext, so unchanged sessions are never re-encrypted, and
  their existing blobs stay readable to the removed key. The removed machine
  also still holds the namespace secret, i.e. write access to the index. Full
  eviction requires rotating the namespace as well — see issue #22.
- **Age keys are the crown jewels.** Any key in the recipient set decrypts all
  synced sessions. Store keys `0600` (the daemon refuses to run on a
  group/world-readable key). In shared mode, provision the one key over a
  trusted channel and manage it with your existing secret tooling (e.g.
  sops-nix); in per-machine mode keys never leave their machine.
- **Pairing tickets are secrets.** A ticket grants write access to your index.
  Share it only with your own machines over a channel you trust.

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
