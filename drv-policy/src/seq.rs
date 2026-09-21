//! The one transport between our pieces: `SOCK_SEQPACKET`, one postcard message per
//! datagram, fds by `SCM_RIGHTS`. A closed peer is `UnexpectedEof`; a message that does not
//! fit is an error, never a truncated read.

use std::io::{self, IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::net::{
    AddressFamily, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags, SocketFlags, SocketType,
};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// Datagrams larger than this are refused.
pub const MAX_MSG: usize = 64 * 1024;
/// Fds per message.
pub const MAX_FDS: usize = 16;

pub fn send<T: Serialize>(sock: impl AsFd, msg: &T, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    let payload = postcard::to_stdvec(msg).map_err(|e| io::Error::other(e.to_string()))?;
    if payload.len() > MAX_MSG {
        return Err(io::Error::other("message too large"));
    }
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
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

/// One datagram and the fds that came with it.
pub fn recv<T: DeserializeOwned>(sock: impl AsFd) -> io::Result<(T, Vec<OwnedFd>)> {
    let (bytes, fds) = recv_bytes(sock)?;
    Ok((decode(&bytes)?, fds))
}

/// A datagram's bytes, undecoded.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    postcard::from_bytes(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// One datagram as it came: for a receiver that must not decode it itself (the forker's
/// parent hands the bytes to the child).
pub fn recv_bytes(sock: impl AsFd) -> io::Result<(Vec<u8>, Vec<OwnedFd>)> {
    let mut buf = vec![0u8; MAX_MSG];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(MAX_FDS))];
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
    buf.truncate(msg.bytes);
    Ok((buf, fds))
}

/// A connected pair, close-on-exec.
pub fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::net::socketpair(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_with_fd_and_eof() {
        let (a, b) = pair().unwrap();
        let (x, _y) = pair().unwrap();
        send(&a, &(7u32, "seven"), &[x.as_fd()]).unwrap();
        let (msg, fds): ((u32, String), _) = recv(&b).unwrap();
        assert_eq!(msg, (7, "seven".to_owned()));
        assert_eq!(fds.len(), 1);
        drop(a);
        assert_eq!(
            recv::<u32>(&b).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
