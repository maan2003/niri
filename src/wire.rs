//! Our peers, handed over by the supervisor as named fds (`drv_os::fds`): the seat daemon
//! and the GPU process are needed before anything else exists and are taken at backend init;
//! the rest are taken by the event loop once it is up. Nothing arrives at runtime.

use std::os::fd::OwnedFd;
use std::sync::Mutex;

use drv_os::fds::{Fds, Kind};

static FDS: Mutex<Option<Fds>> = Mutex::new(None);

/// Peers the event loop installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    /// Our connection to drv-authd: unlocks arrive on it.
    Auth,
    /// The locker's Wayland connection.
    Locker,
    /// Our launch channel to drv-appd.
    Appd,
    /// The poke line to drv-menu: a byte per `show-launcher`.
    Menu,
}

/// Whether the supervisor gave us fds at all. A malformed handoff is fatal.
pub fn init() -> bool {
    match drv_os::fds::take() {
        Ok(fds) if fds.is_empty() => false,
        Ok(fds) => {
            *FDS.lock().unwrap() = Some(fds);
            true
        }
        Err(err) => {
            error!("the supervisor's fds: {err}");
            std::process::exit(1);
        }
    }
}

/// The seat connection.
pub fn take_seat() -> Option<OwnedFd> {
    take_socket("seat", Kind::SeqPacket)
}

/// The GPU process's connection.
pub fn take_gpu() -> Option<OwnedFd> {
    take_socket("gpu", Kind::Stream)
}

fn take_socket(name: &str, kind: Kind) -> Option<OwnedFd> {
    let mut fds = FDS.lock().unwrap();
    let fds = fds.as_mut()?;
    match fds.socket(name, kind) {
        Ok(fd) => Some(fd),
        Err(err) => {
            error!("fd {name:?} from the supervisor: {err}");
            None
        }
    }
}

/// What the event loop installs: every peer that was handed over (a missing one is logged
/// and skipped; the piece it belongs to is simply not there).
pub fn take() -> Vec<(Peer, OwnedFd)> {
    let mut out = Vec::new();
    let mut fds = FDS.lock().unwrap();
    let Some(fds) = fds.as_mut() else {
        return out;
    };
    for (peer, name, kind) in [
        (Peer::Auth, "auth", Kind::SeqPacket),
        (Peer::Locker, "locker", Kind::Stream),
        (Peer::Appd, "appd", Kind::Stream),
        (Peer::Menu, "menu", Kind::Stream),
    ] {
        match fds.socket(name, kind) {
            Ok(fd) => out.push((peer, fd)),
            Err(err) => warn!("fd {name:?} from the supervisor: {err}"),
        }
    }
    let leftover = fds.leftover();
    if !leftover.is_empty() {
        warn!("fds from the supervisor nobody took: {leftover:?}");
    }
    out
}
