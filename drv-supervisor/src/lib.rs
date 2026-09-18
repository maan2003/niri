//! The supervisor starts the trusted set and hands each piece its peers. Every child with
//! peers gets a wire on fd 3 (see `drv_policy::wire`) down which the supervisor pushes
//! connections it made with `socketpair`: nobody connects to anybody, nobody is found, the
//! process that forked both ends hands them over. It takes input from no one.

use std::collections::HashMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::io;

use drv_os::{dup_high, ensure_owned_dir};
use drv_policy::forker::Notice;
use drv_policy::wire::{self, Attach};
use drv_policy::seq;

/// The pieces the supervisor wires. Each pair below gets a fresh socketpair whenever either
/// side (re)starts while the other is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Peer {
    Authd,
    Seatd,
    Compositor,
    Appd,
}

/// `(a, what a receives, b, what b receives)`.
const LINKS: [(Peer, Attach, Peer, Attach); 3] = [
    (Peer::Compositor, Attach::Seat, Peer::Seatd, Attach::Compositor),
    (Peer::Compositor, Attach::Auth, Peer::Authd, Attach::Compositor),
    (Peer::Appd, Attach::Auth, Peer::Authd, Attach::Verifiers),
];

/// The supervisor's ends of the services' wires, and the links it makes between them.
#[derive(Default)]
pub struct Wiring(Mutex<HashMap<Peer, OwnedFd>>);

impl Wiring {
    /// A service came up (again): keep its wire and link it to its counterparts that are up.
    /// Exactly the pairs the newcomer is part of, so nobody is linked twice for one start.
    pub fn attach_service(&self, peer: Peer, wire: OwnedFd) {
        let mut wires = self.0.lock().unwrap();
        wires.insert(peer, wire);
        for (a, for_a, b, for_b) in LINKS {
            if a == peer || b == peer {
                link(&mut wires, a, for_a, b, for_b);
            }
        }
    }

    pub fn detach_service(&self, peer: Peer) {
        self.0.lock().unwrap().remove(&peer);
    }
}

/// Links two peers if both are up: a fresh pair, each side's end pushed down its wire.
fn link(wires: &mut HashMap<Peer, OwnedFd>, a: Peer, for_a: Attach, b: Peer, for_b: Attach) {
    let (Some(wire_a), Some(wire_b)) = (wires.get(&a), wires.get(&b)) else {
        return;
    };
    let (end_a, end_b) = match wire::pair() {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("drv-supervisor: socketpair: {err}");
            return;
        }
    };
    if let Err(err) = wire::send_attach(wire_a, for_a, end_a.as_fd()) {
        eprintln!("drv-supervisor: {a:?}'s wire: {err}");
        wires.remove(&a);
        return;
    }
    if let Err(err) = wire::send_attach(wire_b, for_b, end_b.as_fd()) {
        eprintln!("drv-supervisor: {b:?}'s wire: {err}");
        wires.remove(&b);
    }
}

/// A service the supervisor forks and keeps running: its own user (or root), no sandbox (it
/// is trusted and needs the real `/run`), its environment exactly as listed.
pub struct Service {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// All supplementary groups of the user, resolved at startup.
    pub groups: Vec<u32>,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    /// `(path, mode)`: directories to own before the first start.
    pub dirs: Vec<(PathBuf, u32)>,
}

/// Forks a service with `fds` on the given numbers in the child (3 is the wire by
/// convention; `DRV_WIRE_FD` is set when something is there). Everything else we hold stays
/// close-on-exec and never reaches it.
pub fn start_service(service: &Service, fds: &[(i32, BorrowedFd<'_>)]) -> Result<Child, String> {
    for (dir, mode) in &service.dirs {
        ensure_owned_dir(dir, service.uid, service.gid, *mode)?;
    }
    if service.argv.is_empty() {
        return Err(format!("service {}: empty command", service.name));
    }
    // Copies above the target numbers, so the dup2s in the child never clobber each other
    // and are never a same-fd no-op (which would keep close-on-exec set).
    let mut dups = Vec::new();
    for (target, fd) in fds {
        dups.push((dup_high(fd.as_raw_fd())?, *target));
    }
    let mut command = Command::new(&service.argv[0]);
    command
        .args(&service.argv[1..])
        .env_clear()
        .envs(service.env.iter().cloned())
        .stdin(Stdio::null());
    if fds.iter().any(|(target, _)| *target == wire::WIRE_FD) {
        command.env(wire::WIRE_ENV, wire::WIRE_FD.to_string());
    }
    let (uid, gid) = (service.uid, service.gid);
    let groups = service.groups.clone();
    let child_dups = dups.clone();
    // SAFETY: only dup2/setgroups/setresgid/setresuid/prctl between fork and exec.
    unsafe {
        command.pre_exec(move || {
            for (high, target) in &child_dups {
                if libc::dup2(*high, *target) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if uid != 0
                && (libc::setgroups(groups.len(), groups.as_ptr()) != 0
                    || libc::setresgid(gid, gid, gid) != 0
                    || libc::setresuid(uid, uid, uid) != 0)
            {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", service.name));
    for (high, _) in dups {
        // SAFETY: our duplicates for the child; the child has its own now.
        unsafe {
            libc::close(high);
        }
    }
    child
}

/// Forks a service with a fresh wire on fd 3; returns the child and our end of the wire.
pub fn start_wired(service: &Service) -> Result<(Child, OwnedFd), String> {
    let (child_end, ours) = wire::pair().map_err(|e| format!("socketpair: {e}"))?;
    let child = start_service(service, &[(wire::WIRE_FD, child_end.as_fd())])?;
    Ok((child, ours))
}

/// What the supervisor knows between starts: the wiring, and the notice socket of the
/// current drv-appd, told about compositor starts.
#[derive(Default)]
pub struct Supervisor {
    pub wiring: Wiring,
    notices: Mutex<Option<OwnedFd>>,
    compositor_up: AtomicBool,
}

impl Supervisor {
    /// A new drv-appd: it hears `CompositorStarted` right away if one is up, so it autostarts
    /// what is missing.
    pub fn appd_started(&self, notices: OwnedFd) {
        *self.notices.lock().unwrap() = Some(notices);
        if self.compositor_up.load(Ordering::SeqCst) {
            self.notify(&Notice::CompositorStarted);
        }
    }

    pub fn appd_stopped(&self) {
        *self.notices.lock().unwrap() = None;
    }

    pub fn compositor_started(&self) {
        self.compositor_up.store(true, Ordering::SeqCst);
        self.notify(&Notice::CompositorStarted);
    }

    pub fn compositor_stopped(&self) {
        self.compositor_up.store(false, Ordering::SeqCst);
    }

    /// One-way; a dead drv-appd is forgotten (its successor announces itself).
    fn notify(&self, notice: &Notice) {
        let mut guard = self.notices.lock().unwrap();
        if let Some(sock) = guard.as_ref() {
            if let Err(err) = seq::send(sock, notice, &[]) {
                eprintln!("drv-supervisor: notifying drv-appd: {err}");
                *guard = None;
            }
        }
    }
}
