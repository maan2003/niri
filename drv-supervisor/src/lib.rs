//! The supervisor starts the trusted set, each piece as its own user with its peers already
//! in hand: every connection between two pieces is a socketpair the supervisor made before
//! forking either, handed over by name (`drv_os::fds`). Nobody connects to anybody, nobody is
//! found, nothing arrives at runtime. It takes input from no one. The set is one group: when
//! any member dies the apps are killed, the rest stopped, and everything starts again, locked.

use std::fs::File;
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use drv_os::sandbox::Sandbox;
use drv_os::{dup_high, ensure_owned_dir};
use rustix::thread::{CapabilitySet, CapabilitySets};

/// A service the supervisor forks and keeps running: its own user, the same sandbox an app
/// gets (`drv_os::sandbox`: private `/tmp`, `/dev/shm` and `/proc`, a `/run` holding only
/// `expose`, no network unless `network`), its environment exactly as listed.
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
    /// Entries under `/run` it sees; everything else under `/run` is gone.
    pub expose: Vec<PathBuf>,
    /// Keeps the host's network namespace (the forker: apps with `network` get it from there).
    pub network: bool,
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
        "setpcap" => CapabilitySet::SETPCAP,
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

/// Between fork and exec: become `uid`/`gid`/`groups` keeping exactly `caps`, and make `caps`
/// the bounding set. Works for a root supervisor and for one that holds `caps` itself plus
/// SETUID, SETGID and SETPCAP. Everything here is async-signal-safe (raw syscalls).
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

/// The apps' cgroup: `<ours>/apps`, made once. drv-forker owns the directory (it makes
/// `app-<uid>` cgroups in it) and its process files (moving a process in needs write access
/// to those of the common ancestor, so our own `cgroup.procs` goes to it too); `cgroup.kill`
/// stays ours, and one write to it ends every app at once.
pub struct AppsCgroup {
    kill: PathBuf,
}

impl AppsCgroup {
    /// systemd's `Delegate=yes` made our subtree ours; hand the apps part of it to the forker.
    pub fn create(forker_uid: u32, forker_gid: u32) -> Result<Self, String> {
        let ours = drv_os::own_cgroup()?;
        let dir = ours.join("apps");
        match std::fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("mkdir {}: {e}", dir.display())),
        }
        let owner = (
            Some(rustix::process::Uid::from_raw(forker_uid)),
            Some(rustix::process::Gid::from_raw(forker_gid)),
        );
        for path in [
            ours.join("cgroup.procs"),
            dir.clone(),
            dir.join("cgroup.procs"),
            dir.join("cgroup.threads"),
            dir.join("cgroup.subtree_control"),
        ] {
            rustix::fs::chown(&path, owner.0, owner.1)
                .map_err(|e| format!("chown {}: {e}", path.display()))?;
        }
        Ok(Self {
            kill: dir.join("cgroup.kill"),
        })
    }

    /// SIGKILLs every process in every app cgroup. Returns once the kernel has taken the
    /// request; the processes go shortly after.
    pub fn kill_all(&self) -> Result<(), String> {
        File::options()
            .write(true)
            .open(&self.kill)
            .and_then(|mut f| f.write_all(b"1"))
            .map_err(|e| format!("write {}: {e}", self.kill.display()))
    }
}

/// Forks a service with `fds` as its named fds (`LISTEN_FDS`/`LISTEN_FDNAMES`, from 3 up).
/// Everything else we hold stays close-on-exec and never reaches it.
pub fn start_service(service: &Service, fds: &[(&str, BorrowedFd<'_>)]) -> Result<Child, String> {
    for (dir, mode) in &service.dirs {
        ensure_owned_dir(dir, service.uid, service.gid, *mode)?;
    }
    if service.argv.is_empty() {
        return Err(format!("service {}: empty command", service.name));
    }
    let (fd_env, placed) = drv_os::fds::handoff(fds).map_err(|e| format!("{}: {e}", service.name))?;
    let sandbox = Sandbox::plan(&service.expose, service.network, None)
        .map_err(|e| format!("{}: {e}", service.name))?;
    // Copies above the target numbers, so the dup2s in the child never clobber each other
    // and are never a same-fd no-op (which would keep close-on-exec set).
    let mut dups = Vec::new();
    for (target, fd) in &placed {
        dups.push((dup_high(fd.as_raw_fd())?, *target));
    }
    let mut command = Command::new(&service.argv[0]);
    command
        .args(&service.argv[1..])
        .env_clear()
        .envs(service.env.iter().cloned())
        .envs(fd_env)
        .stdin(Stdio::null());
    let (uid, gid, caps) = (service.uid, service.gid, service.caps);
    let groups = service.groups.clone();
    let child_dups = dups.clone();
    // SAFETY: only dup2, mount, credential and capability syscalls between fork and exec.
    unsafe {
        command.pre_exec(move || {
            for (high, target) in &child_dups {
                if libc::dup2(*high, *target) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            // While CAP_SYS_ADMIN is still ours.
            sandbox.apply()?;
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
