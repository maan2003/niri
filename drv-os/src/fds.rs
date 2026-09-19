//! Named fds at startup: the one way a process under drv-supervisor (or systemd) gets its
//! peers. systemd's convention, so either can start a piece the same way: `LISTEN_FDS` fds
//! from 3 up, `LISTEN_FDNAMES` their names, colon-separated, and `LISTEN_PID`, when set, the
//! pid they are for. Everything here is a unix socket, checked to be one before it is handed
//! out; nothing is found on the filesystem and nothing is connected at runtime.
//!
//! The supervisor cannot set `LISTEN_PID` (std swaps the environment in after `pre_exec`, so
//! the child cannot write its pid into it), and does not need to: its children get an exact
//! environment and pass nothing on. A `LISTEN_PID` that is present and not ours is refused.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::{BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;

use rustix::net::{AddressFamily, SocketFlags, SocketType};

/// The first passed fd, as systemd defines it.
pub const FIRST_FD: i32 = 3;
pub const ENV_FDS: &str = "LISTEN_FDS";
pub const ENV_NAMES: &str = "LISTEN_FDNAMES";
pub const ENV_PID: &str = "LISTEN_PID";
/// systemd's newer pidfd guard; unset with the rest so nothing downstream sees a stale one.
pub const ENV_PIDFDID: &str = "LISTEN_PIDFDID";

/// What a connected socket carries: one message per datagram, or a byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Stream,
    SeqPacket,
}

impl Kind {
    fn socket_type(self) -> SocketType {
        match self {
            Kind::Stream => SocketType::STREAM,
            Kind::SeqPacket => SocketType::SEQPACKET,
        }
    }
}

/// The fds we were started with, by name, each taken once.
#[derive(Debug, Default)]
pub struct Fds(BTreeMap<String, OwnedFd>);

/// Reads and clears the `LISTEN_*` environment. Every fd named must exist and be a socket;
/// the names must be there, one per fd, distinct. An empty result means we were started
/// without fds (nothing to unset); a malformed handoff is an error, never a partial result.
pub fn take() -> io::Result<Fds> {
    let result = take_inner();
    for var in [ENV_PID, ENV_PIDFDID, ENV_FDS, ENV_NAMES] {
        std::env::remove_var(var);
    }
    result
}

fn take_inner() -> io::Result<Fds> {
    let Some(count) = std::env::var_os(ENV_FDS) else {
        return Ok(Fds::default());
    };
    let count: i32 = count
        .to_str()
        .and_then(|s| s.parse().ok())
        .filter(|n| (1..=1024).contains(n))
        .ok_or_else(|| invalid(format!("{ENV_FDS}={count:?} is not a count")))?;
    if let Some(pid) = std::env::var_os(ENV_PID) {
        let ours = std::process::id();
        let theirs: Option<u32> = pid.to_str().and_then(|s| s.parse().ok());
        if theirs != Some(ours) {
            return Err(invalid(format!(
                "{ENV_PID}={pid:?} but we are pid {ours}: these fds are not ours"
            )));
        }
    }
    let names = std::env::var_os(ENV_NAMES)
        .ok_or_else(|| invalid(format!("{ENV_FDS} without {ENV_NAMES}: unnamed fds are refused")))?;
    let names = names
        .to_str()
        .ok_or_else(|| invalid(format!("{ENV_NAMES} is not UTF-8")))?;
    let names: Vec<&str> = names.split(':').collect();
    if names.len() != count as usize {
        return Err(invalid(format!(
            "{} names for {count} fds",
            names.len()
        )));
    }
    let mut fds = BTreeMap::new();
    for (i, name) in names.iter().enumerate() {
        let raw = FIRST_FD + i as i32;
        if name.is_empty() {
            return Err(invalid(format!("fd {raw} has no name")));
        }
        // SAFETY: the process that started us put a live fd on this number and it is ours.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        rustix::fs::fstat(&fd).map_err(|e| invalid(format!("fd {raw} ({name}): {e}")))?;
        rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
        if fds.insert((*name).to_owned(), fd).is_some() {
            return Err(invalid(format!("two fds named {name:?}")));
        }
    }
    Ok(Fds(fds))
}

impl Fds {
    /// A connected unix socket of `kind`. Whatever else it turns out to be is refused, and
    /// the fd is closed.
    pub fn socket(&mut self, name: &str, kind: Kind) -> io::Result<OwnedFd> {
        let fd = self.remove(name)?;
        check_unix(&fd, name, Some(kind.socket_type()), false)?;
        Ok(fd)
    }

    /// Whatever fd was handed over under `name`, for what is not a socket (a device the
    /// supervisor opened for us).
    pub fn file(&mut self, name: &str) -> io::Result<OwnedFd> {
        self.remove(name)
    }

    /// A listening unix stream socket.
    pub fn listener(&mut self, name: &str) -> io::Result<UnixListener> {
        self.listener_of(name, Kind::Stream)
    }

    /// A listening unix socket of `kind`. `std` accepts on it whatever the kind is.
    pub fn listener_of(&mut self, name: &str, kind: Kind) -> io::Result<UnixListener> {
        let fd = self.remove(name)?;
        check_unix(&fd, name, Some(kind.socket_type()), true)?;
        Ok(UnixListener::from(fd))
    }

    fn remove(&mut self, name: &str) -> io::Result<OwnedFd> {
        self.0.remove(name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no fd named {name:?} was handed to us (we run under drv-supervisor)"),
            )
        })
    }

    /// What was handed over and not taken: a mismatch between the two sides worth logging.
    pub fn leftover(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

fn check_unix(
    fd: &OwnedFd,
    name: &str,
    kind: Option<SocketType>,
    listening: bool,
) -> io::Result<()> {
    use rustix::net::sockopt;
    let domain = sockopt::socket_domain(fd).map_err(|e| invalid(format!("{name}: SO_DOMAIN: {e}")))?;
    if domain != AddressFamily::UNIX {
        return Err(invalid(format!("{name} is not a unix socket")));
    }
    if let Some(kind) = kind {
        let actual = sockopt::socket_type(fd).map_err(|e| invalid(format!("{name}: SO_TYPE: {e}")))?;
        if actual != kind {
            return Err(invalid(format!("{name} is a {actual:?} socket, expected {kind:?}")));
        }
    }
    let accepting = sockopt::socket_acceptconn(fd).map_err(|e| invalid(format!("{name}: SO_ACCEPTCONN: {e}")))?;
    if accepting != listening {
        return Err(invalid(if listening {
            format!("{name} is not listening")
        } else {
            format!("{name} is a listening socket, expected a connection")
        }));
    }
    Ok(())
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// The giving side: `(name, fd)` pairs become the child's fds 3.. in order, and the two
/// environment variables that say so. The caller `dup2`s them into place between fork and
/// exec (see drv-supervisor).
pub fn handoff<'a>(fds: &[(&str, BorrowedFd<'a>)]) -> io::Result<(Vec<(String, String)>, Vec<(i32, BorrowedFd<'a>)>)> {
    let mut names = Vec::with_capacity(fds.len());
    let mut placed = Vec::with_capacity(fds.len());
    for (i, (name, fd)) in fds.iter().enumerate() {
        if name.is_empty() || name.contains(':') {
            return Err(invalid(format!("bad fd name {name:?}")));
        }
        if names.contains(name) {
            return Err(invalid(format!("fd name {name:?} given twice")));
        }
        names.push(*name);
        placed.push((FIRST_FD + i as i32, *fd));
    }
    let env = vec![
        (ENV_FDS.to_owned(), fds.len().to_string()),
        (ENV_NAMES.to_owned(), names.join(":")),
    ];
    Ok((env, placed))
}

/// A connected `SOCK_SEQPACKET` pair, close-on-exec: one message per datagram.
pub fn seqpacket_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?)
}

/// A connected `SOCK_STREAM` pair, close-on-exec: for byte-stream protocols (Wayland, `rpc`).
pub fn stream_pair() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;

    use super::*;

    #[test]
    fn pairs_are_what_they_say() {
        let (a, _b) = stream_pair().unwrap();
        check_unix(&a, "a", Some(SocketType::STREAM), false).unwrap();
        assert!(check_unix(&a, "a", Some(SocketType::SEQPACKET), false).is_err());
        assert!(check_unix(&a, "a", Some(SocketType::STREAM), true).is_err());
        let (c, _d) = seqpacket_pair().unwrap();
        check_unix(&c, "c", Some(SocketType::SEQPACKET), false).unwrap();
    }

    #[test]
    fn handoff_numbers_from_three_and_names_in_order() {
        let (a, b) = stream_pair().unwrap();
        let (env, placed) = handoff(&[("wire", a.as_fd()), ("gpu", b.as_fd())]).unwrap();
        assert_eq!(placed[0].0, 3);
        assert_eq!(placed[1].0, 4);
        assert!(env.contains(&(ENV_FDS.to_owned(), "2".to_owned())));
        assert!(env.contains(&(ENV_NAMES.to_owned(), "wire:gpu".to_owned())));
        assert!(handoff(&[("a", a.as_fd()), ("a", b.as_fd())]).is_err());
        assert!(handoff(&[("a:b", a.as_fd())]).is_err());
    }
}
