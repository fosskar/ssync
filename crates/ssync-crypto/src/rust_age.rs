//! DISABLED in-process backend, kept for when the Rust `age` crate gains ML-KEM
//! support. Enable with the `rust-age` cargo feature. It is X25519-only, so it
//! cannot read/write the post-quantum hybrid keys the default (CLI) backend uses.
//! Same `AgeIdentity` API as the default backend so switching back is a one-liner.

use std::io::{Read, Write};
use std::iter;

use age::secrecy::ExposeSecret;
use age::x25519;
use anyhow::{Context, Result};

/// X25519 age identity. Holds the private key; persist `0600`.
pub struct AgeIdentity {
    identity: x25519::Identity,
}

impl AgeIdentity {
    pub fn generate() -> Self {
        Self {
            identity: x25519::Identity::generate(),
        }
    }

    pub fn from_secret_string(s: &str) -> Result<Self> {
        let identity = s
            .parse::<x25519::Identity>()
            .map_err(|e| anyhow::anyhow!("invalid age identity: {e}"))?;
        Ok(Self { identity })
    }

    pub fn to_secret_string(&self) -> String {
        self.identity.to_string().expose_secret().to_string()
    }

    pub fn recipient_string(&self) -> String {
        self.identity.to_public().to_string()
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let recipient = self.identity.to_public();
        let encryptor =
            age::Encryptor::with_recipients(iter::once(&recipient as &dyn age::Recipient))
                .context("building age encryptor")?;
        let mut out = Vec::new();
        let mut writer = encryptor
            .wrap_output(&mut out)
            .context("starting age encryption")?;
        writer.write_all(plaintext).context("writing plaintext")?;
        writer.finish().context("finishing age encryption")?;
        Ok(out)
    }

    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let decryptor = age::Decryptor::new(ciphertext).context("reading age header")?;
        let mut reader = decryptor
            .decrypt(iter::once(&self.identity as &dyn age::Identity))
            .context("decrypting (wrong identity?)")?;
        let mut out = Vec::new();
        reader.read_to_end(&mut out).context("reading plaintext")?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_byte_identical() {
        let id = AgeIdentity::generate();
        let plaintext = b"{\"type\":\"session\",\"version\":3}\n{\"secret\":\"sk-abc\"}\n";
        let ct = id.encrypt(plaintext).unwrap();
        assert_ne!(
            &ct[..],
            &plaintext[..],
            "ciphertext must differ from plaintext"
        );
        let pt = id.decrypt(&ct).unwrap();
        assert_eq!(&pt[..], &plaintext[..]);
    }

    #[test]
    fn secret_string_round_trips() {
        let id = AgeIdentity::generate();
        let s = id.to_secret_string();
        let id2 = AgeIdentity::from_secret_string(&s).unwrap();
        assert_eq!(id.recipient_string(), id2.recipient_string());
    }

    #[test]
    fn wrong_identity_cannot_decrypt() {
        let a = AgeIdentity::generate();
        let b = AgeIdentity::generate();
        let ct = a.encrypt(b"hello").unwrap();
        assert!(b.decrypt(&ct).is_err());
    }
}
