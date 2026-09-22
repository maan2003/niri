//! drv-appd's privileged helper, kept dumb on purpose. It hears from exactly one peer, drv-appd,
//! over the socketpair the supervisor made for the two of them, and forks once per request
//! before looking at it: the parent only receives bytes, forks and reaps. The child builds the
//! app's root in a mount namespace of its own (ARCH-app-policy, "Launching";
//! DESIGN-app-namespace), becomes the UID, restricts itself and execs. No config files, no
//! policy, no idea what an "app" is beyond the request type: drv-appd is the brain; a bug here
//! is reachable only through it. Zygote on Android has the same shape.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use drv_os::landlock::{self, Ruleset};
use drv_os::mounts::{attach, clone_tree, new_fs, Attr};
use drv_policy::forker::{Request, Response};
use drv_policy::seq;
use rustix::event::{poll, PollFd, PollFlags};
use rustix::fs::{Gid, Mode, OFlags, Uid, CWD};
use rustix::process::Pid;
use rustix::thread::CapabilitySet;

/// What forking an app takes: the namespace and its mounts (SYS_ADMIN), the UID switch
/// (SETUID, SETGID) and locking the securebits and emptying the bounding set (SETPCAP).
const NEEDED: CapabilitySet = CapabilitySet::SYS_ADMIN
    .union(CapabilitySet::SETUID)
    .union(CapabilitySet::SETGID)
    .union(CapabilitySet::SETPCAP);

/// The layout of `/run` as the system configuration makes it. Every app: the appd socket,
/// the apps' Wayland socket, the doors of drv-files, drv-cast and drv-shell (each keyed on
/// the peer UID), the ssh agent's door (which asks drv-appd about the UID; the rest are
/// refused there), the driver link (a symlink into the store, remade as one), its own
/// runtime directory, and the documents mount, which it writes.
const RUN: &[&str] = &[
    "/run/drv",
    "/run/drv-wayland",
    "/run/drv-files",
    "/run/drv-cast",
    "/run/drv-shell",
    "/run/drv-agent",
    "/run/opengl-driver",
];
const RUN_DOCS: &str = "/run/drv-doc";
/// `audio`: PipeWire's apps socket and the per-app PulseAudio servers.
const RUN_AUDIO: &[&str] = &["/run/drv-audio", "/run/drv-pulse"];
/// HOME, the same path for every app: a tmpfs of the run, with what persists at `.state`.
const HOME: &str = "/home/app";

#[derive(Parser)]
#[command(name = "drv-forker", about = "Fork sandboxed apps for drv-appd")]
struct Args {
    /// `start:count`: the UIDs apps may run as.
    #[arg(long)]
    range: String,
    /// Per-UID `XDG_RUNTIME_DIR` parent; `tmp/<uid>` under it is the app's /tmp. The
    /// directories exist (tmpfiles); nothing is made or chowned here.
    #[arg(long, default_value = "/run/drv-apps")]
    runtime_base: PathBuf,
    /// Per-UID state parent, bound at `$HOME/.state`.
    #[arg(long, default_value = "/var/lib/drv-apps")]
    state_base: PathBuf,
    /// The store: the only executable thing in an app's root.
    #[arg(long, default_value = "/nix/store")]
    store: PathBuf,
    /// The host's generated views of itself (`dev`, `dev-gpu`, `sys`, `sys-gpu`), written
    /// at boot.
    #[arg(long, default_value = "/run/drv-host")]
    host_views: PathBuf,
    /// The host's resolv.conf, at `/run/host/resolv.conf` for a networked app.
    #[arg(long, default_value = "/etc/resolv.conf")]
    resolv: PathBuf,
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

    /// The child, from clone to exec. Everything that needs no input first, then the request,
    /// then what needs the UID, then the switch; argv, env and the closure are touched only
    /// once the process is the app. Returns only an error; success is exec.
    fn child(&self, channel: &OwnedFd, bytes: &[u8]) -> Result<std::convert::Infallible, String> {
        // 1. No input: handles on everything of the host's we will need, then privilege we
        // will never need again goes, then a mount namespace of our own, emptied.
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
        let resolv = clone(&self.args.resolv, ro_noexec).ok();
        let mut run: Vec<(&str, Result<OwnedFd, PathBuf>, bool)> = Vec::new();
        for (paths, writable) in [(RUN, false), (&[RUN_DOCS][..], true), (RUN_AUDIO, false)] {
            for p in paths {
                let path = Path::new(p);
                let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{p}: {e}"))?;
                // The driver link: a symlink into the store, remade as one.
                let attrs = if writable { rw_noexec } else { ro_noexec };
                let src = if meta.file_type().is_symlink() {
                    Err(std::fs::read_link(path).map_err(|e| format!("{p}: {e}"))?)
                } else {
                    Ok(clone(path, attrs)?)
                };
                run.push((p, src, writable));
            }
        }
        drv_os::creds::lock_securebits()?;
        drv_os::creds::empty_bounding_set()?;
        drv_os::creds::drop_capability(CapabilitySet::SETPCAP)?;
        rustix::thread::set_no_new_privs(true).map_err(|e| format!("no_new_privs: {e}"))?;
        // SAFETY: single-threaded.
        unsafe { drv_os::root::unshare(true) }?;
        // The per-UID directories' parents, as plain handles into this namespace's copy of
        // the old root (a handle from before the unshare would point into the parent's
        // namespace, which nothing may be cloned from): the UID's subdirectory of each is
        // cloned through them once the UID is known, while the old root is still there.
        let run_base = open_path(&self.args.runtime_base)?;
        let tmp_base = open_path(&self.args.runtime_base.join("tmp"))?;
        let state_base = open_path(&self.args.state_base)?;
        // The root: a fresh tmpfs, made our namespace's root (hung on our own runtime base
        // for the moment it takes). The old root stays stacked beneath it, reachable through
        // the handles above and nothing else, until the UID's directories are taken.
        drv_os::root::pivot(&self.args.runtime_base)?;
        let mount = |fd: OwnedFd, at: &str| drv_os::root::mount(fd, Path::new(at));
        // The fixed part:
        mount(store, &self.args.store.to_string_lossy())?;
        mount(dev, "/dev")?;
        mount(
            new_fs("tmpfs", &[("mode", "1777")], rw_noexec).map_err(|e| format!("shm: {e}"))?,
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
        let mut run_rules = Vec::new();
        let mut audio_mounts = Vec::new();
        for (p, src, writable) in run {
            let is_audio = RUN_AUDIO.contains(&p);
            match src {
                Ok(fd) if !is_audio => mount(fd, p)?,
                Ok(fd) => audio_mounts.push((p, fd)),
                Err(target) => drv_os::root::symlink(&target, Path::new(p))?,
            }
            run_rules.push((p, writable, is_audio));
        }

        // 2. The request. From here to the switch, only uid, network, gpu and audio are read.
        let Request::Launch(launch): Request =
            seq::decode(bytes).map_err(|e| format!("request: {e}"))?;
        let uid = launch.uid;
        if !self.covers(uid) {
            return Err(format!("uid {uid} is outside the app range"));
        }
        let uid_s = uid.to_string();
        if launch.network {
            rustix::thread::move_into_link_name_space(
                host_net.as_fd(),
                Some(rustix::thread::LinkNameSpaceType::Network),
            )
            .map_err(|e| format!("rejoin the network: {e}"))?;
        }
        drop(host_net);
        if launch.gpu {
            mount(
                dri.ok_or("no render node view (no GPU on this host?)")?,
                "/dev/dri",
            )?;
            mount(sys_gpu.ok_or("no sys-gpu view")?, "/sys")?;
            drop(sys);
        } else {
            mount(sys, "/sys")?;
            drop((dri, sys_gpu));
        }
        if launch.audio {
            for (p, fd) in audio_mounts {
                mount(fd, p)?;
            }
        } else {
            drop(audio_mounts);
        }
        let owned = |fs: &str, opts: &[(&str, &str)]| -> Result<OwnedFd, String> {
            let mut all = vec![("uid", uid_s.as_str()), ("gid", uid_s.as_str())];
            all.extend_from_slice(opts);
            new_fs(fs, &all, rw_noexec).map_err(|e| format!("{fs}: {e}"))
        };
        // The app's own: /etc (filled by the linker from the store), HOME (what persists at
        // .state inside), /tmp and its runtime directory (kept for the boot).
        let (u, g) = (Uid::from_raw(uid), Gid::from_raw(uid));
        mount(owned("tmpfs", &[("mode", "0755")])?, "/etc")?;
        mount(owned("tmpfs", &[("mode", "0700"), ("size", "256m")])?, HOME)?;
        let per_uid = |base: &OwnedFd, what: &str| {
            clone_tree(base, Path::new(&uid_s), rw_noexec).map_err(|e| format!("{what}/{uid}: {e}"))
        };
        // The one mount inside something the app owns: paths through its 0700 HOME resolve
        // only for it, so this runs as the app's effective UID (the capabilities stay:
        // SECBIT_NO_SETUID_FIXUP); no override capability needed.
        let own = rustix::process::getuid();
        rustix::thread::set_thread_res_uid(own, u, own).map_err(|e| format!("euid {uid}: {e}"))?;
        let state =
            per_uid(&state_base, "state").and_then(|fd| mount(fd, &format!("{HOME}/.state")));
        rustix::thread::set_thread_res_uid(own, own, own).map_err(|e| format!("euid back: {e}"))?;
        state?;
        mount(per_uid(&tmp_base, "tmp")?, "/tmp")?;
        let runtime = format!("{}/{uid}", self.args.runtime_base.display());
        mount(per_uid(&run_base, "runtime")?, &runtime)?;
        drop((run_base, tmp_base, state_base));
        if launch.network {
            let resolv = resolv.ok_or("no resolv.conf on the host")?;
            std::fs::create_dir_all("/run/host").map_err(|e| format!("/run/host: {e}"))?;
            std::fs::File::create("/run/host/resolv.conf")
                .map_err(|e| format!("resolv.conf: {e}"))?;
            attach(resolv, Path::new("/run/host/resolv.conf"))
                .map_err(|e| format!("resolv.conf: {e}"))?;
        }
        // What the manifest wants at fixed places (`/bin/sh`): links into the store, made
        // while the top level is still writable.
        let store = self.args.store.to_string_lossy();
        for (at, target) in &launch.links {
            let at = Path::new(at);
            let ok = at.is_absolute() && Path::new(target).starts_with(&*store);
            if !ok {
                return Err(format!("link {}: not absolute or not into the store", at.display()));
            }
            drv_os::root::symlink(Path::new(target), at)?;
        }
        // The old root, stacked beneath ours since the pivot: gone, with the handles into it;
        // the top level read-only.
        drv_os::root::finish(ro_noexec)?;
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

        // 3. The switch. Its own group and nothing else (set explicitly: gid 0 would see
        // through hidepid), no capability of any kind.
        drv_os::creds::switch_to(u, g, &[g], CapabilitySet::empty())?;
        rustix::process::chdir(HOME).map_err(|e| format!("chdir {HOME}: {e}"))?;

        // 4. As the app, with nothing: what it may open, then its command.
        let rules = Ruleset::new().map_err(|e| format!("landlock: {e}"))?;
        let all = rules.all();
        let allow = |path: &str, access: u64| {
            rules
                .allow(Path::new(path), access)
                .map_err(|e| format!("landlock {path}: {e}"))
        };
        let mut missing = 0;
        for path in &launch.closure {
            if !allow(path, landlock::READ | landlock::EXECUTE)? {
                missing += 1;
            }
        }
        if missing > 0 {
            drv_os::say!("drv-forker: uid {uid}: {missing} closure paths are not on this machine");
        }
        allow(
            "/dev",
            landlock::READ | landlock::WRITE_FILE | landlock::IOCTL_DEV,
        )?;
        allow("/dev/shm", all & !landlock::EXECUTE)?;
        allow("/sys", landlock::READ)?;
        allow("/proc", landlock::READ | landlock::WRITE_FILE)?;
        for path in ["/etc", HOME, "/tmp", &runtime] {
            allow(path, all)?;
        }
        if launch.network {
            allow("/run/host", landlock::READ)?;
        }
        for (p, writable, is_audio) in run_rules {
            if !is_audio || launch.audio {
                allow(p, if writable { all } else { landlock::READ })?;
            }
        }
        rules
            .restrict_self()
            .map_err(|e| format!("landlock: {e}"))?;
        refuse_syscalls(launch.userns)?;
        if !launch.jit {
            const PR_SET_MDWE: libc::c_int = 65;
            const PR_MDWE_REFUSE_EXEC_GAIN: libc::c_ulong = 1;
            // SAFETY: plain prctl.
            if unsafe { libc::prctl(PR_SET_MDWE, PR_MDWE_REFUSE_EXEC_GAIN, 0, 0, 0) } != 0 {
                return err("mdwe", io::Error::last_os_error());
            }
        }
        if launch.argv.is_empty() {
            return Err("empty argv".into());
        }
        let mut env: Vec<(String, String)> = launch.env.clone();
        env.push(("HOME".into(), HOME.into()));
        env.push(("XDG_RUNTIME_DIR".into(), runtime.clone()));
        let path = env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let exe = which(&launch.argv[0], &path)
            .ok_or_else(|| format!("{}: not found in PATH", launch.argv[0]))?;
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
        // PID 1 of the app's namespace, from here on drv-init: it links what the app
        // needs, forks the app and stays as its init. Forked goes first: after the exec there
        // is nobody left to answer, and `which` has already found the file.
        seq::send(channel, &Response::Forked, &[]).map_err(|e| format!("channel: {e}"))?;
        let mut argv_p: Vec<*const libc::c_char> = c_argv.iter().map(|s| s.as_ptr()).collect();
        argv_p.push(std::ptr::null());
        let mut env_p: Vec<*const libc::c_char> = c_env.iter().map(|s| s.as_ptr()).collect();
        env_p.push(std::ptr::null());
        // SAFETY: NUL-terminated arrays of NUL-terminated strings.
        unsafe { libc::execve(exe.as_ptr(), argv_p.as_ptr(), env_p.as_ptr()) };
        Err(format!(
            "exec {}: {}",
            launch.argv[0],
            io::Error::last_os_error()
        ))
    }
}

/// What no app gets, whatever its manifest: an executable memfd (so, with the noexec mounts
/// and `vm.memfd_noexec`, only the store runs code) and io_uring. Without `userns`, no user
/// namespace either: `unshare`, `clone` and `setns` refuse CLONE_NEWUSER and `clone3`, whose
/// flags are behind a pointer, is not there (ENOSYS, which libc falls back from). Everything
/// else passes: this is a denylist for a few doors, not the sandbox.
fn refuse_syscalls(userns: bool) -> Result<(), String> {
    use std::collections::BTreeMap;

    use seccompiler::{
        SeccompAction, SeccompCmpArgLen as Len, SeccompCmpOp as Op, SeccompCondition as Cond,
        SeccompFilter, SeccompRule,
    };
    const MFD_EXEC: u64 = 0x0010;
    let arch = std::env::consts::ARCH
        .try_into()
        .map_err(|_| "seccomp: unknown arch")?;
    let rule = |arg: u8, op: Op, value: u64| {
        SeccompRule::new(vec![
            Cond::new(arg, Len::Dword, op, value).map_err(|e| format!("seccomp: {e}"))?
        ])
        .map_err(|e| format!("seccomp: {e}"))
    };
    let newuser = libc::CLONE_NEWUSER as u64;
    let mut eperm: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    eperm.insert(
        libc::SYS_memfd_create,
        vec![rule(1, Op::MaskedEq(MFD_EXEC), MFD_EXEC)?],
    );
    for nr in [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ] {
        eperm.insert(nr, vec![]);
    }
    if !userns {
        eperm.insert(
            libc::SYS_unshare,
            vec![rule(0, Op::MaskedEq(newuser), newuser)?],
        );
        eperm.insert(
            libc::SYS_clone,
            vec![rule(0, Op::MaskedEq(newuser), newuser)?],
        );
        // setns: to a user namespace by type, or by any type (0) which a userns fd satisfies.
        eperm.insert(
            libc::SYS_setns,
            vec![
                rule(1, Op::MaskedEq(newuser), newuser)?,
                rule(1, Op::Eq, 0)?,
            ],
        );
    }
    let apply =
        |rules: BTreeMap<i64, Vec<SeccompRule>>, action: SeccompAction| -> Result<(), String> {
            let filter = SeccompFilter::new(rules, SeccompAction::Allow, action, arch)
                .map_err(|e| format!("seccomp: {e}"))?;
            let bpf: seccompiler::BpfProgram =
                filter.try_into().map_err(|e| format!("seccomp: {e}"))?;
            seccompiler::apply_filter(&bpf).map_err(|e| format!("seccomp: {e}"))
        };
    apply(eperm, SeccompAction::Errno(libc::EPERM as u32))?;
    if !userns {
        let mut enosys = BTreeMap::new();
        enosys.insert(libc::SYS_clone3, vec![]);
        apply(enosys, SeccompAction::Errno(libc::ENOSYS as u32))?;
    }
    Ok(())
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
