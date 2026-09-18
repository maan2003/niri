//! The channel between drv-appd and drv-forker: a socketpair the supervisor made before
//! forking both, so nothing else can reach it. Requests and replies in lock step on
//! [`seq`](crate::seq); a launch can carry fds for the child (the lock screen's wire). The
//! forker knows nothing about apps: what it gets here is what it does.

use std::io;
use std::os::fd::{BorrowedFd, OwnedFd};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::seq;

/// Where the supervisor puts the channel in drv-forker (fd 3, like a wire).
pub const CHANNEL_FD: i32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    pub uid: u32,
    /// Supplementary group names (`render` for the GPU). Each must be on the forker's list.
    pub groups: Vec<String>,
    /// `argv[0]` is looked up in `PATH` from `env`.
    pub argv: Vec<String>,
    /// The child's whole environment, plus `HOME` and `XDG_RUNTIME_DIR` which the forker sets.
    pub env: Vec<(String, String)>,
    /// Keep the host network. Otherwise the child gets a new, empty network namespace.
    pub network: bool,
    /// Extra `/run` entries for this app (the services' bus for a notification daemon). Each
    /// must be on the forker's optional list.
    #[serde(default)]
    pub expose: Vec<String>,
    /// Where the fds sent with this request land in the child, in order (3 for a wire).
    #[serde(default)]
    pub fds: Vec<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Launch(Launch),
    /// UIDs with a live child right now.
    Running,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Forked { pid: u32 },
    Running { uids: Vec<u32> },
    Error(String),
}

/// What the supervisor tells drv-appd unprompted, on its notice socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Notice {
    /// A compositor was just started, at boot or after one died.
    CompositorStarted,
}

/// drv-appd's end. Shared between its connection threads; one request at a time.
pub struct Channel(Mutex<OwnedFd>);

impl Channel {
    pub fn new(sock: OwnedFd) -> Self {
        Self(Mutex::new(sock))
    }

    fn call(&self, request: &Request, fds: &[BorrowedFd<'_>]) -> io::Result<Response> {
        let sock = self.0.lock().unwrap();
        seq::send(&*sock, request, fds)?;
        let (response, _) = seq::recv(&*sock)?;
        Ok(response)
    }

    pub fn launch(&self, launch: &Launch, fds: &[BorrowedFd<'_>]) -> io::Result<u32> {
        match self.call(&Request::Launch(launch.clone()), fds)? {
            Response::Forked { pid } => Ok(pid),
            Response::Error(err) => Err(io::Error::other(err)),
            other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
        }
    }

    pub fn running(&self) -> io::Result<Vec<u32>> {
        match self.call(&Request::Running, &[])? {
            Response::Running { uids } => Ok(uids),
            Response::Error(err) => Err(io::Error::other(err)),
            other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
        }
    }
}
