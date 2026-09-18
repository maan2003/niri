//! Screen-lock auth daemon protocol and logic.
//!
//! Two kinds of peer talk to `drv-authd`, and the spawner hands it both, already connected
//! (see `drv_policy::wire`): the lock app sends `Verify` with the PIN, and the compositor sits on
//! its connection waiting for `Event::Unlock`. The lock app never unlocks anything itself; a
//! correct PIN makes the daemon push the unlock to the compositor.

use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use rand_core::OsRng;
use rustix::net::{RecvFlags, SendFlags, recv, send};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

pub const MAX_MSG: usize = 4096;

/// Free attempts before the delay kicks in.
pub const FREE_ATTEMPTS: u32 = 5;
const BASE_DELAY: Duration = Duration::from_secs(30);
const MAX_DELAY: Duration = Duration::from_secs(3600);

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Verify { secret: Vec<u8> },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Granted,
    Denied { retry_after_ms: u64 },
    Error(String),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum Event {
    /// The PIN was right: unlock for this long of inactivity.
    Unlock { idle_timeout_ms: u64 },
}

pub fn send_msg<T: Serialize>(sock: &OwnedFd, msg: &T) -> io::Result<()> {
    let buf = postcard::to_allocvec(msg).map_err(io::Error::other)?;
    if buf.len() > MAX_MSG {
        return Err(io::Error::other("message too large"));
    }
    let n = send(sock, &buf, SendFlags::NOSIGNAL)?;
    if n != buf.len() {
        return Err(io::Error::other("short send"));
    }
    Ok(())
}

pub fn recv_msg<T: for<'de> Deserialize<'de>>(sock: &OwnedFd) -> io::Result<T> {
    let mut buf = vec![0u8; MAX_MSG];
    let (n, _) = recv(sock, &mut buf, RecvFlags::empty())?;
    if n == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let msg = postcard::from_bytes(&buf[..n]).map_err(io::Error::other);
    buf.zeroize();
    msg
}

/// One `Verify` on the lock app's connection.
pub fn verify(sock: &OwnedFd, secret: &[u8]) -> io::Result<Response> {
    send_msg(
        sock,
        &Request::Verify {
            secret: secret.to_vec(),
        },
    )?;
    recv_msg(sock)
}

/// Wait for a daemon message (for the compositor's connection).
pub fn recv_event(sock: &OwnedFd) -> io::Result<Event> {
    recv_msg(sock)
}

/// On-disk PIN verifier and failure count.
pub struct Store {
    dir: PathBuf,
}

impl Store {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn write_private(&self, name: &str, data: &[u8]) -> io::Result<()> {
        let tmp = self.dir.join(format!(".{name}.tmp"));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        fs::rename(tmp, self.dir.join(name))
    }

    pub fn set_pin(&self, pin: &[u8]) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))?;
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(pin, &salt)
            .map_err(|err| io::Error::other(format!("hashing the PIN: {err}")))?;
        self.write_private("pin", hash.to_string().as_bytes())?;
        self.write_private("failures", b"0")
    }

    pub fn has_pin(&self) -> bool {
        self.dir.join("pin").exists()
    }

    fn check(&self, pin: &[u8]) -> io::Result<bool> {
        let mut s = String::new();
        fs::File::open(self.dir.join("pin"))?.read_to_string(&mut s)?;
        let hash = PasswordHash::new(s.trim())
            .map_err(|err| io::Error::other(format!("bad PIN file: {err}")))?;
        Ok(Argon2::default().verify_password(pin, &hash).is_ok())
    }

    fn failures(&self) -> u32 {
        fs::read_to_string(self.dir.join("failures"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Granted,
    Denied { retry_after: Duration },
}

/// Verification with an escalating delay after `FREE_ATTEMPTS` wrong PINs. The failure count
/// survives restarts; the delay itself restarts from the count on boot.
pub struct Auth {
    store: Store,
    failures: u32,
    retry_at: Option<Instant>,
}

pub fn delay_for(failures: u32) -> Duration {
    if failures <= FREE_ATTEMPTS {
        return Duration::ZERO;
    }
    let steps = (failures - FREE_ATTEMPTS - 1).min(8);
    (BASE_DELAY * (1 << steps)).min(MAX_DELAY)
}

impl Auth {
    pub fn new(store: Store) -> Self {
        let failures = store.failures();
        let delay = delay_for(failures);
        let retry_at = (!delay.is_zero()).then(|| Instant::now() + delay);
        Self {
            store,
            failures,
            retry_at,
        }
    }

    pub fn verify(&mut self, pin: &[u8]) -> io::Result<Outcome> {
        self.verify_at(pin, Instant::now())
    }

    fn verify_at(&mut self, pin: &[u8], now: Instant) -> io::Result<Outcome> {
        if let Some(at) = self.retry_at {
            if now < at {
                return Ok(Outcome::Denied {
                    retry_after: at - now,
                });
            }
        }
        if !self.store.has_pin() {
            return Err(io::Error::other("no PIN enrolled"));
        }
        if self.store.check(pin)? {
            self.failures = 0;
            self.retry_at = None;
            self.store.write_private("failures", b"0")?;
            return Ok(Outcome::Granted);
        }
        self.failures = self.failures.saturating_add(1);
        self.store
            .write_private("failures", self.failures.to_string().as_bytes())?;
        let delay = delay_for(self.failures);
        self.retry_at = (!delay.is_zero()).then(|| now + delay);
        Ok(Outcome::Denied { retry_after: delay })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_and_backoff() {
        let dir = std::env::temp_dir().join(format!("drv-auth-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = Store::new(&dir);
        store.set_pin(b"1234").unwrap();
        let mut auth = Auth::new(Store::new(&dir));
        let t0 = Instant::now();
        assert_eq!(auth.verify_at(b"1234", t0).unwrap(), Outcome::Granted);
        for _ in 0..FREE_ATTEMPTS {
            assert_eq!(
                auth.verify_at(b"0000", t0).unwrap(),
                Outcome::Denied {
                    retry_after: Duration::ZERO
                }
            );
        }
        // Sixth failure starts the delay; the right PIN is refused until it passes.
        assert_eq!(
            auth.verify_at(b"0000", t0).unwrap(),
            Outcome::Denied {
                retry_after: BASE_DELAY
            }
        );
        assert!(matches!(
            auth.verify_at(b"1234", t0).unwrap(),
            Outcome::Denied { .. }
        ));
        assert_eq!(
            auth.verify_at(b"1234", t0 + BASE_DELAY).unwrap(),
            Outcome::Granted
        );
        // The count survived on disk.
        auth.verify_at(b"0000", t0).unwrap();
        assert_eq!(Store::new(&dir).failures(), 1);
        assert_eq!(delay_for(FREE_ATTEMPTS + 20), MAX_DELAY);
        fs::remove_dir_all(&dir).unwrap();
    }
}
