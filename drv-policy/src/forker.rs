//! The channel between drv-appd and drv-forker: a socketpair the supervisor made before
//! forking both (fd `channel` on both sides), so nothing else can reach it. Requests and
//! replies in lock step on [`seq`](crate::seq). The forker knows nothing about apps: what it
//! gets here is what it does. Apps get no fds: nothing forked here holds authority.

use std::io;
use std::os::fd::OwnedFd;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::seq;

/// An app, as far as the forker is concerned: the UID, the mounts that depend on the
/// manifest (the render node, the nix daemon's socket, the host's network) and the command.
/// Everything the app does to its own root once it is the app (its `/etc`, HOME, the links,
/// the Landlock rules, the syscall filter) is drv-init's, from the command line the system
/// configuration gave it. No paths here: the forker has its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    pub uid: u32,
    /// `argv[0]` is looked up in `PATH` from `env`.
    pub argv: Vec<String>,
    /// The child's whole environment. `HOME` and `XDG_RUNTIME_DIR` are drv-init's to set.
    pub env: Vec<(String, String)>,
    /// Keep the host network. Otherwise the child gets a new, empty network namespace.
    #[serde(default)]
    pub network: bool,
    /// The render node and the host's view of it.
    #[serde(default)]
    pub gpu: bool,
    /// The nix daemon's socket directory, at its own path.
    #[serde(default)]
    pub nix: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Launch(Launch),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Forked,
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

    pub fn launch(&self, launch: &Launch) -> io::Result<()> {
        match self.call(&Request::Launch(launch.clone()))? {
            Response::Forked => Ok(()),
            Response::Error(err) => Err(io::Error::other(err)),
        }
    }
}
