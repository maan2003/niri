//! drv-appd's privileged helper, kept dumb on purpose. It hears from exactly one peer, drv-appd,
//! over the socketpair the supervisor made for the two of them. Each `Launch` it checks against
//! the UID range and group list it was started with, puts the child in a per-UID cgroup,
//! sandboxes it, hands it the fds that came with the request, becomes the UID and execs. No
//! config files, no policy, no idea what an "app" is. drv-appd is the brain; a bug here is
//! reachable only through it. Zygote on Android has the same shape.

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::{io, thread};

use clap::Parser;
use drv_os::{dup_high, ensure_owned_dir, group_id};
use drv_policy::forker::{Launch, Request, Response};
use drv_policy::{seq, wire};
use rustix::fs::Mode;

#[derive(Parser)]
#[command(name = "drv-forker", about = "Fork sandboxed apps for drv-appd")]
struct Args {
    /// `start:count`: the UIDs apps may run as.
    #[arg(long)]
    range: String,
    /// A supplementary group an app may be given. Repeatable.
    #[arg(long = "group")]
    groups: Vec<String>,
    /// Per-UID `XDG_RUNTIME_DIR` parent.
    #[arg(long, default_value = "/run/drv-apps")]
    runtime_base: PathBuf,
    /// Per-UID `HOME` parent.
    #[arg(long, default_value = "/var/lib/drv-apps")]
    home_base: PathBuf,
    /// An entry of `/run` apps may see (a directory to bind mount or a symlink to recreate).
    /// Repeatable. Apps get a fresh `/run` with only these plus their own runtime directory.
    #[arg(long = "expose")]
    expose: Vec<PathBuf>,
    /// An entry of `/run` an app may ask for. Repeatable.
    #[arg(long = "expose-optional")]
    optional_expose: Vec<PathBuf>,
}

struct Forker {
    /// UIDs a request may name: `[start, start + count)`. Never root, never a service.
    start: u32,
    count: u32,
    /// Group names resolved at startup, so a request can only name what is listed here.
    groups: Vec<(String, u32)>,
    runtime_base: PathBuf,
    home_base: PathBuf,
    /// Entries of `/run` every app may see. Everything else under `/run` is hidden.
    expose: Vec<PathBuf>,
    /// Entries a request may ask for on top. Anything else asked for is refused.
    optional_expose: Vec<PathBuf>,
    /// Live children, pid to uid.
    running: Arc<Mutex<HashMap<u32, u32>>>,
}

impl Forker {
    fn covers(&self, uid: u32) -> bool {
        self.start <= uid && (uid as u64) < self.start as u64 + self.count as u64
    }

    /// Answers requests on the channel until the peer closes it (drv-appd died; the
    /// supervisor restarts us both).
    fn serve(&self, channel: OwnedFd) -> io::Result<()> {
        loop {
            let (request, fds): (Request, Vec<OwnedFd>) = match seq::recv(&channel) {
                Ok(r) => r,
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(err) => return Err(err),
            };
            let response = match request {
                Request::Launch(launch) => match self.launch(&launch, fds) {
                    Ok(pid) => Response::Forked { pid },
                    Err(err) => {
                        eprintln!("drv-forker: refused uid {}: {err}", launch.uid);
                        Response::Error(err)
                    }
                },
                Request::Running => Response::Running { uids: self.running_uids() },
            };
            seq::send(&channel, &response, &[])?;
        }
    }

    /// UIDs with a live process. From the app cgroups when we have them: apps outlive a
    /// forker (the group restarts around them), and the cgroups are how the next forker sees
    /// them. Our own children otherwise (tests).
    fn running_uids(&self) -> Vec<u32> {
        let mut uids: Vec<u32> = if rustix::process::geteuid().is_root() {
            app_cgroups_in_use().unwrap_or_default()
        } else {
            self.running.lock().unwrap().values().copied().collect()
        };
        uids.sort_unstable();
        uids.dedup();
        uids
    }

    fn launch(&self, launch: &Launch, fds: Vec<OwnedFd>) -> Result<u32, String> {
        if launch.argv.is_empty() {
            return Err("empty argv".to_owned());
        }
        let uid = launch.uid;
        let we_are_root = rustix::process::getuid().is_root();
        // Unprivileged (tests): can only fork as ourselves, with no sandbox and no cgroup.
        let as_self = !we_are_root && uid == rustix::process::getuid().as_raw();
        if !as_self && !self.covers(uid) {
            return Err(format!("uid {uid} is outside the app range"));
        }
        if !as_self && !we_are_root {
            return Err("forker is not root, can only fork as itself".to_owned());
        }
        let mut extra_expose = Vec::new();
        for path in &launch.expose {
            let path = PathBuf::from(path);
            if !self.optional_expose.contains(&path) {
                return Err(format!(
                    "{} is not on the forker's optional expose list",
                    path.display()
                ));
            }
            extra_expose.push(path);
        }
        let mut gids = Vec::new();
        for name in &launch.groups {
            let (_, gid) = self
                .groups
                .iter()
                .find(|(n, _)| n == name)
                .ok_or_else(|| format!("group {name:?} is not on the forker's list"))?;
            gids.push(*gid);
        }
        if launch.fds.len() != fds.len() {
            return Err(format!(
                "{} fd targets but {} fds attached",
                launch.fds.len(),
                fds.len()
            ));
        }
        // Copies above the target numbers, so the dup2s in the child never clobber each other.
        let mut child_fds = Vec::new();
        for (fd, target) in fds.iter().zip(&launch.fds) {
            if !(3..10).contains(target) {
                return Err(format!("fd target {target} is not in 3..10"));
            }
            child_fds.push((dup_high(fd.as_raw_fd())?, *target));
        }
        // One cgroup per app UID under our own delegated subtree, so killing an app is killing
        // a cgroup. Only as root; unprivileged (tests) has no subtree to write.
        let cgroup_procs = if we_are_root {
            Some(app_cgroup_procs(uid)?)
        } else {
            None
        };

        let mut command = Command::new(&launch.argv[0]);
        command
            .args(&launch.argv[1..])
            .env_clear()
            .envs(launch.env.iter().cloned());
        command.stdin(Stdio::null());

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
            sandbox = Some(Sandbox::plan(&expose, launch.network)?);
        }
        let mut all_gids = vec![gid];
        all_gids.extend(gids);
        let switch_uid = we_are_root;
        let dups = child_fds.clone();

        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                for (high, target) in &dups {
                    if libc::dup2(*high, *target) < 0 {
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
            .map_err(|err| format!("spawn {:?}: {err}", launch.argv[0]));
        for (high, _) in child_fds {
            // SAFETY: our duplicates for the child.
            unsafe {
                libc::close(high);
            }
        }
        let mut child = spawned?;
        let pid = child.id();
        let name = launch.argv[0].clone();
        self.running.lock().unwrap().insert(pid, uid);
        let running = Arc::clone(&self.running);
        // Reap it, or every launched app leaves a zombie under us.
        thread::spawn(move || {
            match child.wait() {
                Ok(status) => eprintln!("drv-forker: {name} (pid {pid}, uid {uid}) exited: {status}"),
                Err(err) => eprintln!("drv-forker: waiting for {name} (pid {pid}): {err}"),
            }
            running.lock().unwrap().remove(&pid);
        });
        Ok(pid)
    }

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

/// Our own cgroup v2 directory.
fn own_cgroup() -> Result<PathBuf, String> {
    let own = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| format!("/proc/self/cgroup: {e}"))?;
    // cgroup v2: a single line "0::/path".
    let path = own
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or_else(|| "not on cgroup v2".to_owned())?
        .trim();
    Ok(PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// UIDs whose `app-<uid>` cgroup has a process in it. `None` when our cgroup cannot be read.
fn app_cgroups_in_use() -> Option<Vec<u32>> {
    let entries = std::fs::read_dir(own_cgroup().ok()?).ok()?;
    let mut uids = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(uid) = name.to_str().and_then(|n| n.strip_prefix("app-")) else {
            continue;
        };
        let Ok(uid) = uid.parse::<u32>() else { continue };
        let procs = std::fs::read_to_string(entry.path().join("cgroup.procs")).ok()?;
        if procs.lines().any(|l| !l.trim().is_empty()) {
            uids.push(uid);
        }
    }
    Some(uids)
}

/// `cgroup.procs` of `<our cgroup>/app-<uid>`, created if needed. Requires cgroup v2 and a
/// delegated subtree (`Delegate=yes` on the supervisor's unit).
fn app_cgroup_procs(uid: u32) -> Result<std::fs::File, String> {
    let dir = own_cgroup()?.join(format!("app-{uid}"));
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
    let channel = wire::take().ok_or("no channel on fd 3: drv-forker runs under drv-supervisor")?;
    let (start, count) = parse_range(&args.range)?;
    let groups = args
        .groups
        .iter()
        .map(|name| Ok((name.clone(), group_id(name)?)))
        .collect::<Result<Vec<_>, String>>()?;
    let forker = Forker {
        start,
        count,
        groups,
        runtime_base: args.runtime_base,
        home_base: args.home_base,
        expose: args.expose,
        optional_expose: args.optional_expose,
        running: Default::default(),
    };
    forker.serve(channel).map_err(|e| format!("channel: {e}"))
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("drv-forker: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use drv_policy::forker::Channel;

    use super::*;
    use std::os::fd::AsFd;

    #[test]
    fn forks_as_ourselves_over_the_channel_and_refuses_the_rest() {
        let dir = std::env::temp_dir().join(format!("drv-forker-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let uid = rustix::process::getuid().as_raw();
        let forker = Forker {
            start: 0,
            count: 0,
            groups: Vec::new(),
            runtime_base: dir.join("run"),
            home_base: dir.join("home"),
            expose: Vec::new(),
            optional_expose: vec![PathBuf::from("/run/allowed")],
            running: Default::default(),
        };
        let (ours, theirs) = seq::pair().unwrap();
        let _forker = thread::spawn(move || forker.serve(theirs));
        let channel = Channel::new(ours);
        let path = std::env::var("PATH").unwrap();
        let launch = |uid, groups: Vec<&str>, argv: Vec<&str>| Launch {
            uid,
            groups: groups.into_iter().map(String::from).collect(),
            argv: argv.into_iter().map(String::from).collect(),
            env: vec![("PATH".into(), path.clone())],
            network: false,
            expose: Vec::new(),
            fds: Vec::new(),
        };

        let pid = channel
            .launch(&launch(uid, vec![], vec!["sh", "-c", "exit 0"]), &[])
            .unwrap();
        assert!(pid > 0);

        let err = channel
            .launch(&launch(uid, vec!["render"], vec!["sh"]), &[])
            .unwrap_err();
        assert!(err.to_string().contains("forker's list"), "{err}");

        let err = channel
            .launch(&launch(uid.wrapping_add(1), vec![], vec!["sh"]), &[])
            .unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");

        let err = channel
            .launch(
                &Launch {
                    expose: vec!["/run/secret".into()],
                    ..launch(uid, vec![], vec!["sh"])
                },
                &[],
            )
            .unwrap_err();
        assert!(err.to_string().contains("optional expose"), "{err}");

        // An fd for the child lands where asked: sh reads it as fd 3.
        let (a, b) = seq::pair().unwrap();
        seq::send(&a, &"hi", &[]).unwrap();
        let pid = channel
            .launch(
                &Launch {
                    fds: vec![3],
                    ..launch(uid, vec![], vec!["sh", "-c", "test -e /proc/self/fd/3"])
                },
                &[b.as_fd()],
            )
            .unwrap();
        assert!(pid > 0);

        // The channel survives refusals: still answering.
        channel
            .launch(&launch(uid, vec![], vec!["sh", "-c", "exit 0"]), &[])
            .unwrap();
        assert!(channel.running().is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
