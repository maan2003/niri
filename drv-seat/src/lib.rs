//! Seat protocol: the compositor gets its DRM and evdev fds from a root daemon instead of
//! holding device groups itself. One `SOCK_SEQPACKET` socket, requests in lock step, a reply
//! may carry one fd. Session enable/disable arrive on a second socket handed over with
//! `Hello`, so they never interleave with replies.

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::Path;

use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketAddrUnix, SocketFlags, SocketType,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change; the daemon answers `Hello` with its own version.
pub const VERSION: u32 = 1;
pub const SOCKET_ENV: &str = "DRV_SEAT_SOCKET";
pub const DEFAULT_SOCKET: &str = "/run/drv-seat/seat.sock";
/// Datagrams larger than this are refused.
pub const MAX_MSG: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Hello { version: u32 },
    /// Open a device node; only what [`is_allowed_device`] accepts.
    Open { path: String },
    /// Close a device opened here. The client drops its own fd itself.
    Close { id: u32 },
    SwitchVt { vt: i32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// Carries the events socket as its fd.
    Hello {
        version: u32,
        seat: String,
        active: bool,
    },
    /// Carries the device fd.
    Opened { id: u32 },
    Done,
    Error(String),
}

/// Session state changes, on the events socket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// Devices are live (again).
    Enable,
    /// The seat went elsewhere (VT switch): devices are revoked until `Enable`.
    Disable,
}

/// Only the seat's display and input nodes, by their canonical names: no render nodes, no
/// symlinks, nothing else under `/dev`.
pub fn is_allowed_device(path: &str) -> bool {
    let number = path
        .strip_prefix("/dev/dri/card")
        .or_else(|| path.strip_prefix("/dev/input/event"));
    matches!(number, Some(n) if !n.is_empty() && n.len() <= 4 && n.bytes().all(|b| b.is_ascii_digit()))
}

pub fn send<T: Serialize>(sock: impl AsFd, msg: &T, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    let payload = postcard::to_stdvec(msg).map_err(|e| io::Error::other(e.to_string()))?;
    if payload.len() > MAX_MSG {
        return Err(io::Error::other("message too large"));
    }
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(fds)) {
        return Err(io::Error::other("too many fds"));
    }
    let sent = rustix::net::sendmsg(
        sock,
        &[IoSlice::new(&payload)],
        &mut control,
        SendFlags::NOSIGNAL,
    )?;
    if sent != payload.len() {
        return Err(io::Error::other("short send"));
    }
    Ok(())
}

/// One datagram and the fds that came with it. A closed peer is `UnexpectedEof`.
pub fn recv<T: DeserializeOwned>(sock: impl AsFd) -> io::Result<(T, Vec<OwnedFd>)> {
    let mut buf = [0u8; MAX_MSG];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let msg = rustix::net::recvmsg(
        sock,
        &mut [IoSliceMut::new(&mut buf)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    let mut fds = Vec::new();
    for m in control.drain() {
        if let RecvAncillaryMessage::ScmRights(received) = m {
            fds.extend(received);
        }
    }
    if msg.bytes == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
    }
    if msg.flags.contains(rustix::net::ReturnFlags::TRUNC) {
        return Err(io::Error::other("message too large"));
    }
    let value = postcard::from_bytes(&buf[..msg.bytes])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    Ok((value, fds))
}

fn seqpacket() -> io::Result<OwnedFd> {
    Ok(rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?)
}

/// A pair of connected event sockets.
pub fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?)
}

pub fn listen(path: &Path) -> io::Result<OwnedFd> {
    let _ = std::fs::remove_file(path);
    let sock = seqpacket()?;
    rustix::net::bind(&sock, &SocketAddrUnix::new(path)?)?;
    rustix::net::listen(&sock, 4)?;
    Ok(sock)
}

pub fn connect(path: &Path) -> io::Result<OwnedFd> {
    let sock = seqpacket()?;
    rustix::net::connect(&sock, &SocketAddrUnix::new(path)?)?;
    Ok(sock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_allowlist() {
        assert!(is_allowed_device("/dev/dri/card0"));
        assert!(is_allowed_device("/dev/input/event12"));
        assert!(!is_allowed_device("/dev/dri/renderD128"));
        assert!(!is_allowed_device("/dev/dri/card"));
        assert!(!is_allowed_device("/dev/dri/card0/../renderD128"));
        assert!(!is_allowed_device("/dev/input/mice"));
        assert!(!is_allowed_device("/dev/tty0"));
    }

    #[test]
    fn roundtrip_with_fd() {
        let (a, b) = pair().unwrap();
        let (x, _y) = pair().unwrap();
        send(&a, &Response::Opened { id: 7 }, &[x.as_fd()]).unwrap();
        let (msg, fds): (Response, _) = recv(&b).unwrap();
        assert_eq!(msg, Response::Opened { id: 7 });
        assert_eq!(fds.len(), 1);
        drop(a);
        assert_eq!(
            recv::<Response>(&b).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
