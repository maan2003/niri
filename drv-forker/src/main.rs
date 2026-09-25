//! drv-appd's privileged helper, kept dumb on purpose. It hears from exactly one peer, drv-appd,
//! over the socketpair the supervisor made for the two of them, and forks once per request
//! before looking at it: the parent only receives bytes, forks and reaps. The child does what
//! takes privilege and nothing else (ARCH-app-policy, "Launching"; DESIGN-app-namespace): a
//! mount namespace with a root tmpfs the app owns, the few mounts into it (the store, the
//! device and sysfs views, proc, the doors, the app's state, the person's folders it is given),
//! the cgroup, the UID switch, and
//! the exec of drv-init, which as the app makes the rest of the root and restricts itself.
//! No config files, no policy, no idea what an "app" is beyond the request type: drv-appd is
//! the brain; a bug here is reachable only through it. Zygote on Android has the same shape.

use std::convert::Infallible;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use drv_os::mounts::{clone_tree, new_fs, set_attrs, Attr};
use drv_policy::forker::{Request, Response};
use drv_policy::seq;
use rustix::event::{poll, PollFd, PollFlags};
use rustix::fs::{Gid, Mode, OFlags, Uid, CWD};
use rustix::process::Pid;
use rustix::thread::CapabilitySet;

/// What forking an app takes: the namespace and its mounts (SYS_ADMIN), the UID switch
/// (SETUID, SETGID), locking the securebits and emptying the bounding set (SETPCAP), and
/// the walk into the person's folders, drv-files' and 0700 (DAC_READ_SEARCH).
const NEEDED: CapabilitySet = CapabilitySet::SYS_ADMIN
    .union(CapabilitySet::SETUID)
    .union(CapabilitySet::SETGID)
    .union(CapabilitySet::SETPCAP)
    .union(CapabilitySet::DAC_READ_SEARCH);

/// The app's persistent directory, the one mount made for its UID.
const STATE: &str = "/state";
/// Where the person's folders an app is given go, one idmapped bind each.
const FILES: &str = "/files";
/// The root tmpfs's size: `/tmp`, `/etc`, a HOME of the run and the runtime directory
/// share it. What persists is on `/state`.
const ROOT_SIZE: &str = "1g";

#[derive(Parser)]
#[command(name = "drv-forker", about = "Fork sandboxed apps for drv-appd")]
struct Args {
    /// `start:count`: the UIDs apps may run as.
    #[arg(long)]
    range: String,
    /// A directory of ours to hang an app's new root on for the moment the pivot takes.
    #[arg(long, default_value = "/run/drv-apps")]
    base: PathBuf,
    /// Per-UID state parent: `<state-base>/<uid>` is the app's `/state`. The directories
    /// exist (tmpfiles); nothing is made or chowned here.
    #[arg(long, default_value = "/var/lib/drv-apps")]
    state_base: PathBuf,
    /// The store: the only executable thing in an app's root.
    #[arg(long, default_value = "/nix/store")]
    store: PathBuf,
    /// The doors: the one directory the system configuration fills with every socket an
    /// app may reach and the documents mount, bound read-only at the same path in every
    /// app's root (the mount inside it as it is). Each door keys on the peer UID itself.
    #[arg(long, default_value = "/run/drv")]
    run: PathBuf,
    /// The nix daemon's socket directory, at the same path inside for an app with `nix`.
    #[arg(long, default_value = "/nix/var/nix/daemon-socket")]
    daemon_socket: PathBuf,
    /// The host's generated views of itself (`dev`, `dev-gpu`, `sys`, `sys-gpu`), written
    /// at boot.
    #[arg(long, default_value = "/run/drv-host")]
    host_views: PathBuf,
    /// The host's resolv.conf, handed to drv-init as fd `resolv` for a networked app.
    #[arg(long, default_value = "/etc/resolv.conf")]
    resolv: PathBuf,
    /// The person's files (drv-files' tree): a folder of it named in the request is bound
    /// into the app's root idmapped, the tree's owner appearing as the app.
    #[arg(long, default_value = "/var/lib/drv-files")]
    folders_base: PathBuf,
}

struct Forker {
    start: u32,
    count: u32,
    args: Args,
}

fn err<T>(what: &str, e: impl std::fmt::Display) -> Result<T, String> {
    Err(format!("{what}: {e}"))
}

impl Forker {
    fn covers(&self, uid: u32) -> bool {
        self.start <= uid && (uid as u64) < self.start as u64 + self.count as u64
    }

    /// The parent: receive, fork, reap, until the peer closes the channel (drv-appd died;
    /// the supervisor restarts us both). Single-threaded, so a child may do anything.
    fn serve(&self, channel: OwnedFd) -> Result<(), String> {
        let mut children: Vec<(Pid, OwnedFd)> = Vec::new();
        loop {
            let mut fds = vec![PollFd::new(&channel, PollFlags::IN)];
            fds.extend(
                children
                    .iter()
                    .map(|(_, fd)| PollFd::new(fd, PollFlags::IN)),
            );
            poll(&mut fds, None).map_err(|e| format!("poll: {e}"))?;
            let ready: Vec<bool> = fds.iter().map(|f| !f.revents().is_empty()).collect();
            let mut i = 0;
            children.retain(|(pid, _)| {
                i += 1;
                if ready[i] {
                    reap(*pid);
                }
                !ready[i]
            });
            if !ready[0] {
                continue;
            }
            let (bytes, _fds) = match seq::recv_bytes(&channel) {
                Ok(r) => r,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return err("channel", e),
            };
            // A fork with the child in PID, IPC and UTS namespaces of its own from the start:
            // it is PID 1 there, and so is drv-init, which it execs. Mount and net come
            // in the child, which has handles to take first.
            // SAFETY: single-threaded; the child runs ordinary code and never returns.
            let pid = unsafe {
                libc::syscall(
                    libc::SYS_clone,
                    (libc::CLONE_NEWPID | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS) as libc::c_long
                        | libc::SIGCHLD as libc::c_long,
                    0,
                    0,
                    0,
                    0,
                )
            } as libc::pid_t;
            if pid < 0 {
                let e = io::Error::last_os_error();
                seq::send(&channel, &Response::Error(format!("clone: {e}")), &[])
                    .map_err(|e| format!("channel: {e}"))?;
                continue;
            }
            if pid == 0 {
                let response = match self.child(&channel, &bytes) {
                    Ok(never) => match never {},
                    Err(e) => Response::Error(e),
                };
                let _ = seq::send(&channel, &response, &[]);
                std::process::exit(1);
            }
            let pid = Pid::from_raw(pid).unwrap();
            match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
                Ok(fd) => children.push((pid, fd)),
                Err(e) => drv_os::say!("drv-forker: pidfd for {pid:?}: {e}"),
            }
        }
    }

    /// The child, from clone to exec. Handles on the host first, then the request (its UID
    /// and the three features that are mounts), then the namespace and its root, then the
    /// switch; argv and env are touched only once the process is the app. Returns only an
    /// error; success is exec.
    fn child(&self, channel: &OwnedFd, bytes: &[u8]) -> Result<Infallible, String> {
        // 1. Handles on everything of the host's we will need. A handle taken before the
        // unshare stays valid in the new namespace; nothing can be cloned from there after.
        let host_net = rustix::fs::open(
            "/proc/self/ns/net",
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| format!("/proc/self/ns/net: {e}"))?;
        // The apps' cgroup is our cgroup's sibling: the supervisor keeps its members in `set`
        // and the apps' subtree, ours to fill, in `apps` next to it.
        let own = drv_os::own_cgroup()?;
        let apps_cgroup = open_path(&own.parent().ok_or("own cgroup has no parent")?.join("apps"))?;
        let ro = Attr::MOUNT_ATTR_RDONLY | Attr::MOUNT_ATTR_NOSUID | Attr::MOUNT_ATTR_NODEV;
        let ro_noexec = ro | Attr::MOUNT_ATTR_NOEXEC;
        let rw_noexec = Attr::MOUNT_ATTR_NOSUID | Attr::MOUNT_ATTR_NODEV | Attr::MOUNT_ATTR_NOEXEC;
        // Device nodes live in the views, so no NODEV.
        let dev_attr = Attr::MOUNT_ATTR_RDONLY | Attr::MOUNT_ATTR_NOSUID | Attr::MOUNT_ATTR_NOEXEC;
        let clone = |path: &Path, attrs| {
            clone_tree(CWD, path, attrs).map_err(|e| format!("{}: {e}", path.display()))
        };
        let view = |name: &str, attrs| clone(&self.args.host_views.join(name), attrs);
        let store = clone(&self.args.store, ro)?;
        let dev = view("dev", dev_attr)?;
        let dri = view("dev-gpu/dri", dev_attr).ok();
        let sys = view("sys", ro_noexec)?;
        let sys_gpu = view("sys-gpu", ro_noexec).ok();
        // The doors, the documents mount among them: read-only at the top, the mount inside
        // left as it is (apps write the documents they were given).
        let run = clone(&self.args.run, rw_noexec)?;
        set_attrs(&run, ro_noexec, false)
            .map_err(|e| format!("{}: {e}", self.args.run.display()))?;
        let daemon_socket = clone(&self.args.daemon_socket, ro_noexec).ok();
        // The host's resolver, open for reading: drv-init copies it into a networked app's /etc.
        let resolv = rustix::fs::open(
            &self.args.resolv,
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok();

        // 2. The request. Until the switch, only uid, network, gpu, nix and folders are read.
        let Request::Launch(launch): Request =
            seq::decode(bytes).map_err(|e| format!("request: {e}"))?;
        let uid = launch.uid;
        if !self.covers(uid) {
            return Err(format!("uid {uid} is outside the app range"));
        }
        let uid_s = uid.to_string();
        let (u, g) = (Uid::from_raw(uid), Gid::from_raw(uid));
        if launch.argv.is_empty() {
            return Err("empty argv".into());
        }
        // The person's folders, cloned from the host now (nothing can be after the unshare)
        // and idmapped: the tree's owner (drv-files, read off the base directory) is the app
        // inside. One namespace holds the mapping for all of them.
        let folders = if launch.folders.is_empty() {
            Vec::new()
        } else {
            let base = rustix::fs::stat(&self.args.folders_base)
                .map_err(|e| format!("{}: {e}", self.args.folders_base.display()))?;
            let userns = drv_os::userns::map((base.st_uid, base.st_gid), (uid, uid))
                .map_err(|e| format!("folders' user namespace: {e}"))?;
            let mut clones = Vec::with_capacity(launch.folders.len());
            for name in &launch.folders {
                let rel = Path::new(name);
                if rel.is_absolute()
                    || rel
                        .components()
                        .any(|c| !matches!(c, std::path::Component::Normal(_)))
                {
                    return Err(format!("folder {name:?}: not a plain relative path"));
                }
                let fd = clone(&self.args.folders_base.join(rel), rw_noexec)?;
                drv_os::mounts::set_idmap(&fd, &userns)
                    .map_err(|e| format!("folder {name:?}: idmap: {e}"))?;
                clones.push((name.clone(), fd));
            }
            clones
        };

        // 3. Privilege we will never need again goes, then a mount namespace of our own,
        // emptied, with the host's network only if asked.
        drv_os::creds::lock_securebits()?;
        drv_os::creds::empty_bounding_set()?;
        drv_os::creds::drop_capability(CapabilitySet::SETPCAP)?;
        rustix::thread::set_no_new_privs(true).map_err(|e| format!("no_new_privs: {e}"))?;
        // SAFETY: single-threaded.
        unsafe { drv_os::root::unshare(true) }?;
        // The state directories' parent, as a plain handle into this namespace's copy of the
        // old root (a handle from before the unshare would point into the parent's
        // namespace, which nothing may be cloned from): the UID's subdirectory is cloned
        // through it below, while the old root is still there.
        let state_base = open_path(&self.args.state_base)?;
        if launch.network {
            rustix::thread::move_into_link_name_space(
                host_net.as_fd(),
                Some(rustix::thread::LinkNameSpaceType::Network),
            )
            .map_err(|e| format!("rejoin the network: {e}"))?;
        }
        drop(host_net);
        // The root: a fresh tmpfs the app owns (drv-init makes /tmp, /etc, HOME and the
        // runtime directory in it, as the app), made our namespace's root. The old root
        // stays stacked beneath it, reachable through the handles above and nothing else.
        drv_os::root::pivot(
            &self.args.base,
            &[("uid", &uid_s), ("gid", &uid_s), ("size", ROOT_SIZE)],
        )?;
        // The mountpoints and the mounts on them, as the app's effective UID (the
        // capabilities stay: SECBIT_NO_SETUID_FIXUP): the root is the app's, so a directory
        // made in it as us would not be, and its state directory opens for it alone.
        let own_uid = rustix::process::getuid();
        rustix::thread::set_thread_res_uid(own_uid, u, own_uid)
            .map_err(|e| format!("euid {uid}: {e}"))?;
        let mounted = (|| -> Result<(), String> {
            let mount = |fd: OwnedFd, at: &str| drv_os::root::mount(fd, Path::new(at));
            mount(store, &self.args.store.to_string_lossy())?;
            mount(dev, "/dev")?;
            mount(
                new_fs(
                    "tmpfs",
                    &[("mode", "1777"), ("uid", &uid_s), ("gid", &uid_s)],
                    rw_noexec,
                )
                .map_err(|e| format!("shm: {e}"))?,
                "/dev/shm",
            )?;
            // Its pseudo-terminals: a devpts instance of its own (every mount is one), reached
            // through the view's /dev/ptmx link to pts/ptmx. Device nodes, so no NODEV.
            mount(
                new_fs(
                    "devpts",
                    &[("ptmxmode", "0666")],
                    Attr::MOUNT_ATTR_NOSUID | Attr::MOUNT_ATTR_NOEXEC,
                )
                .map_err(|e| format!("pts: {e}"))?,
                "/dev/pts",
            )?;
            // The app's own PID namespace seen through its own proc instance: the pid entries
            // and nothing else (no /proc/sys, meminfo, cpuinfo: side channels, not the app's).
            mount(
                new_fs(
                    "proc",
                    &[("hidepid", "invisible"), ("subset", "pid")],
                    rw_noexec,
                )
                .map_err(|e| format!("proc: {e}"))?,
                "/proc",
            )?;
            if launch.gpu {
                mount(
                    dri.ok_or("no render node view (no GPU on this host?)")?,
                    "/dev/dri",
                )?;
                mount(sys_gpu.ok_or("no sys-gpu view")?, "/sys")?;
            } else {
                mount(sys, "/sys")?;
            }
            mount(run, &self.args.run.to_string_lossy())?;
            if launch.nix {
                mount(
                    daemon_socket.ok_or("no nix daemon socket directory on this host")?,
                    &self.args.daemon_socket.to_string_lossy(),
                )?;
            }
            let state = clone_tree(&state_base, Path::new(&uid_s), rw_noexec)
                .map_err(|e| format!("state/{uid}: {e}"))?;
            mount(state, STATE)?;
            for (name, fd) in folders {
                mount(fd, &format!("{FILES}/{name}"))?;
            }
            Ok(())
        })();
        rustix::thread::set_thread_res_uid(own_uid, own_uid, own_uid)
            .map_err(|e| format!("euid back: {e}"))?;
        mounted?;
        drop(state_base);
        // The old root, stacked beneath ours since the pivot: gone, with the handles into it.
        // The top level stays writable (it is the app's; nothing in it is ours) but runs
        // nothing: the store is the one mount without noexec.
        drv_os::root::finish(rw_noexec)?;
        // One cgroup per app UID under the subtree the supervisor delegated to us.
        let name = format!("app-{uid}");
        match rustix::fs::mkdirat(&apps_cgroup, &name, Mode::from_raw_mode(0o755)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return err("cgroup", e),
        }
        let procs = rustix::fs::openat(
            &apps_cgroup,
            format!("{name}/cgroup.procs"),
            OFlags::WRONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|e| format!("cgroup.procs: {e}"))?;
        // "0" means the writing process itself.
        rustix::io::write(&procs, b"0").map_err(|e| format!("cgroup.procs: {e}"))?;
        drop((procs, apps_cgroup));
        // Now that this is the app's cgroup, a cgroup namespace rooted here: /proc/self/cgroup
        // says "/" and nothing about the host's tree.
        // SAFETY: single-threaded.
        unsafe { rustix::thread::unshare_unsafe(rustix::thread::UnshareFlags::NEWCGROUP) }
            .map_err(|e| format!("cgroup namespace: {e}"))?;

        // 4. The switch. Its own group and nothing else (set explicitly: gid 0 would see
        // through hidepid), no capability of any kind.
        drv_os::creds::switch_to(u, g, &[g], CapabilitySet::empty())?;

        // 5. As the app, with nothing: its command (drv-init, which the system configuration
        // puts first: it makes the rest of the root, restricts itself, runs the app and stays
        // as its init), with the resolver's fd for a networked app.
        let mut env: Vec<(String, String)> = launch.env.clone();
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let exe = which(&launch.argv[0], &path)
            .ok_or_else(|| format!("{}: not found in PATH", launch.argv[0]))?;
        let mut placed = Vec::new();
        if launch.network {
            let resolv = resolv.ok_or("no resolv.conf on the host")?;
            let (fd_env, fds) = drv_os::fds::handoff(&[("resolv", resolv.as_fd())])
                .map_err(|e| format!("resolv: {e}"))?;
            env.extend(fd_env);
            // A copy above the target, so the dup2 below is never a same-fd no-op (which
            // would keep close-on-exec set).
            for (target, fd) in fds {
                placed.push((drv_os::dup_high(fd.as_raw_fd())?, target));
            }
        }
        let c_argv: Vec<CString> = launch
            .argv
            .iter()
            .map(|a| CString::new(a.as_bytes()))
            .collect::<Result<_, _>>()
            .map_err(|_| "NUL in argv")?;
        let c_env: Vec<CString> = env
            .iter()
            .map(|(k, v)| CString::new(format!("{k}={v}")))
            .collect::<Result<_, _>>()
            .map_err(|_| "NUL in env")?;
        // Forked goes first: after the exec there is nobody left to answer, and `which` has
        // already found the file. From here a failure is the journal's, not the channel's.
        seq::send(channel, &Response::Forked, &[]).map_err(|e| format!("channel: {e}"))?;
        for (high, target) in placed {
            // SAFETY: plain dup2 of fds we own.
            if unsafe { libc::dup2(high, target) } < 0 {
                drv_os::say!(
                    "drv-forker: uid {uid}: dup2: {}",
                    io::Error::last_os_error()
                );
                std::process::exit(1);
            }
        }
        let mut argv_p: Vec<*const libc::c_char> = c_argv.iter().map(|s| s.as_ptr()).collect();
        argv_p.push(std::ptr::null());
        let mut env_p: Vec<*const libc::c_char> = c_env.iter().map(|s| s.as_ptr()).collect();
        env_p.push(std::ptr::null());
        // SAFETY: NUL-terminated arrays of NUL-terminated strings.
        unsafe { libc::execve(exe.as_ptr(), argv_p.as_ptr(), env_p.as_ptr()) };
        drv_os::say!(
            "drv-forker: uid {uid}: exec {}: {}",
            launch.argv[0],
            io::Error::last_os_error()
        );
        std::process::exit(1);
    }
}

/// `argv[0]` as a path: itself if it has a slash, else the first hit in `path`.
fn which(name: &str, path: &str) -> Option<CString> {
    let candidates: Vec<PathBuf> = if name.contains('/') {
        vec![PathBuf::from(name)]
    } else {
        path.split(':')
            .filter(|d| !d.is_empty())
            .map(|d| Path::new(d).join(name))
            .collect()
    };
    candidates
        .into_iter()
        .find(|c| rustix::fs::access(c, rustix::fs::Access::EXEC_OK).is_ok())
        .and_then(|c| CString::new(c.as_os_str().as_bytes()).ok())
}

fn open_path(path: &Path) -> Result<OwnedFd, String> {
    rustix::fs::open(
        path,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| format!("{}: {e}", path.display()))
}

/// A child of ours ended: reap it and log which UID it ran as. The UID comes with the
/// status (siginfo's si_uid, which rustix does not expose): our /proc hides other UIDs'
/// processes.
fn reap(pid: Pid) {
    // SAFETY: a zeroed siginfo_t is a valid out-parameter; the accessors are read after a
    // successful waitid, which filled it for a child that exited or was killed.
    let (code, status, uid) = unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        if libc::waitid(
            libc::P_PID,
            pid.as_raw_nonzero().get() as libc::id_t,
            &mut info,
            libc::WEXITED,
        ) != 0
        {
            return;
        }
        (info.si_code, info.si_status(), info.si_uid())
    };
    let how = if code == libc::CLD_EXITED {
        format!("exit status: {status}")
    } else {
        format!("signal: {status}")
    };
    drv_os::say!(
        "drv-forker: pid {} (uid {uid}) exited: {how}",
        pid.as_raw_nonzero()
    );
}

fn parse_range(s: &str) -> Result<(u32, u32), String> {
    let (start, count) = s
        .split_once(':')
        .ok_or_else(|| format!("--range {s:?}: expected start:count"))?;
    let num = |x: &str| x.parse::<u32>().map_err(|e| format!("--range {x:?}: {e}"));
    let (start, count) = (num(start)?, num(count)?);
    if start == 0 {
        return Err("--range must not include root".to_owned());
    }
    Ok((start, count))
}

fn run(args: Args) -> Result<(), String> {
    let channel = drv_os::fds::take()
        .and_then(|mut fds| fds.socket("channel", drv_os::fds::Kind::SeqPacket))
        .map_err(|e| format!("the channel from the supervisor: {e}"))?;
    let caps = rustix::thread::capabilities(None).map_err(|e| format!("capabilities: {e}"))?;
    if !caps.effective.contains(NEEDED) {
        return Err(format!("needs {NEEDED:?}, has {:?}", caps.effective));
    }
    let (start, count) = parse_range(&args.range)?;
    let forker = Forker { start, count, args };
    forker.serve(channel)
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            drv_os::say!("drv-forker: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_and_which() {
        assert_eq!(parse_range("100000:1000").unwrap(), (100000, 1000));
        assert!(parse_range("0:5").is_err());
        assert!(parse_range("5").is_err());
        assert!(which("sh", "/nonexistent:/bin:/usr/bin").is_some());
        assert!(which("definitely-not-a-program", "/bin").is_none());
        assert_eq!(which("/bin/sh", "").unwrap().to_str().unwrap(), "/bin/sh");
    }
}
