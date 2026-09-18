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
use rustix::thread::{CapabilitySet, CapabilitySets};

/// The pieces the supervisor wires. Each pair below gets a fresh socketpair whenever either
/// side (re)starts while the other is up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Peer {
    Authd,
    Seatd,
    Compositor,
    Gpu,
    Locker,
    Appd,
}

/// The socket a link is made of: one message per datagram, or a byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    Seq,
    Stream,
}

/// `(a, what a receives, b, what b receives, socket type)`.
const LINKS: [(Peer, Attach, Peer, Attach, Link); 6] = [
    (Peer::Compositor, Attach::Seat, Peer::Seatd, Attach::Compositor, Link::Seq),
    (Peer::Compositor, Attach::Auth, Peer::Authd, Attach::Compositor, Link::Seq),
    (Peer::Compositor, Attach::Gpu, Peer::Gpu, Attach::Compositor, Link::Stream),
    (Peer::Compositor, Attach::Locker, Peer::Locker, Attach::Compositor, Link::Stream),
    (Peer::Locker, Attach::Auth, Peer::Authd, Attach::Verifier, Link::Seq),
    (Peer::Appd, Attach::Auth, Peer::Authd, Attach::Verifiers, Link::Seq),
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
        for (a, for_a, b, for_b, kind) in LINKS {
            if a == peer || b == peer {
                link(&mut wires, a, for_a, b, for_b, kind);
            }
        }
    }

    pub fn detach_service(&self, peer: Peer) {
        self.0.lock().unwrap().remove(&peer);
    }
}

/// Links two peers if both are up: a fresh pair, each side's end pushed down its wire.
fn link(
    wires: &mut HashMap<Peer, OwnedFd>,
    a: Peer,
    for_a: Attach,
    b: Peer,
    for_b: Attach,
    kind: Link,
) {
    let (Some(wire_a), Some(wire_b)) = (wires.get(&a), wires.get(&b)) else {
        return;
    };
    let pair = match kind {
        Link::Seq => wire::pair(),
        Link::Stream => wire::stream_pair(),
    };
    let (end_a, end_b) = match pair {
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
    /// Capabilities a non-root service keeps (ambient, so they survive the exec): its whole
    /// bounding set, so nothing it runs can have more.
    pub caps: CapabilitySet,
}

/// A capability by its kernel name without the `CAP_` prefix (`sys_tty_config`).
pub fn capability(name: &str) -> Result<CapabilitySet, String> {
    Ok(match name {
        "chown" => CapabilitySet::CHOWN,
        "dac_override" => CapabilitySet::DAC_OVERRIDE,
        "dac_read_search" => CapabilitySet::DAC_READ_SEARCH,
        "fowner" => CapabilitySet::FOWNER,
        "kill" => CapabilitySet::KILL,
        "setgid" => CapabilitySet::SETGID,
        "setuid" => CapabilitySet::SETUID,
        "net_admin" => CapabilitySet::NET_ADMIN,
        "sys_chroot" => CapabilitySet::SYS_CHROOT,
        "sys_ptrace" => CapabilitySet::SYS_PTRACE,
        "sys_admin" => CapabilitySet::SYS_ADMIN,
        "sys_tty_config" => CapabilitySet::SYS_TTY_CONFIG,
        "mknod" => CapabilitySet::MKNOD,
        other => return Err(format!("unknown capability {other:?}")),
    })
}

/// Between fork and exec, as root: become `uid`/`gid`/`groups` keeping exactly `caps`, and
/// make `caps` the bounding set. Everything here is async-signal-safe (raw syscalls).
fn become_user(uid: u32, gid: u32, groups: &[u32], caps: CapabilitySet) -> io::Result<()> {
    // The bounding set first, while CAP_SETPCAP is still effective.
    for cap in CapabilitySet::all().iter() {
        if cap.bits().count_ones() == 1
            && !caps.contains(cap)
            && rustix::thread::capability_is_in_bounding_set(cap).unwrap_or(false)
        {
            rustix::thread::remove_capability_from_bounding_set(cap)?;
        }
    }
    // Keep the permitted set across the uid change; it is narrowed to `caps` right after.
    rustix::thread::set_keep_capabilities(true)?;
    // SAFETY: plain syscalls on our own credentials.
    if unsafe { libc::setgroups(groups.len(), groups.as_ptr()) } != 0
        || unsafe { libc::setresgid(gid, gid, gid) } != 0
        || unsafe { libc::setresuid(uid, uid, uid) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    rustix::thread::set_capabilities(
        None,
        CapabilitySets {
            effective: caps,
            permitted: caps,
            inheritable: caps,
        },
    )?;
    // Ambient, so they survive the exec.
    for cap in caps.iter() {
        if cap.bits().count_ones() == 1 {
            rustix::thread::configure_capability_in_ambient_set(cap, true)?;
        }
    }
    rustix::thread::set_keep_capabilities(false)?;
    Ok(())
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
    let (uid, gid, caps) = (service.uid, service.gid, service.caps);
    let groups = service.groups.clone();
    let child_dups = dups.clone();
    // SAFETY: only dup2, credential and capability syscalls between fork and exec.
    unsafe {
        command.pre_exec(move || {
            for (high, target) in &child_dups {
                if libc::dup2(*high, *target) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if uid != 0 {
                become_user(uid, gid, &groups, caps)?;
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
