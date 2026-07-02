//! age encryption at rest (DECISIONS §7). Shells out to `age`/`age-keygen`
//! (>= 1.3, on `PATH`) for native post-quantum hybrid keys (`age-keygen -pq`,
//! ML-KEM-768 + X25519). The X25519-only Rust `age` crate backend is kept,
//! disabled, in [`rust_age`] (feature `rust-age`) for when it gains ML-KEM.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};

#[cfg(feature = "rust-age")]
pub mod rust_age;

/// A shared age identity: the secret key string plus its recipient (public key).
/// New identities are post-quantum hybrid (`AGE-SECRET-KEY-PQ-1…` / `age1pq1…`);
/// classical (`AGE-SECRET-KEY-1…` / `age1…`) keys are still accepted.
pub struct AgeIdentity {
    secret: String,
    recipient: String,
}

impl AgeIdentity {
    /// Generate a fresh post-quantum hybrid identity (`age-keygen -pq`).
    pub fn generate() -> Result<Self> {
        let out =
            run(Command::new("age-keygen").arg("-pq"), &[]).context("running age-keygen -pq")?;
        let text = String::from_utf8(out).context("age-keygen output not utf-8")?;
        let mut secret = None;
        let mut recipient = None;
        for line in text.lines() {
            let line = line.trim();
            if let Some(pk) = line.strip_prefix("# public key: ") {
                recipient = Some(pk.trim().to_string());
            } else if line.starts_with("AGE-SECRET-KEY-") {
                secret = Some(line.to_string());
            }
        }
        let secret = secret.ok_or_else(|| anyhow!("age-keygen produced no secret key"))?;
        let recipient = match recipient {
            Some(r) => r,
            None => recipient_of(&secret)?,
        };
        Ok(Self { secret, recipient })
    }

    /// Build from an age identity: either a bare `AGE-SECRET-KEY[-PQ]-1…` line or
    /// a full `age-keygen` file (comment lines are ignored).
    pub fn from_secret_string(s: &str) -> Result<Self> {
        let secret = s
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("AGE-SECRET-KEY-"))
            .ok_or_else(|| anyhow!("no age secret key found"))?
            .to_string();
        let recipient = recipient_of(&secret)?;
        Ok(Self { secret, recipient })
    }

    /// The secret key string. Handle as a secret; persist `0600`.
    pub fn to_secret_string(&self) -> String {
        self.secret.clone()
    }

    /// The recipient (public key) string.
    pub fn recipient_string(&self) -> String {
        self.recipient.clone()
    }

    /// Encrypt `plaintext` to this identity's recipient (binary age output).
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        run(
            Command::new("age").args(["-e", "-r", &self.recipient]),
            plaintext,
        )
        .context("age encrypt")
    }

    /// Decrypt age `ciphertext` with this identity.
    pub fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let key = SecretFile::new(&self.secret)?;
        run(
            Command::new("age").arg("-d").arg("-i").arg(&key.path),
            ciphertext,
        )
        .context("age decrypt (wrong identity?)")
    }
}

fn recipient_of(secret: &str) -> Result<String> {
    let key = SecretFile::new(secret)?;
    let out =
        run(Command::new("age-keygen").arg("-y").arg(&key.path), &[]).context("age-keygen -y")?;
    let recipient = String::from_utf8(out)
        .context("age-keygen -y output not utf-8")?
        .trim()
        .to_string();
    if recipient.is_empty() {
        bail!("age-keygen -y produced no recipient");
    }
    Ok(recipient)
}

/// Run `cmd` with `input` on stdin, returning stdout; errors carry stderr.
fn run(cmd: &mut Command, input: &[u8]) -> Result<Vec<u8>> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {:?}", cmd.get_program()))?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let output = child.wait_with_output()?;
    let _ = writer.join();
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output.stdout)
}

/// `0600` temp file holding a secret key, removed on drop (age wants `-i FILE`).
/// Lives in `$XDG_RUNTIME_DIR` when available (tmpfs, user-private `0700`) so
/// the key never touches persistent storage; created with `create_new` +
/// `mode(0o600)` atomically, so there is no permission window and a
/// pre-existing path (symlink planted by another user) is never followed.
struct SecretFile {
    path: PathBuf,
}

impl SecretFile {
    fn new(contents: &str) -> Result<Self> {
        let dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|p| p.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let mut last_err = None;
        for _ in 0..16 {
            let path = dir.join(format!("ssync-age-{}-{}", std::process::id(), nonce()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
            {
                Ok(mut f) => {
                    f.write_all(contents.as_bytes())?;
                    f.write_all(b"\n")?;
                    return Ok(Self { path });
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(
            anyhow::Error::new(last_err.expect("attempted at least once"))
                .context(format!("creating secret file in {}", dir.display())),
        )
    }
}

impl Drop for SecretFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_is_byte_identical() {
        let id = AgeIdentity::generate().unwrap();
        assert!(
            id.recipient_string().starts_with("age1pq1"),
            "expected PQ recipient"
        );
        let plaintext = b"{\"type\":\"session\",\"version\":3}\n{\"secret\":\"sk-abc\"}\n";
        let ct = id.encrypt(plaintext).unwrap();
        assert_ne!(&ct[..], &plaintext[..]);
        let pt = id.decrypt(&ct).unwrap();
        assert_eq!(&pt[..], &plaintext[..]);
    }

    #[test]
    fn secret_string_round_trips() {
        let id = AgeIdentity::generate().unwrap();
        let id2 = AgeIdentity::from_secret_string(&id.to_secret_string()).unwrap();
        assert_eq!(id.recipient_string(), id2.recipient_string());
    }

    #[test]
    fn parses_full_age_keygen_file() {
        let id = AgeIdentity::generate().unwrap();
        let file = format!(
            "# created: 2026\n# public key: {}\n{}\n",
            id.recipient_string(),
            id.to_secret_string()
        );
        let id2 = AgeIdentity::from_secret_string(&file).unwrap();
        assert_eq!(id.recipient_string(), id2.recipient_string());
    }

    #[test]
    fn wrong_identity_cannot_decrypt() {
        let a = AgeIdentity::generate().unwrap();
        let b = AgeIdentity::generate().unwrap();
        let ct = a.encrypt(b"hello").unwrap();
        assert!(b.decrypt(&ct).is_err());
    }
}
