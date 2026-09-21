//! The channel between drv-appd and drv-forker: a socketpair the supervisor made before
//! forking both (fd `channel` on both sides), so nothing else can reach it. Requests and
//! replies in lock step on [`seq`](crate::seq). The forker knows nothing about apps: what it
//! gets here is what it does. Apps get no fds: nothing forked here holds authority.

use std::io;
use std::os::fd::OwnedFd;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::seq;

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
    /// The app's `/etc`: a store path built from its manifest entry, bound read-only.
    #[serde(default)]
    pub etc: Option<String>,
    /// The render node and the host's view of it.
    #[serde(default)]
    pub gpu: bool,
    /// The app's name: its HOME is `/home/<name>`.
    #[serde(default)]
    pub name: String,
    /// The store paths the app may open: a file listing them, one per line (closureInfo).
    #[serde(default)]
    pub closure: Option<String>,
    /// The app makes code at runtime (a JIT): no MDWE for it.
    #[serde(default)]
    pub jit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Launch(Launch),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Forked { pid: u32 },
    Error(String),
}

/// drv-appd's end. Shared between its connection threads; one request at a time.
pub struct Channel(Mutex<OwnedFd>);

impl Channel {
    pub fn new(sock: OwnedFd) -> Self {
        Self(Mutex::new(sock))
    }

    fn call(&self, request: &Request) -> io::Result<Response> {
        let sock = self.0.lock().unwrap();
        seq::send(&*sock, request, &[])?;
        let (response, _) = seq::recv(&*sock)?;
        Ok(response)
    }

    pub fn launch(&self, launch: &Launch) -> io::Result<u32> {
        match self.call(&Request::Launch(launch.clone()))? {
            Response::Forked { pid } => Ok(pid),
            Response::Error(err) => Err(io::Error::other(err)),
        }
    }
}
