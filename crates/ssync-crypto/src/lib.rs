//! age encryption at rest (DECISIONS §7). Shells out to `age`/`age-keygen`
//! (>= 1.3, resolved from `PATH` when an identity is constructed) for native
//! post-quantum hybrid keys (`age-keygen -pq`, ML-KEM-768 + X25519). The
//! X25519-only Rust `age` crate backend stays disabled behind `rust-age`.

use std::ffi::OsStr;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::time::Instant;

#[cfg(feature = "rust-age")]
pub mod rust_age;

/// An age identity: the secret key string plus its recipient (public key),
/// optionally extended with peer recipients for multi-recipient encryption.
/// New identities are post-quantum hybrid (`AGE-SECRET-KEY-PQ-1…` / `age1pq1…`);
/// classical (`AGE-SECRET-KEY-1…` / `age1…`) keys are still accepted.
pub struct AgeIdentity {
    secret: String,
    recipient: String,
    extra_recipients: Vec<String>,
    commands: AgeCommands,
}

impl AgeIdentity {
    /// Generate a fresh post-quantum hybrid identity (`age-keygen -pq`).
    pub fn generate() -> Result<Self> {
        let commands = AgeCommands::resolve()?;
        let out = commands.keygen(["-pq"]).context("running age-keygen -pq")?;
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
            None => recipient_of(&commands, &secret)?,
        };
        Ok(Self {
            secret,
            recipient,
            extra_recipients: Vec::new(),
            commands,
        })
    }

    /// Build from an age identity: either a bare `AGE-SECRET-KEY[-PQ]-1…` line or
    /// a full `age-keygen` file (comment lines are ignored).
    pub fn from_secret_string(s: &str) -> Result<Self> {
        Self::from_secret_string_with_commands(s, AgeCommands::resolve()?)
    }

    fn from_secret_string_with_commands(s: &str, commands: AgeCommands) -> Result<Self> {
        let secret = s
            .lines()
            .map(str::trim)
            .find(|l| l.starts_with("AGE-SECRET-KEY-"))
            .ok_or_else(|| anyhow!("no age secret key found"))?
            .to_string();
        let recipient = recipient_of(&commands, &secret)?;
        Ok(Self {
            secret,
            recipient,
            extra_recipients: Vec::new(),
            commands,
        })
    }

    /// The secret key string. Handle as a secret; persist `0600`.
    pub fn to_secret_string(&self) -> String {
        self.secret.clone()
    }

    /// The recipient (public key) string.
    pub fn recipient_string(&self) -> String {
        self.recipient.clone()
    }

    /// Every recipient this identity encrypts to (own plus extras), sorted —
    /// the canonical form for detecting recipient-set changes.
    pub fn recipients(&self) -> Vec<String> {
        let mut all = self.extra_recipients.clone();
        all.push(self.recipient.clone());
        all.sort_unstable();
        all
    }

    /// Extend the encryption recipient set with peer recipients (their machines
    /// can then decrypt what this one publishes). Own recipient stays included;
    /// duplicates are dropped.
    pub fn add_recipients<I: IntoIterator<Item = String>>(&mut self, recipients: I) {
        for r in recipients {
            if r != self.recipient && !self.extra_recipients.contains(&r) {
                self.extra_recipients.push(r);
            }
        }
    }

    /// Encrypt `plaintext` to this identity's recipient plus any added peer
    /// recipients (binary age output).
    pub async fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut command = self.commands.age();
        command.args(["-e", "-r", &self.recipient]);
        for recipient in &self.extra_recipients {
            command.args(["-r", recipient]);
        }
        run(&mut command, plaintext, self.commands.inactivity_timeout)
            .await
            .context("age encrypt")
    }

    /// Decrypt age `ciphertext` with this identity.
    pub async fn decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let key = SecretFile::new(&self.secret)?;
        let mut command = self.commands.age();
        command.arg("-d").arg("-i").arg(&key.path);
        run(&mut command, ciphertext, self.commands.inactivity_timeout)
            .await
            .context("age decrypt (wrong identity?)")
    }
}

const INACTIVITY_TIMEOUT: Duration = Duration::from_secs(30);
const IO_CHUNK: usize = 64 * 1024;

struct AgeCommands {
    age: PathBuf,
    age_keygen: PathBuf,
    inactivity_timeout: Duration,
}

impl AgeCommands {
    fn resolve() -> Result<Self> {
        Ok(Self::new(
            resolve_executable("age")?,
            resolve_executable("age-keygen")?,
            INACTIVITY_TIMEOUT,
        ))
    }

    fn new(age: PathBuf, age_keygen: PathBuf, inactivity_timeout: Duration) -> Self {
        Self {
            age,
            age_keygen,
            inactivity_timeout,
        }
    }

    fn age(&self) -> Command {
        let mut command = Command::new(&self.age);
        command.env_clear();
        command
    }

    fn keygen<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = StdCommand::new(&self.age_keygen)
            .env_clear()
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("spawning {}", self.age_keygen.display()))?;
        if !output.status.success() {
            bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(output.stdout)
    }
}

fn resolve_executable(name: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").ok_or_else(|| anyhow!("PATH is not set"))?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        let candidate = if candidate.is_absolute() {
            candidate
        } else {
            std::env::current_dir()
                .context("resolving executable path")?
                .join(candidate)
        };
        let Ok(metadata) = candidate.metadata() else {
            continue;
        };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            return Ok(candidate);
        }
    }
    bail!("{name} not found on PATH")
}

fn recipient_of(commands: &AgeCommands, secret: &str) -> Result<String> {
    let key = SecretFile::new(secret)?;
    let out = commands
        .keygen(["-y".as_ref(), key.path.as_os_str()])
        .context("age-keygen -y")?;
    let recipient = String::from_utf8(out)
        .context("age-keygen -y output not utf-8")?
        .trim()
        .to_string();
    if recipient.is_empty() {
        bail!("age-keygen -y produced no recipient");
    }
    Ok(recipient)
}

async fn terminate(child: &mut Child) {
    let _ = child.start_kill();
    let _ = child.wait().await;
}

/// Run `command` with `input` on stdin. The timeout resets on every I/O event.
async fn run(command: &mut Command, input: &[u8], inactivity_timeout: Duration) -> Result<Vec<u8>> {
    let program = command.as_std().get_program().to_owned();
    let mut child = command
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning {program:?}"))?;
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut input_offset = 0;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_open = true;
    let mut stderr_open = true;
    let mut status = None;
    let mut stdout_chunk = vec![0; IO_CHUNK];
    let mut stderr_chunk = vec![0; IO_CHUNK];
    if input.is_empty() {
        stdin = None;
    }
    let idle = tokio::time::sleep(inactivity_timeout);
    tokio::pin!(idle);

    while status.is_none() || stdout_open || stderr_open {
        tokio::select! {
            result = async {
                stdin
                    .as_mut()
                    .expect("guarded stdin")
                    .write(&input[input_offset..])
                    .await
            }, if stdin.is_some() => {
                match result {
                    Ok(0) => {
                        terminate(&mut child).await;
                        bail!("age child closed stdin");
                    }
                    Ok(written) => {
                        input_offset += written;
                        if input_offset == input.len()
                            && let Some(mut pipe) = stdin.take()
                            && let Err(error) = pipe.shutdown().await
                        {
                            terminate(&mut child).await;
                            return Err(error).context("closing age stdin");
                        }
                    }
                    Err(error) => {
                        terminate(&mut child).await;
                        return Err(error).context("writing age stdin");
                    }
                }
                idle.as_mut().reset(Instant::now() + inactivity_timeout);
            }
            result = stdout.read(&mut stdout_chunk), if stdout_open => {
                match result {
                    Ok(0) => stdout_open = false,
                    Ok(read) => stdout_bytes.extend_from_slice(&stdout_chunk[..read]),
                    Err(error) => {
                        terminate(&mut child).await;
                        return Err(error).context("reading age stdout");
                    }
                }
                idle.as_mut().reset(Instant::now() + inactivity_timeout);
            }
            result = stderr.read(&mut stderr_chunk), if stderr_open => {
                match result {
                    Ok(0) => stderr_open = false,
                    Ok(read) => stderr_bytes.extend_from_slice(&stderr_chunk[..read]),
                    Err(error) => {
                        terminate(&mut child).await;
                        return Err(error).context("reading age stderr");
                    }
                }
                idle.as_mut().reset(Instant::now() + inactivity_timeout);
            }
            result = child.wait(), if status.is_none() => {
                match result {
                    Ok(exit) => status = Some(exit),
                    Err(error) => {
                        terminate(&mut child).await;
                        return Err(error).context("waiting for age child");
                    }
                }
                idle.as_mut().reset(Instant::now() + inactivity_timeout);
            }
            () = &mut idle => {
                terminate(&mut child).await;
                bail!("age child inactive for {}s", inactivity_timeout.as_secs_f64());
            }
        }
    }

    if !status.expect("loop waits for status").success() {
        bail!("{}", String::from_utf8_lossy(&stderr_bytes).trim());
    }
    Ok(stdout_bytes)
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
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::{Duration, Instant};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ssync-crypto-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    fn identity_with_commands(age: PathBuf, age_keygen: PathBuf, timeout: Duration) -> AgeIdentity {
        AgeIdentity::from_secret_string_with_commands(
            "AGE-SECRET-KEY-1-TEST",
            AgeCommands::new(age, age_keygen, timeout),
        )
        .unwrap()
    }

    #[test]
    fn injected_keygen_path_is_used() {
        let dir = scratch("injected-keygen");
        let age = script(&dir, "age", "exit 1");
        let age_keygen = script(&dir, "age-keygen", "printf 'age1injected\\n'");
        let id = identity_with_commands(age, age_keygen, Duration::from_secs(1));

        assert_eq!(id.recipient_string(), "age1injected");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn resolved_commands_are_absolute() {
        let commands = AgeCommands::resolve().unwrap();
        assert!(commands.age.is_absolute());
        assert!(commands.age_keygen.is_absolute());
    }

    #[tokio::test]
    async fn age_child_receives_empty_environment() {
        let dir = scratch("empty-environment");
        let cat = resolve_executable("cat").unwrap();
        let age = script(
            &dir,
            "age",
            &format!(
                "if [ -s /proc/self/environ ]; then exit 9; fi\n'{}' >/dev/null\nprintf clean",
                cat.display()
            ),
        );
        let age_keygen = script(&dir, "age-keygen", "printf 'age1injected\\n'");
        let id = identity_with_commands(age, age_keygen, Duration::from_secs(1));

        assert_eq!(id.encrypt(b"plaintext").await.unwrap(), b"clean");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn inactivity_timeout_kills_and_reaps_child() {
        let dir = scratch("inactivity");
        let pid_file = dir.join("pid");
        let sleep = resolve_executable("sleep").unwrap();
        let age = script(
            &dir,
            "age",
            &format!(
                "printf '%s' $$ > '{}'\nprintf ready\nexec '{}' 60",
                pid_file.display(),
                sleep.display()
            ),
        );
        let age_keygen = script(&dir, "age-keygen", "printf 'age1injected\\n'");
        let id = identity_with_commands(age, age_keygen, Duration::from_millis(500));

        let started = Instant::now();
        let error = id.encrypt(b"plaintext").await.unwrap_err();
        assert!(
            format!("{error:#}").contains("inactive"),
            "unexpected error: {error:#}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let pid = pid.trim();
        assert!(!pid.is_empty(), "child did not record its pid");
        assert!(
            !Path::new("/proc").join(pid).exists(),
            "child was not reaped"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn decrypt_identity_file_lives_for_one_operation() {
        let dir = scratch("identity-lifetime");
        let report = dir.join("identity-path");
        let cat = resolve_executable("cat").unwrap();
        let age = script(
            &dir,
            "age",
            &format!(
                "while [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = -i ]; then key=$2; shift 2; else shift; fi\ndone\n[ -f \"$key\" ] || exit 9\nprintf '%s' \"$key\" > '{}'\n'{}' >/dev/null\nprintf plain",
                report.display(),
                cat.display()
            ),
        );
        let age_keygen = script(&dir, "age-keygen", "printf 'age1injected\\n'");
        let id = identity_with_commands(age, age_keygen, Duration::from_secs(1));

        assert_eq!(id.decrypt(b"ciphertext").await.unwrap(), b"plain");
        let identity_path = std::fs::read_to_string(report).unwrap();
        assert!(!Path::new(&identity_path).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn round_trip_is_byte_identical() {
        let id = AgeIdentity::generate().unwrap();
        assert!(
            id.recipient_string().starts_with("age1pq1"),
            "expected PQ recipient"
        );
        let plaintext = b"{\"type\":\"session\",\"version\":3}\n{\"secret\":\"sk-abc\"}\n";
        let ct = id.encrypt(plaintext).await.unwrap();
        assert_ne!(&ct[..], &plaintext[..]);
        let pt = id.decrypt(&ct).await.unwrap();
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

    #[tokio::test]
    async fn wrong_identity_cannot_decrypt() {
        let a = AgeIdentity::generate().unwrap();
        let b = AgeIdentity::generate().unwrap();
        let ct = a.encrypt(b"hello").await.unwrap();
        assert!(b.decrypt(&ct).await.is_err());
    }

    #[tokio::test]
    async fn extra_recipients_can_decrypt_and_self_stays_included() {
        let a = AgeIdentity::generate().unwrap();
        let b = AgeIdentity::generate().unwrap();
        let c = AgeIdentity::generate().unwrap();
        let mut sender = AgeIdentity::from_secret_string(&a.to_secret_string()).unwrap();
        // duplicate of self plus b: dedup must not break encryption
        sender.add_recipients([a.recipient_string(), b.recipient_string()]);
        let ct = sender.encrypt(b"shared session").await.unwrap();
        assert_eq!(a.decrypt(&ct).await.unwrap(), b"shared session");
        assert_eq!(b.decrypt(&ct).await.unwrap(), b"shared session");
        assert!(
            c.decrypt(&ct).await.is_err(),
            "non-recipient must not decrypt"
        );
    }
}
