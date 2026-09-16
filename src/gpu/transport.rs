//! Length-prefixed postcard frames over a Unix stream socket, with fds
//! attached via SCM_RIGHTS.
//!
//! Fds are attached to the `sendmsg` carrying the frame's first byte, so on the
//! receiving side they always arrive no later than the frame that references
//! them. Received fds go into a FIFO and each decoded frame takes the number
//! it declared in its header.

use std::collections::VecDeque;
use std::io::{self, IoSlice, IoSliceMut, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;

use rustix::net::{
    recvmsg, sendmsg, RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
    SendAncillaryMessage, SendFlags,
};
use serde::de::DeserializeOwned;
use serde::Serialize;

/// Upper bound on a single frame body. Scenes and screenshots fit comfortably.
const MAX_BODY: usize = 256 * 1024 * 1024;
const MAX_FDS_PER_FRAME: usize = 64;
const HEADER_LEN: usize = 8;

pub struct Channel {
    stream: UnixStream,
    fds: VecDeque<OwnedFd>,
}

impl Channel {
    pub fn new(fd: OwnedFd) -> Self {
        Self {
            stream: UnixStream::from(fd),
            fds: VecDeque::new(),
        }
    }

    pub fn pair() -> io::Result<(Channel, Channel)> {
        let (a, b) = UnixStream::pair()?;
        Ok((Channel::new(a.into()), Channel::new(b.into())))
    }

    pub fn send<T: Serialize>(&mut self, msg: &T, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
        if fds.len() > MAX_FDS_PER_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "too many fds"));
        }
        let body = postcard::to_stdvec(msg).map_err(|e| io::Error::other(e.to_string()))?;
        if body.len() > MAX_BODY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "frame too large",
            ));
        }
        let mut frame = Vec::with_capacity(HEADER_LEN + body.len());
        frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
        frame.extend_from_slice(&(fds.len() as u32).to_le_bytes());
        frame.extend_from_slice(&body);

        let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(64))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        if !fds.is_empty() && !control.push(SendAncillaryMessage::ScmRights(fds)) {
            return Err(io::Error::other("cannot attach fds"));
        }
        let sent = sendmsg(
            self.stream.as_fd(),
            &[IoSlice::new(&frame)],
            &mut control,
            SendFlags::NOSIGNAL,
        )?;
        // Fds went with the first byte; the rest is plain stream data.
        self.stream.write_all(&frame[sent..])
    }

    /// Receives one frame. `UnexpectedEof` means the peer closed the socket.
    pub fn recv<T: DeserializeOwned>(&mut self) -> io::Result<(T, Vec<OwnedFd>)> {
        let mut header = [0u8; HEADER_LEN];
        self.read_exact_with_fds(&mut header)?;
        let len = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
        let num_fds = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        if len > MAX_BODY || num_fds > MAX_FDS_PER_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bad frame header",
            ));
        }
        let mut body = vec![0u8; len];
        self.read_exact_with_fds(&mut body)?;
        if self.fds.len() < num_fds {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame declared more fds than were received",
            ));
        }
        let fds = self.fds.drain(..num_fds).collect();
        let msg = postcard::from_bytes(&body)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        Ok((msg, fds))
    }

    fn read_exact_with_fds(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let mut filled = 0;
        while filled < buf.len() {
            let mut space = [MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(64))];
            let mut control = RecvAncillaryBuffer::new(&mut space);
            let msg = recvmsg(
                self.stream.as_fd(),
                &mut [IoSliceMut::new(&mut buf[filled..])],
                &mut control,
                RecvFlags::CMSG_CLOEXEC,
            );
            let n = match msg {
                Ok(msg) => msg.bytes,
                Err(rustix::io::Errno::INTR) => continue,
                Err(err) => return Err(err.into()),
            };
            for m in control.drain() {
                if let RecvAncillaryMessage::ScmRights(fds) = m {
                    self.fds.extend(fds);
                }
            }
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed"));
            }
            filled += n;
        }
        Ok(())
    }

    pub fn as_fd(&self) -> BorrowedFd<'_> {
        self.stream.as_fd()
    }

    pub fn into_fd(self) -> OwnedFd {
        self.stream.into()
    }
}

// Keep the unused-import lint quiet for Read on platforms where write_all is enough.
#[allow(dead_code)]
fn _assert_read(s: &mut UnixStream, b: &mut [u8]) -> io::Result<usize> {
    s.read(b)
}
