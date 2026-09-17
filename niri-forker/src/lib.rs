//! The one root piece of app launching, kept dumb on purpose: it takes `{uid, groups, argv,
//! env}` from an allowed peer, checks the UID and groups are ones that peer may hand out, puts
//! the child in a per-UID cgroup, becomes the UID and execs. No config files, no policy, no idea
//! what an "app" is. The identity daemon is the brain; a bug here is reachable only through it.
//!
//! Zygote on Android has the same shape: root, forks on command, only `system` may connect.

use std::ffi::{CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{io, thread};

use niri_policy::rpc::{read_msg, write_msg};
use rustix::fs::{Mode, OFlags};
use rustix::net::UCred;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub uid: u32,
    /// Supplementary group names (`render` for the GPU). Each must be on the peer's allow list.
    pub groups: Vec<String>,
    /// `argv[0]` is looked up in `PATH` from `env`.
    pub argv: Vec<String>,
    /// The child's whole environment, plus `HOME` and `XDG_RUNTIME_DIR` which the forker sets
    /// for range UIDs.
    pub env: Vec<(String, String)>,
    /// Keep the host network. Otherwise the child gets a new, empty network namespace.
    #[serde(default)]
    pub network: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Forked { pid: u32 },
    Error(String),
}

/// One connection, one request. The identity daemon calls this per launch.
pub fn fork(socket: &Path, request: &Request) -> io::Result<u32> {
    let stream = UnixStream::connect(socket)?;
    write_msg(&stream, request)?;
    match read_msg::<Response>(&stream)? {
        Response::Forked { pid } => Ok(pid),
        Response::Error(err) => Err(io::Error::other(err)),
    }
}

/// Who may ask, for which UIDs, and which supplementary groups they may hand out. A peer may
/// always fork as itself (the child still goes through the UID switch: it is never root).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowed {
    pub peer: u32,
    pub start: u32,
    pub count: u32,
    /// Group names resolved at startup, so a request can only name what is listed here.
    pub groups: Vec<(String, u32)>,
}

impl Allowed {
    /// `peer:start:count[:group,group]`, e.g. `1000:100000:65536:render`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let parts: Vec<_> = s.split(':').collect();
        let (peer, start, count, groups) = match parts.as_slice() {
            [peer, start, count] => (peer, start, count, ""),
            [peer, start, count, groups] => (peer, start, count, *groups),
            _ => return Err(format!("expected peer:start:count[:groups], got {s:?}")),
        };
        let num = |x: &str| x.parse::<u32>().map_err(|e| format!("{x:?}: {e}"));
        let groups = groups
            .split(',')
            .filter(|g| !g.is_empty())
            .map(|name| Ok((name.to_owned(), group_id(name)?)))
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            peer: num(peer)?,
            start: num(start)?,
            count: num(count)?,
            groups,
        })
    }

    fn covers(&self, uid: u32) -> bool {
        self.start <= uid && (uid as u64) < self.start as u64 + self.count as u64
    }
}

/// `getgrnam_r`, so `--allow` and requests can use names.
pub fn group_id(name: &str) -> Result<u32, String> {
    let cname = std::ffi::CString::new(name).map_err(|_| format!("bad group name {name:?}"))?;
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

pub struct Server {
    pub allowed: Vec<Allowed>,
    /// `<runtime_base>/<uid>` becomes the child's `XDG_RUNTIME_DIR`.
    pub runtime_base: PathBuf,
    /// `<home_base>/<uid>` becomes the child's `HOME` and working directory.
    pub home_base: PathBuf,
    /// Entries of `/run` an app may see, e.g. the apps' Wayland socket directory or the
    /// `opengl-driver` symlink. Everything else under `/run` is hidden (see [`Sandbox`]).
    pub expose: Vec<PathBuf>,
}

impl Server {
    pub fn serve(self, listener: UnixListener) -> io::Result<()> {
        let server = std::sync::Arc::new(self);
        loop {
            let (stream, _) = listener.accept()?;
            let server = server.clone();
            thread::spawn(move || {
                let _ = server.handle(stream);
            });
        }
    }

    pub fn handle(&self, stream: UnixStream) -> io::Result<()> {
        let peer = rustix::net::sockopt::socket_peercred(&stream)?;
        let request: Request = read_msg(&stream)?;
        let response = match self.launch(&peer, &request) {
            Ok(pid) => Response::Forked { pid },
            Err(err) => {
                eprintln!(
                    "niri-forker: refused uid {} from peer uid {}: {err}",
                    request.uid,
                    peer.uid.as_raw()
                );
                Response::Error(err)
            }
        };
        write_msg(&stream, &response)
    }

    fn launch(&self, peer: &UCred, request: &Request) -> Result<u32, String> {
        let peer_uid = peer.uid.as_raw();
        let allowed = self
            .allowed
            .iter()
            .find(|a| a.peer == peer_uid)
            .ok_or_else(|| format!("uid {peer_uid} may not use the forker"))?;
        if request.argv.is_empty() {
            return Err("empty argv".to_owned());
        }

        let uid = request.uid;
        let as_self = uid == peer_uid;
        if !as_self && !allowed.covers(uid) {
            return Err(format!("uid {uid} is outside the peer's range"));
        }
        let we_are_root = rustix::process::getuid().is_root();
        if !as_self && !we_are_root {
            return Err("forker is not root, can only fork as the peer itself".to_owned());
        }
        let mut gids = Vec::new();
        for name in &request.groups {
            let (_, gid) = allowed
                .groups
                .iter()
                .find(|(n, _)| n == name)
                .ok_or_else(|| format!("group {name:?} is not on the peer's allow list"))?;
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

        let gid = if as_self { peer.gid.as_raw() } else { uid };
        let mut sandbox = None;
        if !as_self {
            let runtime = self.owned_dir(&self.runtime_base, uid, gid)?;
            let home = self.owned_dir(&self.home_base, uid, gid)?;
            command.env("XDG_RUNTIME_DIR", &runtime).env("HOME", &home);
            command.current_dir(&home);
            if we_are_root {
                let mut expose = self.expose.clone();
                expose.push(runtime);
                sandbox = Some(Sandbox::plan(&expose, request.network)?);
            }
        }
        let mut all_gids = vec![gid];
        all_gids.extend(gids);
        // Root always becomes the requested UID, including a peer forking "as itself": the
        // child must never keep our privileges. Unprivileged (tests) has nothing to drop.
        let switch_uid = we_are_root;

        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
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

        let mut child = command
            .spawn()
            .map_err(|err| format!("spawn {:?}: {err}", request.argv[0]))?;
        let pid = child.id();
        let name = request.argv[0].clone();
        // Reap it, or every launched app leaves a zombie under us.
        thread::spawn(move || match child.wait() {
            Ok(status) => eprintln!("niri-forker: {name} (pid {pid}, uid {uid}) exited: {status}"),
            Err(err) => eprintln!("niri-forker: waiting for {name} (pid {pid}): {err}"),
        });
        Ok(pid)
    }
}

/// The app's private view of the filesystem, decided before fork and applied between fork and
/// exec (only syscalls on pre-built strings; nothing allocates there). Same UID plus this is
/// the floor every app gets; what it may reach on top is groups and sockets.
///
/// - a new mount namespace, so none of it leaks out, and unless the app was granted the
///   network a new network namespace with nothing in it;
/// - `/tmp` and `/dev/shm` are fresh tmpfs: no shared scratch space between apps;
/// - `/proc` shows only the app's own processes;
/// - `/run` is a fresh, read-only tmpfs holding only the exposed entries: no system D-Bus,
///   no forker or identity sockets, no other app's runtime directory, no setuid wrappers.
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
            let msg = b"niri-forker: sandbox: ";
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
        let mnt = |src: &CStr, dst: &CStr, fstype: *const libc::c_char, flags: libc::c_ulong, data: *const libc::c_char| {
            // SAFETY: all pointers are valid C strings (or null where the kernel allows it).
            unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), fstype, flags, data.cast()) }
        };
        let nodev = libc::MS_NOSUID | libc::MS_NODEV;
        let flags = libc::CLONE_NEWNS | if self.no_network { libc::CLONE_NEWNET } else { 0 };
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
        if mnt(proc_, procdir, proc_.as_ptr(), nodev | libc::MS_NOEXEC, hidepid.as_ptr()) != 0 {
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
        if mnt(c"none", run, none, libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | nodev, none) != 0 {
            return fail("remount /run read-only");
        }
        Ok(())
    }
}

/// `cgroup.procs` of `<our cgroup>/app-<uid>`, created if needed. Requires cgroup v2 and a
/// delegated subtree (`Delegate=yes` on the forker's unit).
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
        let mkdir = |path: &Path, mode: Mode| match rustix::fs::mkdir(path, mode) {
            Ok(()) => Ok(()),
            Err(rustix::io::Errno::EXIST) => Ok(()),
            Err(err) => Err(format!("mkdir {}: {err}", path.display())),
        };
        mkdir(base, Mode::from_raw_mode(0o711))?;
        mkdir(&dir, Mode::from_raw_mode(0o700))?;
        let fd = rustix::fs::open(&dir, OFlags::DIRECTORY | OFlags::NOFOLLOW, Mode::empty())
            .map_err(|err| format!("open {}: {err}", dir.display()))?;
        rustix::fs::fchown(
            &fd,
            Some(rustix::process::Uid::from_raw(uid)),
            Some(rustix::process::Gid::from_raw(gid)),
        )
        .map_err(|err| format!("chown {}: {err}", dir.display()))?;
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700))
            .map_err(|err| format!("chmod {}: {err}", dir.display()))?;
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_for_us(dir: &Path) -> (PathBuf, thread::JoinHandle<()>) {
        let socket = dir.join("forker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = Server {
            allowed: vec![Allowed {
                peer: rustix::process::getuid().as_raw(),
                start: 0,
                count: 0,
                groups: Vec::new(),
            }],
            runtime_base: dir.join("run"),
            home_base: dir.join("home"),
            expose: Vec::new(),
        };
        let handle = thread::spawn(move || {
            let _ = server.serve(listener);
        });
        (socket, handle)
    }

    #[test]
    fn forks_as_ourselves_and_refuses_other_uids() {
        let dir = std::env::temp_dir().join(format!("niri-forker-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (socket, _server) = server_for_us(&dir);
        let uid = rustix::process::getuid().as_raw();
        let path = std::env::var("PATH").unwrap();

        let pid = fork(
            &socket,
            &Request {
                uid,
                groups: Vec::new(),
                argv: vec!["sh".into(), "-c".into(), "exit 0".into()],
                env: vec![("PATH".into(), path.clone())],
                network: false,
            },
        )
        .unwrap();
        assert!(pid > 0);

        let err = fork(
            &socket,
            &Request {
                uid,
                groups: vec!["render".into()],
                argv: vec!["sh".into()],
                env: vec![("PATH".into(), path.clone())],
                network: false,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("allow list"), "{err}");

        let err = fork(
            &socket,
            &Request {
                uid: uid.wrapping_add(1),
                groups: Vec::new(),
                argv: vec!["sh".into()],
                env: vec![("PATH".into(), path)],
                network: false,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_allow() {
        assert_eq!(
            Allowed::parse("1000:100000:65536").unwrap(),
            Allowed {
                peer: 1000,
                start: 100000,
                count: 65536,
                groups: Vec::new(),
            }
        );
        assert!(Allowed::parse("1000:100000").is_err());
        assert!(Allowed::parse("1000:1:1:no-such-group-xyz").is_err());
        // Every system has group 0.
        let root = Allowed::parse("1000:1:1:root").unwrap();
        assert_eq!(root.groups, vec![("root".to_owned(), 0)]);
    }
}
