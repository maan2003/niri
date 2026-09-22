//! The supervisor starts the trusted set, each piece as its own user with its peers already
//! in hand: every connection between two pieces is a socketpair the supervisor made before
//! forking either, handed over by name (`drv_os::fds`). Nobody connects to anybody, nobody is
//! found, nothing arrives at runtime. It takes input from no one. The set is one group: when
//! any member dies the apps are killed, the rest stopped, and everything starts again, locked.

use std::fs::File;
use std::io::{self, Write as _};
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use drv_os::mounts::{clone_tree, new_fs, Attr};
use drv_os::{dup_high, ensure_owned_dir};
use rustix::fs::CWD;
use rustix::thread::{CapabilitySet, Gid, Uid};

/// A service the supervisor forks and keeps running: its own user, a root of its own built
/// the way an app's is (`drv_os::root`: the store, the real `/dev` and `/sys`, the host's
/// `/etc` read-only, fresh `/tmp`, `/dev/shm` and `/proc`, under `/run` only `expose`, its
/// `dirs`, nothing else of the host's; no network unless `network`), its environment exactly
/// as listed.
#[derive(Clone)]
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
    /// Sees the cgroup tree writable (the forker: one cgroup per app under its subtree).
    pub cgroups: bool,
}

/// A member's root, built in the child: what every member gets, plus its `expose` entries
/// under `/run` (a symlink into the store remade as one) and its `dirs`. Handles first, while
/// the host's root is still in view; then the pivot and the mounts.
fn build_root(service: &Service) -> Result<(), String> {
    let ro = Attr::MOUNT_ATTR_RDONLY | Attr::MOUNT_ATTR_NOSUID | Attr::MOUNT_ATTR_NODEV;
    let ro_noexec = ro | Attr::MOUNT_ATTR_NOEXEC;
    let rw_noexec = Attr::MOUNT_ATTR_NOSUID | Attr::MOUNT_ATTR_NODEV | Attr::MOUNT_ATTR_NOEXEC;
    let clone = |path: &Path, attrs| {
        clone_tree(CWD, path, attrs).map_err(|e| format!("{}: {e}", path.display()))
    };
    let fresh = |fs: &str, opts: &[(&str, &str)]| {
        new_fs(fs, opts, rw_noexec).map_err(|e| format!("{fs}: {e}"))
    };
    // SAFETY: the child of a single-threaded fork.
    unsafe { drv_os::root::unshare(!service.network) }?;
    let mut mounts: Vec<(PathBuf, OwnedFd)> = vec![
        ("/nix/store".into(), clone(Path::new("/nix/store"), ro)?),
        // Device nodes, so no NODEV; the real ones: seatd and the compositor open them.
        (
            "/dev".into(),
            clone(
                Path::new("/dev"),
                Attr::MOUNT_ATTR_NOSUID | Attr::MOUNT_ATTR_NOEXEC,
            )?,
        ),
        ("/dev/shm".into(), fresh("tmpfs", &[("mode", "1777")])?),
        ("/proc".into(), fresh("proc", &[("hidepid", "invisible")])?),
        ("/sys".into(), clone(Path::new("/sys"), ro_noexec)?),
        ("/etc".into(), clone(Path::new("/etc"), ro_noexec)?),
        ("/tmp".into(), fresh("tmpfs", &[("mode", "1777")])?),
    ];
    if service.cgroups {
        mounts.push((
            "/sys/fs/cgroup".into(),
            clone(Path::new("/sys/fs/cgroup"), rw_noexec)?,
        ));
    }
    let mut links: Vec<(PathBuf, PathBuf)> = Vec::new();
    for path in &service.expose {
        let rel = path
            .strip_prefix("/run")
            .map_err(|_| format!("expose {}: not under /run", path.display()))?;
        if rel.as_os_str().is_empty() {
            return Err("expose /run: exposing everything defeats the sandbox".to_owned());
        }
        let meta =
            std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.file_type().is_symlink() {
            links.push((
                std::fs::read_link(path).map_err(|e| format!("{}: {e}", path.display()))?,
                path.clone(),
            ));
        } else {
            mounts.push((path.clone(), clone(path, rw_noexec)?));
        }
    }
    for (dir, _) in &service.dirs {
        if !service.expose.contains(dir) {
            mounts.push((dir.clone(), clone(dir, rw_noexec)?));
        }
    }
    drv_os::root::pivot(Path::new("/tmp"))?;
    for (at, fd) in mounts {
        drv_os::root::mount(fd, &at)?;
    }
    for (target, at) in links {
        drv_os::root::symlink(&target, &at)?;
    }
    drv_os::root::finish(ro_noexec)
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
    let (fd_env, placed) =
        drv_os::fds::handoff(fds).map_err(|e| format!("{}: {e}", service.name))?;
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
    let child_service = service.clone();
    let child_dups = dups.clone();
    // SAFETY: between fork and exec in a single-threaded parent (the supervisor has no
    // threads), so the child may do ordinary work: it builds its root and switches user.
    unsafe {
        command.pre_exec(move || {
            for (high, target) in &child_dups {
                if libc::dup2(*high, *target) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            let s = &child_service;
            // While CAP_SYS_ADMIN is still ours. Only an errno reaches the parent; the words
            // go to the journal from here.
            let child = || -> Result<(), String> {
                build_root(s)?;
                let groups: Vec<Gid> = s.groups.iter().map(|g| Gid::from_raw(*g)).collect();
                drv_os::creds::switch_to(
                    Uid::from_raw(s.uid),
                    Gid::from_raw(s.gid),
                    &groups,
                    s.caps,
                )?;
                rustix::thread::set_no_new_privs(true).map_err(|e| format!("no_new_privs: {e}"))
            };
            child().map_err(|e| {
                drv_os::say!("drv-supervisor: {}: {e}", s.name);
                io::Error::other(e)
            })
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
