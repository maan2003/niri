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
use drv_os::{check_owned_dir, dup_high};
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
    /// `(path, mode)`: directories it owns (tmpfiles rules), checked before each start and in its
    /// root.
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
    /// Sees `/sys` writable (the media keys: the backlight), as far as its groups allow.
    pub writable_sys: bool,
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
        (
            "/sys".into(),
            clone(
                Path::new("/sys"),
                if service.writable_sys { rw_noexec } else { ro_noexec },
            )?,
        ),
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

/// Our two cgroups under the subtree systemd delegated to us, made once. `apps`: drv-forker
/// owns the directory (it makes `app-<uid>` cgroups in it) and its process files; our own
/// `cgroup.procs` is group-writable for it (the common ancestor of any move). The one chown
/// of our life. `set`: ours, where every member puts itself before it switches user. Both
/// `cgroup.kill` files stay ours: one write to each ends every app, then every member, with
/// no CAP_KILL.
pub struct Cgroups {
    apps_kill: PathBuf,
    set_kill: PathBuf,
    /// `set/cgroup.procs`: a member's child writes `0` to it, as us, before the switch.
    pub set_procs: PathBuf,
}

impl Cgroups {
    pub fn create(forker_uid: u32, forker_gid: u32) -> Result<Self, String> {
        let ours = drv_os::own_cgroup()?;
        let mkdir = |dir: &Path| match std::fs::create_dir(dir) {
            Ok(()) | Err(_) if dir.is_dir() => Ok(()),
            Err(e) => Err(format!("mkdir {}: {e}", dir.display())),
            Ok(()) => Ok(()),
        };
        let apps = ours.join("apps");
        mkdir(&apps)?;
        let owner = (
            Some(rustix::process::Uid::from_raw(forker_uid)),
            Some(rustix::process::Gid::from_raw(forker_gid)),
        );
        for path in [
            apps.clone(),
            apps.join("cgroup.procs"),
            apps.join("cgroup.threads"),
            apps.join("cgroup.subtree_control"),
        ] {
            rustix::fs::chown(&path, owner.0, owner.1)
                .map_err(|e| format!("chown {}: {e}", path.display()))?;
        }
        // Moving a process needs write access to the common ancestor's cgroup.procs: ours,
        // for the forker (set -> apps/app-<uid>) and for us (root -> set) alike. Ours by
        // owner, the forker's by group.
        let procs = ours.join("cgroup.procs");
        rustix::fs::chown(&procs, None, owner.1)
            .map_err(|e| format!("chgrp {}: {e}", procs.display()))?;
        rustix::fs::chmod(&procs, rustix::fs::Mode::from_raw_mode(0o664))
            .map_err(|e| format!("chmod {}: {e}", procs.display()))?;
        let set = ours.join("set");
        mkdir(&set)?;
        Ok(Self {
            apps_kill: apps.join("cgroup.kill"),
            set_kill: set.join("cgroup.kill"),
            set_procs: set.join("cgroup.procs"),
        })
    }

    /// SIGKILLs every process in every app cgroup. Returns once the kernel has taken the
    /// request; the processes go shortly after.
    pub fn kill_apps(&self) -> Result<(), String> {
        kill(&self.apps_kill)
    }

    /// SIGKILLs every member of the set.
    pub fn kill_set(&self) -> Result<(), String> {
        kill(&self.set_kill)
    }
}

fn kill(path: &Path) -> Result<(), String> {
    File::options()
        .write(true)
        .open(path)
        .and_then(|mut f| f.write_all(b"1"))
        .map_err(|e| format!("write {}: {e}", path.display()))
}

/// Forks a service with `fds` as its named fds (`LISTEN_FDS`/`LISTEN_FDNAMES`, from 3 up).
/// Everything else we hold stays close-on-exec and never reaches it.
pub fn start_service(
    service: &Service,
    fds: &[(&str, BorrowedFd<'_>)],
    set_procs: &Path,
) -> Result<Child, String> {
    for (dir, mode) in &service.dirs {
        check_owned_dir(dir, service.uid, service.gid, *mode)
            .map_err(|e| format!("{}: {e}", service.name))?;
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
    let set_procs = set_procs.to_owned();
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
                // Into the set's cgroup, as us: one write to its cgroup.kill ends us all.
                std::fs::write(&set_procs, b"0")
                    .map_err(|e| format!("{}: {e}", set_procs.display()))?;
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
