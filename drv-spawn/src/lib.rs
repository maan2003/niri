//! The one root piece of app launching, kept dumb on purpose. It hears from exactly one peer,
//! the identity daemon it forked itself, over a socketpair nothing else can reach. Each
//! `{uid, groups, argv, env}` it checks against the UID range and group list it was started
//! with, puts the child in a per-UID cgroup, sandboxes it, becomes the UID and execs. No config
//! files, no policy, no idea what an "app" is. The identity daemon is the brain; a bug here is
//! reachable only through it.
//!
//! It also forks the two fixed services with peers, `drv-authd` and the compositor, and does
//! their wiring: every child with peers gets a wire on fd 3 (see `drv_policy::wire`) down which
//! the spawner pushes connections it made with `socketpair`. Nobody connects to anybody; the
//! spawner, which forked both ends, hands them over.
//!
//! Zygote on Android has the same shape: root, forks on command, only `system_server` talks
//! to it.

use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::{io, thread};

use drv_policy::rpc::{read_msg, write_msg};
use drv_policy::spawn::{Request, Response};
use drv_policy::wire::{self, Attach};
use rustix::fs::{Mode, OFlags};

/// `getgrnam_r`, so the command line and requests can use group names.
pub fn group_id(name: &str) -> Result<u32, String> {
    let cname = CString::new(name).map_err(|_| format!("bad group name {name:?}"))?;
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 16 * 1024];
    let mut result: *mut libc::group = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the call; buf outlives the use of `grp`.
    let rc = unsafe {
        libc::getgrnam_r(
            cname.as_ptr(),
            &mut grp,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        return Err(format!(
            "getgrnam {name:?}: {}",
            io::Error::from_raw_os_error(rc)
        ));
    }
    if result.is_null() {
        return Err(format!("no such group {name:?}"));
    }
    Ok(grp.gr_gid)
}

/// `getpwnam_r`: a user's uid and primary gid.
pub fn user_ids(name: &str) -> Result<(u32, u32), String> {
    let cname = CString::new(name).map_err(|_| format!("bad user name {name:?}"))?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the call; buf outlives the use of `pwd`.
    let rc = unsafe {
        libc::getpwnam_r(
            cname.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        return Err(format!(
            "getpwnam {name:?}: {}",
            io::Error::from_raw_os_error(rc)
        ));
    }
    if result.is_null() {
        return Err(format!("no such user {name:?}"));
    }
    Ok((pwd.pw_uid, pwd.pw_gid))
}

/// Every group of a user (`getgrouplist`), for services that keep their supplementary groups.
pub fn user_groups(name: &str, gid: u32) -> Result<Vec<u32>, String> {
    let cname = CString::new(name).map_err(|_| format!("bad user name {name:?}"))?;
    let mut n: libc::c_int = 64;
    loop {
        let mut groups = vec![0 as libc::gid_t; n as usize];
        // SAFETY: the buffer holds `n` gids; the call writes at most that many.
        let rc = unsafe { libc::getgrouplist(cname.as_ptr(), gid, groups.as_mut_ptr(), &mut n) };
        if rc >= 0 {
            groups.truncate(n as usize);
            return Ok(groups);
        }
        if n as usize <= groups.len() {
            return Err(format!("getgrouplist {name:?} failed"));
        }
    }
}

pub struct Server {
    /// UIDs a request may name: `[start, start + count)`. Never root, never a service.
    pub start: u32,
    pub count: u32,
    /// Group names resolved at startup, so a request can only name what is listed here.
    pub groups: Vec<(String, u32)>,
    /// `<runtime_base>/<uid>` becomes the child's `XDG_RUNTIME_DIR`.
    pub runtime_base: PathBuf,
    /// `<home_base>/<uid>` becomes the child's `HOME` and working directory.
    pub home_base: PathBuf,
    /// Entries of `/run` every app may see, e.g. the apps' Wayland socket directory or the
    /// `opengl-driver` symlink. Everything else under `/run` is hidden (see [`Sandbox`]).
    pub expose: Vec<PathBuf>,
    /// Entries a request may ask for on top (the services' bus directory for the desktop
    /// services). Anything else asked for is refused.
    pub optional_expose: Vec<PathBuf>,
    /// The wires to the auth daemon and the compositor, for apps that get a peer.
    pub wiring: Wiring,
}

/// The spawner's ends of the services' wires, and the links it makes between them.
#[derive(Default)]
pub struct Wiring(Mutex<WiringInner>);

#[derive(Default)]
struct WiringInner {
    authd: Option<OwnedFd>,
    compositor: Option<OwnedFd>,
}

impl Wiring {
    /// A service came up (again): keep its wire and link it to the other one if that is up.
    pub fn attach_service(&self, service: Peer, wire: OwnedFd) {
        let mut inner = self.0.lock().unwrap();
        match service {
            Peer::Authd => inner.authd = Some(wire),
            Peer::Compositor => inner.compositor = Some(wire),
        }
        inner.link();
    }

    pub fn detach_service(&self, service: Peer) {
        let mut inner = self.0.lock().unwrap();
        match service {
            Peer::Authd => inner.authd = None,
            Peer::Compositor => inner.compositor = None,
        }
    }

    /// A fresh verifier connection: the daemon gets one end, the app's end comes back. `None`
    /// when the daemon is not up, so the app finds an empty wire.
    fn verifier(&self) -> Option<OwnedFd> {
        let mut inner = self.0.lock().unwrap();
        let authd = inner.authd.as_ref()?;
        let (app_end, daemon_end) = match wire::pair() {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("drv-spawnd: socketpair: {err}");
                return None;
            }
        };
        if let Err(err) = wire::send_attach(authd, Attach::Verifier, daemon_end.as_fd()) {
            eprintln!("drv-spawnd: drv-authd's wire: {err}");
            inner.authd = None;
            return None;
        }
        Some(app_end)
    }
}

impl WiringInner {
    fn link(&mut self) {
        let (Some(authd), Some(compositor)) = (&self.authd, &self.compositor) else {
            return;
        };
        let (compositor_end, daemon_end) = match wire::pair() {
            Ok(pair) => pair,
            Err(err) => {
                eprintln!("drv-spawnd: socketpair: {err}");
                return;
            }
        };
        if let Err(err) = wire::send_attach(authd, Attach::Compositor, daemon_end.as_fd()) {
            eprintln!("drv-spawnd: drv-authd's wire: {err}");
            self.authd = None;
            return;
        }
        if let Err(err) = wire::send_attach(compositor, Attach::Auth, compositor_end.as_fd()) {
            eprintln!("drv-spawnd: the compositor's wire: {err}");
            self.compositor = None;
        }
    }
}

/// The two services the spawner forks itself and wires together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    Authd,
    Compositor,
}

/// A service the spawner forks and keeps running: its own user, no sandbox (it is trusted and
/// needs the real `/run`), a wire on fd 3.
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

/// `fcntl(F_DUPFD_CLOEXEC)` at 10 or above, so a `dup2` onto a low target is never a same-fd
/// no-op (which would keep close-on-exec set).
pub fn dup_high(fd: i32) -> Result<i32, String> {
    // SAFETY: plain fcntl on an fd we own.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if new < 0 {
        return Err(format!("dup fd {fd}: {}", io::Error::last_os_error()));
    }
    Ok(new)
}

/// Forks a service with a fresh wire on fd 3; returns the child and our end of the wire.
pub fn start_service(service: &Service) -> Result<(Child, OwnedFd), String> {
    for (dir, mode) in &service.dirs {
        ensure_owned_dir(dir, service.uid, service.gid, *mode)?;
    }
    if service.argv.is_empty() {
        return Err(format!("service {}: empty command", service.name));
    }
    let (child_end, ours) = wire::pair().map_err(|e| format!("socketpair: {e}"))?;
    let wire_fd = dup_high(child_end.as_raw_fd())?;
    let mut command = Command::new(&service.argv[0]);
    command
        .args(&service.argv[1..])
        .env_clear()
        .envs(service.env.iter().cloned())
        .env(wire::WIRE_ENV, wire::WIRE_FD.to_string())
        .stdin(Stdio::null());
    let (uid, gid) = (service.uid, service.gid);
    let groups = service.groups.clone();
    // SAFETY: only dup2/setgroups/setresgid/setresuid/prctl between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(wire_fd, wire::WIRE_FD) < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::setgroups(groups.len(), groups.as_ptr()) != 0
                || libc::setresgid(gid, gid, gid) != 0
                || libc::setresuid(uid, uid, uid) != 0
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
        .map_err(|e| format!("spawn {}: {e}", service.name))?;
    // SAFETY: our duplicate for the child; the child has its own now.
    unsafe {
        libc::close(wire_fd);
    }
    Ok((child, ours))
}

/// Creates `path` (parents too) owned by `uid:gid` with `mode`, or fixes an existing one.
pub fn ensure_owned_dir(path: &Path, uid: u32, gid: u32, mode: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    match rustix::fs::mkdir(path, Mode::from_raw_mode(mode)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(err) => return Err(format!("mkdir {}: {err}", path.display())),
    }
    let fd = rustix::fs::open(path, OFlags::DIRECTORY | OFlags::NOFOLLOW, Mode::empty())
        .map_err(|err| format!("open {}: {err}", path.display()))?;
    rustix::fs::fchown(
        &fd,
        Some(rustix::process::Uid::from_raw(uid)),
        Some(rustix::process::Gid::from_raw(gid)),
    )
    .map_err(|err| format!("chown {}: {err}", path.display()))?;
    rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode))
        .map_err(|err| format!("chmod {}: {err}", path.display()))
}

impl Server {
    fn covers(&self, uid: u32) -> bool {
        self.start <= uid && (uid as u64) < self.start as u64 + self.count as u64
    }

    /// Answers requests on the channel until the peer closes it (the identity daemon died:
    /// the supervisor forks a new one).
    pub fn serve(&self, stream: UnixStream) -> io::Result<()> {
        loop {
            let request: Request = match read_msg(&stream) {
                Ok(request) => request,
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(err) => return Err(err),
            };
            let response = match self.launch(&request) {
                Ok(pid) => Response::Forked { pid },
                Err(err) => {
                    eprintln!("drv-spawnd: refused uid {}: {err}", request.uid);
                    Response::Error(err)
                }
            };
            write_msg(&stream, &response)?;
        }
    }

    fn launch(&self, request: &Request) -> Result<u32, String> {
        if request.argv.is_empty() {
            return Err("empty argv".to_owned());
        }
        let uid = request.uid;
        let we_are_root = rustix::process::getuid().is_root();
        // Unprivileged (tests): can only fork as ourselves, with no sandbox and no cgroup.
        let as_self = !we_are_root && uid == rustix::process::getuid().as_raw();
        if !as_self && !self.covers(uid) {
            return Err(format!("uid {uid} is outside the app range"));
        }
        if !as_self && !we_are_root {
            return Err("spawner is not root, can only fork as itself".to_owned());
        }
        let mut extra_expose = Vec::new();
        for path in &request.expose {
            let path = PathBuf::from(path);
            if !self.optional_expose.contains(&path) {
                return Err(format!(
                    "{} is not on the spawner's optional expose list",
                    path.display()
                ));
            }
            extra_expose.push(path);
        }
        let mut gids = Vec::new();
        for name in &request.groups {
            let (_, gid) = self
                .groups
                .iter()
                .find(|(n, _)| n == name)
                .ok_or_else(|| format!("group {name:?} is not on the spawner's list"))?;
            gids.push(*gid);
        }
        // One cgroup per app UID under our own delegated subtree, so killing an app is killing
        // a cgroup. Only as root; unprivileged (tests) has no subtree to write.
        let cgroup_procs = if we_are_root {
            Some(app_cgroup_procs(uid)?)
        } else {
            None
        };

        let mut command = Command::new(&request.argv[0]);
        command
            .args(&request.argv[1..])
            .env_clear()
            .envs(request.env.iter().cloned());
        command.stdin(Stdio::null());
        // The app's wire, with its auth connection already on it (or nothing, if the daemon
        // is down: the app sees the wire close).
        let mut wire_fd = None;
        let _child_wire;
        if request.auth {
            let (child_end, ours) = wire::pair().map_err(|e| format!("socketpair: {e}"))?;
            match self.wiring.verifier() {
                Some(auth) => wire::send_attach(&ours, Attach::Auth, auth.as_fd())
                    .map_err(|e| format!("attaching auth: {e}"))?,
                None => eprintln!("drv-spawnd: uid {uid} asked for auth but drv-authd is down"),
            }
            wire_fd = Some(dup_high(child_end.as_raw_fd())?);
            _child_wire = child_end;
            command.env(wire::WIRE_ENV, wire::WIRE_FD.to_string());
        }

        let gid = if as_self {
            rustix::process::getgid().as_raw()
        } else {
            uid
        };
        let mut sandbox = None;
        if !as_self {
            let runtime = self.owned_dir(&self.runtime_base, uid, gid)?;
            let home = self.owned_dir(&self.home_base, uid, gid)?;
            command.env("XDG_RUNTIME_DIR", &runtime).env("HOME", &home);
            command.current_dir(&home);
            let mut expose = self.expose.clone();
            expose.extend(extra_expose);
            expose.push(runtime);
            sandbox = Some(Sandbox::plan(&expose, request.network)?);
        }
        let mut all_gids = vec![gid];
        all_gids.extend(gids);
        let switch_uid = we_are_root;

        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if let Some(wire_fd) = wire_fd {
                    if libc::dup2(wire_fd, wire::WIRE_FD) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                if let Some(sandbox) = &sandbox {
                    sandbox.apply()?;
                }
                if let Some(procs) = &cgroup_procs {
                    // "0" means the writing process itself.
                    let mut procs = procs;
                    use std::io::Write as _;
                    procs.write_all(b"0")?;
                }
                if switch_uid {
                    if libc::setgroups(all_gids.len(), all_gids.as_ptr()) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::setresgid(gid, gid, gid) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::setresuid(uid, uid, uid) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::getuid() != uid || libc::geteuid() != uid {
                        return Err(io::Error::other("uid did not change"));
                    }
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let spawned = command
            .spawn()
            .map_err(|err| format!("spawn {:?}: {err}", request.argv[0]));
        if let Some(wire_fd) = wire_fd {
            // SAFETY: our duplicate for the child.
            unsafe {
                libc::close(wire_fd);
            }
        }
        let mut child = spawned?;
        let pid = child.id();
        let name = request.argv[0].clone();
        // Reap it, or every launched app leaves a zombie under us.
        thread::spawn(move || match child.wait() {
            Ok(status) => eprintln!("drv-spawnd: {name} (pid {pid}, uid {uid}) exited: {status}"),
            Err(err) => eprintln!("drv-spawnd: waiting for {name} (pid {pid}): {err}"),
        });
        Ok(pid)
    }
}

/// The app's private view of the filesystem, decided before fork and applied between fork and
/// exec (only syscalls on pre-built strings; nothing allocates there). Same UID plus this is
/// the floor every app gets; what it may reach on top is groups and sockets.
///
/// - a new mount namespace, so none of it leaks out, and unless the app was granted the network a
///   new network namespace with nothing in it;
/// - `/tmp` and `/dev/shm` are fresh tmpfs: no shared scratch space between apps;
/// - `/proc` shows only the app's own processes;
/// - `/run` is a fresh, read-only tmpfs holding only the exposed entries: no system D-Bus, no
///   identity socket unless exposed, no other app's runtime directory, no setuid wrappers.
struct Sandbox {
    /// `unshare(CLONE_NEWNET)` too: no interfaces at all.
    no_network: bool,
    /// Directories to create in the staging tmpfs, parents first.
    dirs: Vec<CString>,
    /// `(source, target)` bind mounts into the staging tmpfs.
    binds: Vec<(CString, CString)>,
    /// `(link target, link path)` symlinks recreated in the staging tmpfs.
    symlinks: Vec<(CString, CString)>,
}

const STAGE: &str = "/tmp/.run";

impl Sandbox {
    fn plan(expose: &[PathBuf], network: bool) -> Result<Self, String> {
        let cstr = |p: &Path| {
            CString::new(p.as_os_str().as_bytes()).map_err(|_| format!("NUL in {}", p.display()))
        };
        let mut dirs = Vec::new();
        let mut binds = Vec::new();
        let mut symlinks = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for path in expose {
            let rel = path
                .strip_prefix("/run")
                .map_err(|_| format!("--expose {}: not under /run", path.display()))?;
            if rel.as_os_str().is_empty() {
                return Err("--expose /run: exposing everything defeats the sandbox".to_owned());
            }
            let staged = Path::new(STAGE).join(rel);
            // Parents inside the stage, outermost first.
            let mut parents: Vec<_> = staged.ancestors().skip(1).collect();
            parents.reverse();
            for parent in parents {
                if parent.starts_with(STAGE) && seen.insert(parent.to_owned()) {
                    dirs.push(cstr(parent)?);
                }
            }
            let meta = std::fs::symlink_metadata(path)
                .map_err(|e| format!("--expose {}: {e}", path.display()))?;
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(path)
                    .map_err(|e| format!("readlink {}: {e}", path.display()))?;
                symlinks.push((cstr(&target)?, cstr(&staged)?));
            } else if meta.is_dir() {
                if seen.insert(staged.clone()) {
                    dirs.push(cstr(&staged)?);
                }
                binds.push((cstr(path)?, cstr(&staged)?));
            } else {
                return Err(format!(
                    "--expose {}: only directories and symlinks",
                    path.display()
                ));
            }
        }
        Ok(Self {
            no_network: !network,
            dirs,
            binds,
            symlinks,
        })
    }

    /// Runs as root in the child. On failure the step's name goes to stderr (the forker's
    /// journal) and the errno comes back to the parent.
    fn apply(&self) -> io::Result<()> {
        // Written by hand rather than through a helper closure so every string is a literal.
        fn fail(step: &'static str) -> io::Result<()> {
            let err = io::Error::last_os_error();
            let msg = b"drv-spawnd: sandbox: ";
            // SAFETY: plain write(2) of static bytes.
            unsafe {
                libc::write(2, msg.as_ptr().cast(), msg.len());
                libc::write(2, step.as_ptr().cast(), step.len());
                libc::write(2, b"\n".as_ptr().cast(), 1);
            }
            Err(err)
        }
        let root = c"/";
        let tmpfs = c"tmpfs";
        let mode1777 = c"mode=1777";
        let mode0755 = c"mode=0755";
        let proc_ = c"proc";
        let hidepid = c"hidepid=invisible";
        let tmp = c"/tmp";
        let shm = c"/dev/shm";
        let procdir = c"/proc";
        let stage = c"/tmp/.run";
        let run = c"/run";
        let none: *const libc::c_char = std::ptr::null();
        let mnt = |src: &CStr,
                   dst: &CStr,
                   fstype: *const libc::c_char,
                   flags: libc::c_ulong,
                   data: *const libc::c_char| {
            // SAFETY: all pointers are valid C strings (or null where the kernel allows it).
            unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), fstype, flags, data.cast()) }
        };
        let nodev = libc::MS_NOSUID | libc::MS_NODEV;
        let flags = libc::CLONE_NEWNS
            | if self.no_network {
                libc::CLONE_NEWNET
            } else {
                0
            };
        // SAFETY: syscalls only.
        unsafe {
            if libc::unshare(flags) != 0 {
                return fail("unshare(CLONE_NEWNS | CLONE_NEWNET)");
            }
        }
        if mnt(c"none", root, none, libc::MS_REC | libc::MS_PRIVATE, none) != 0 {
            return fail("make / private");
        }
        if mnt(tmpfs, tmp, tmpfs.as_ptr(), nodev, mode1777.as_ptr()) != 0 {
            return fail("tmpfs on /tmp");
        }
        if mnt(tmpfs, shm, tmpfs.as_ptr(), nodev, mode1777.as_ptr()) != 0 {
            return fail("tmpfs on /dev/shm");
        }
        if mnt(
            proc_,
            procdir,
            proc_.as_ptr(),
            nodev | libc::MS_NOEXEC,
            hidepid.as_ptr(),
        ) != 0
        {
            return fail("proc with hidepid");
        }
        // SAFETY: syscalls on static strings.
        unsafe {
            if libc::mkdir(stage.as_ptr(), 0o755) != 0 {
                return fail("mkdir stage");
            }
        }
        if mnt(tmpfs, stage, tmpfs.as_ptr(), nodev, mode0755.as_ptr()) != 0 {
            return fail("tmpfs on stage");
        }
        for dir in &self.dirs {
            // SAFETY: valid C string; EEXIST is fine (shared parents).
            let rc = unsafe { libc::mkdir(dir.as_ptr(), 0o755) };
            if rc != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return fail("mkdir in stage");
            }
        }
        for (src, dst) in &self.binds {
            if mnt(src, dst, none, libc::MS_BIND | libc::MS_REC, none) != 0 {
                return fail("bind mount into stage");
            }
        }
        for (target, link) in &self.symlinks {
            // SAFETY: valid C strings.
            if unsafe { libc::symlink(target.as_ptr(), link.as_ptr()) } != 0 {
                return fail("symlink in stage");
            }
        }
        if mnt(stage, run, none, libc::MS_MOVE, none) != 0 {
            return fail("move stage to /run");
        }
        // SAFETY: syscall on a static string; the stage directory is empty after the move.
        unsafe {
            libc::rmdir(stage.as_ptr());
        }
        // The tmpfs itself read-only; the bind mounts inside keep their own flags.
        if mnt(
            c"none",
            run,
            none,
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | nodev,
            none,
        ) != 0
        {
            return fail("remount /run read-only");
        }
        Ok(())
    }
}

/// `cgroup.procs` of `<our cgroup>/app-<uid>`, created if needed. Requires cgroup v2 and a
/// delegated subtree (`Delegate=yes` on the spawner's unit).
fn app_cgroup_procs(uid: u32) -> Result<std::fs::File, String> {
    let own = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| format!("/proc/self/cgroup: {e}"))?;
    // cgroup v2: a single line "0::/path".
    let path = own
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or_else(|| "not on cgroup v2".to_owned())?
        .trim();
    let dir = PathBuf::from("/sys/fs/cgroup")
        .join(path.trim_start_matches('/'))
        .join(format!("app-{uid}"));
    match std::fs::create_dir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("mkdir {}: {e}", dir.display())),
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(dir.join("cgroup.procs"))
        .map_err(|e| format!("open {}/cgroup.procs: {e}", dir.display()))
}

impl Server {
    /// `<base>/<uid>`, mode 0700, owned by the UID. Created on first launch.
    fn owned_dir(&self, base: &Path, uid: u32, gid: u32) -> Result<PathBuf, String> {
        let dir = base.join(uid.to_string());
        match rustix::fs::mkdir(base, Mode::from_raw_mode(0o711)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(err) => return Err(format!("mkdir {}: {err}", base.display())),
        }
        ensure_owned_dir(&dir, uid, gid, 0o700)?;
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use drv_policy::spawn::Channel;

    use super::*;

    #[test]
    fn forks_as_ourselves_over_the_channel_and_refuses_the_rest() {
        let dir = std::env::temp_dir().join(format!("drv-spawn-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let uid = rustix::process::getuid().as_raw();
        let server = Server {
            start: 0,
            count: 0,
            groups: Vec::new(),
            runtime_base: dir.join("run"),
            home_base: dir.join("home"),
            expose: Vec::new(),
            optional_expose: vec![PathBuf::from("/run/allowed")],
            wiring: Wiring::default(),
        };
        let (ours, theirs) = UnixStream::pair().unwrap();
        let _server = thread::spawn(move || server.serve(theirs));
        let channel = Channel::new(ours);
        let path = std::env::var("PATH").unwrap();
        let request = |uid, groups: Vec<&str>, argv: Vec<&str>| Request {
            uid,
            groups: groups.into_iter().map(String::from).collect(),
            argv: argv.into_iter().map(String::from).collect(),
            env: vec![("PATH".into(), path.clone())],
            network: false,
            expose: Vec::new(),
            auth: false,
        };

        let pid = channel
            .fork(&request(uid, vec![], vec!["sh", "-c", "exit 0"]))
            .unwrap();
        assert!(pid > 0);

        let err = channel
            .fork(&request(uid, vec!["render"], vec!["sh"]))
            .unwrap_err();
        assert!(err.to_string().contains("spawner's list"), "{err}");

        let err = channel
            .fork(&request(uid.wrapping_add(1), vec![], vec!["sh"]))
            .unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");

        let err = channel
            .fork(&Request {
                expose: vec!["/run/secret".into()],
                ..request(uid, vec![], vec!["sh"])
            })
            .unwrap_err();
        assert!(err.to_string().contains("optional expose"), "{err}");

        // The channel survives refusals: still answering.
        channel
            .fork(&request(uid, vec![], vec!["sh", "-c", "exit 0"]))
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolves_groups_and_users() {
        assert_eq!(group_id("root").unwrap(), 0);
        assert!(group_id("no-such-group-xyz").is_err());
        assert_eq!(user_ids("root").unwrap(), (0, 0));
        assert!(user_ids("no-such-user-xyz").is_err());
    }
}
