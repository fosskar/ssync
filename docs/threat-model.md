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
  needs the shared age identity, distributed separately.
- **Index metadata:** the index maps `{agent}/{relative_path}` to a blob hash.
  The relative path (which encodes the project's absolute directory and the
  session id) is visible to anyone in your namespace — i.e. your own machines.

## Trust assumptions

- **Your machines are trusted.** ssync is designed for one user's own set of
  machines. Every machine in a namespace holds the shared age key and can decrypt
  every synced session. There is no per-device revocation in v1 (per-machine
  keypairs with multi-recipient encryption is a deferred refinement — see
  `../TODO.md`).
- **The shared age key is the crown jewel.** Anyone with it can decrypt all synced
  sessions. Provision it over a trusted channel, store it `0600` (the daemon
  refuses to run on a group/world-readable key), and manage it with your existing
  secret tooling (e.g. sops-nix).
- **Pairing tickets are secrets.** A ticket grants write access to your index.
  Share it only with your own machines over a channel you trust.

## Non-goals

- ssync does not defend against a compromised machine that already holds the age
  key — such a machine can read everything by design.
- ssync does not hide the *existence* or *size* of sessions from peers in your
  namespace, nor the relative paths. It hides their contents.

## Post-quantum

ssync uses post-quantum hybrid keys by default: ML-KEM-768 + X25519, native in
`age` >= 1.3 (`age-keygen -pq`, recipients `age1pq1…`). Your sessions stay
confidential if either primitive is broken (a future quantum computer, or a bug in
the young ML-KEM). PQ-only is intentionally not used — it would drop the classical
fallback with no security gain.
