//! The spawner's wire (see `drv_policy::wire`): our peers arrive on it already connected.
//! The seat is needed before anything else exists, so it is read blocking at startup; what
//! else arrives meanwhile is kept for the event loop.

use std::os::fd::OwnedFd;
use std::sync::Mutex;

use drv_policy::wire::{self, Attach};

static WIRE: Mutex<Option<OwnedFd>> = Mutex::new(None);
static PENDING: Mutex<Vec<(Attach, OwnedFd)>> = Mutex::new(Vec::new());

/// Whether the spawner gave us a wire at all.
pub fn init() -> bool {
    match wire::take() {
        Some(wire) => {
            *WIRE.lock().unwrap() = Some(wire);
            true
        }
        None => false,
    }
}

/// Blocks until the seat connection arrives: the spawner sends it as soon as both we and the
/// seat daemon are up, and there is nothing to do before that.
pub fn take_seat() -> Option<OwnedFd> {
    let wire = WIRE.lock().unwrap();
    let wire = wire.as_ref()?;
    let mut pending = PENDING.lock().unwrap();
    if let Some(i) = pending.iter().position(|(a, _)| *a == Attach::Seat) {
        return Some(pending.remove(i).1);
    }
    loop {
        match wire::recv_attach(wire) {
            Ok((Attach::Seat, fd)) => return Some(fd),
            Ok(other) => pending.push(other),
            Err(err) => {
                error!("the spawner's wire failed before the seat arrived: {err}");
                return None;
            }
        }
    }
}

/// The wire for the event loop, and what arrived before it took over.
pub fn take() -> Option<(OwnedFd, Vec<(Attach, OwnedFd)>)> {
    let wire = WIRE.lock().unwrap().take()?;
    Some((wire, std::mem::take(&mut *PENDING.lock().unwrap())))
}
