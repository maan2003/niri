//! What the spawner hands a child that has peers: fd [`WIRE_FD`], a `SOCK_SEQPACKET`
//! socketpair on which the spawner pushes [`Attach`] messages, each carrying one fd. The child
//! never connects to anything; it is handed its peers, already connected, by the one process
//! that forked both sides. Nothing on the filesystem, no UID checks at the receiving end.

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, FromRawFd, OwnedFd};

use rustix::net::{
    RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use serde::{Deserialize, Serialize};

/// Where the spawner puts the wire; `WIRE_ENV` is set to say it is there.
pub const WIRE_FD: i32 = 3;
pub const WIRE_ENV: &str = "DRV_WIRE_FD";

/// What the fd riding along is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Attach {
    /// To the compositor or the lock app: a connection to `drv-authd`.
    Auth,
    /// To `drv-authd`: the compositor's connection (replaces the previous one).
    Compositor,
    /// To `drv-authd`: a lock app's connection.
    Verifier,
}

/// The child's end, if the spawner gave us one.
pub fn take() -> Option<OwnedFd> {
    std::env::var_os(WIRE_ENV)?;
    // SAFETY: the spawner put the wire on this fd and nothing else owns it.
    Some(unsafe { OwnedFd::from_raw_fd(WIRE_FD) })
}

pub fn send_attach(wire: impl AsFd, attach: Attach, fd: BorrowedFd<'_>) -> io::Result<()> {
    let payload = postcard::to_stdvec(&attach).map_err(|e| io::Error::other(e.to_string()))?;
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    let fds = [fd];
    if !control.push(SendAncillaryMessage::ScmRights(&fds)) {
        return Err(io::Error::other("no room for the fd"));
    }
    rustix::net::sendmsg(
        wire,
        &[IoSlice::new(&payload)],
        &mut control,
        SendFlags::NOSIGNAL,
    )?;
    Ok(())
}

/// The next attachment. A closed wire is `UnexpectedEof`; a message without its fd is refused.
pub fn recv_attach(wire: impl AsFd) -> io::Result<(Attach, OwnedFd)> {
    let mut buf = [0u8; 64];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let msg = rustix::net::recvmsg(
        wire,
        &mut [IoSliceMut::new(&mut buf)],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
    )?;
    let mut fds: Vec<OwnedFd> = control
        .drain()
        .filter_map(|m| match m {
            RecvAncillaryMessage::ScmRights(received) => Some(received),
            _ => None,
        })
        .flatten()
        .collect();
    if msg.bytes == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "wire closed"));
    }
    let attach: Attach = postcard::from_bytes(&buf[..msg.bytes])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    match (fds.pop(), fds.is_empty()) {
        (Some(fd), true) => Ok((attach, fd)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "attachment without exactly one fd",
        )),
    }
}

/// A connected `SOCK_SEQPACKET` pair, close-on-exec.
pub fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::net::socketpair(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::SEQPACKET,
        rustix::net::SocketFlags::CLOEXEC,
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_carries_its_fd() {
        let (a, b) = pair().unwrap();
        let (x, y) = pair().unwrap();
        send_attach(&a, Attach::Auth, x.as_fd()).unwrap();
        let (attach, fd) = recv_attach(&b).unwrap();
        assert_eq!(attach, Attach::Auth);
        // The received fd is x: what we write on y comes out of it.
        rustix::net::send(&y, b"hi", SendFlags::empty()).unwrap();
        let mut buf = [0u8; 8];
        let (n, _) = rustix::net::recv(&fd, &mut buf, RecvFlags::empty()).unwrap();
        assert_eq!(&buf[..n], b"hi");
        drop(a);
        assert_eq!(
            recv_attach(&b).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
