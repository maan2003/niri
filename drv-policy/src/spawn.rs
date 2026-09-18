//! The channel between the identity daemon and the root spawner: a socketpair the spawner
//! created before forking the daemon, so nothing else can ever reach it. Requests and responses
//! in lock step, framed like [`rpc`](crate::rpc). What a child is handed on top of its exec
//! (see [`wire`](crate::wire)) is asked for here too.

use std::io;
use std::os::unix::net::UnixStream;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::rpc::{read_msg, write_msg};

/// Fd number the spawner hands its child for this channel.
pub const CHANNEL_FD: i32 = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub uid: u32,
    /// Supplementary group names (`render` for the GPU). Each must be on the spawner's list.
    pub groups: Vec<String>,
    /// `argv[0]` is looked up in `PATH` from `env`.
    pub argv: Vec<String>,
    /// The child's whole environment, plus `HOME` and `XDG_RUNTIME_DIR` which the spawner sets.
    pub env: Vec<(String, String)>,
    /// Keep the host network. Otherwise the child gets a new, empty network namespace.
    pub network: bool,
    /// Extra `/run` entries for this app (the services' bus for a notification daemon). Each
    /// must be on the spawner's optional list.
    #[serde(default)]
    pub expose: Vec<String>,
    /// Hand the child a connection to `drv-authd` on its wire (the lock app). The daemon gets
    /// the other end as a verifier.
    #[serde(default)]
    pub auth: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Forked { pid: u32 },
    Error(String),
}

/// The identity daemon's end. Shared between its connection threads; one request at a time.
pub struct Channel(Mutex<UnixStream>);

impl Channel {
    pub fn new(stream: UnixStream) -> Self {
        Self(Mutex::new(stream))
    }

    pub fn fork(&self, request: &Request) -> io::Result<u32> {
        let stream = self.0.lock().unwrap();
        write_msg(&*stream, request)?;
        match read_msg::<Response>(&*stream)? {
            Response::Forked { pid } => Ok(pid),
            Response::Error(err) => Err(io::Error::other(err)),
        }
    }
}
