//! drv-appd's privileged helper, kept dumb on purpose. It hears from exactly one peer, drv-appd,
//! over the socketpair the supervisor made for the two of them. Each `Launch` it checks against
//! the UID range and group list it was started with, puts the child in a per-UID cgroup,
//! sandboxes it, becomes the UID and execs. Apps get no fds: nothing forked here holds
//! authority. No config files, no policy, no idea what an "app" is. drv-appd is the brain; a bug here is
//! reachable only through it. Zygote on Android has the same shape.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::{io, thread};

use clap::Parser;
use drv_os::approot::{AppRoot, RunEntry, Spec};
use drv_os::{ensure_owned_dir, own_cgroup};
use drv_policy::forker::{Launch, Request, Response};
use drv_policy::seq;
use rustix::fs::{Gid, Mode, Uid};
use rustix::thread::{CapabilitiesSecureBits, CapabilitySet, CapabilitySets};

/// What forking an app takes when we are not root: the sandbox (SYS_ADMIN), the UID switch
/// (SETUID, SETGID), the app's directories (CHOWN) and dropping our own bounding set in the
/// child (SETPCAP).
const NEEDED: CapabilitySet = CapabilitySet::SYS_ADMIN
    .union(CapabilitySet::SETUID)
    .union(CapabilitySet::SETGID)
    .union(CapabilitySet::CHOWN)
    .union(CapabilitySet::SETPCAP);

/// Root, or a user the supervisor left exactly the capabilities above.
fn privileged() -> bool {
    if rustix::process::geteuid().is_root() {
        return true;
    }
    rustix::thread::capabilities(None).is_ok_and(|caps| caps.effective.contains(NEEDED))
}

/// In the child, after the UID switch: a non-root forker's capabilities survive setresuid,
/// and the ambient set would survive execve, so everything goes explicitly. The bounding set
/// first (that needs CAP_SETPCAP, which a root forker's child lost with the UID already).
fn drop_all_capabilities() -> io::Result<()> {
    let caps = rustix::thread::capabilities(None)?;
    if caps.effective.contains(CapabilitySet::SETPCAP) {
        for cap in CapabilitySet::all().iter() {
            if cap.bits().count_ones() == 1 && rustix::thread::capability_is_in_bounding_set(cap)? {
                rustix::thread::remove_capability_from_bounding_set(cap)?;
            }
        }
    }
    rustix::thread::clear_ambient_capability_set()?;
    rustix::thread::set_capabilities(
        None,
        CapabilitySets {
            effective: CapabilitySet::empty(),
            permitted: CapabilitySet::empty(),
            inheritable: CapabilitySet::empty(),
        },
    )?;
    Ok(())
}

#[derive(Parser)]
#[command(name = "drv-forker", about = "Fork sandboxed apps for drv-appd")]
struct Args {
    /// `start:count`: the UIDs apps may run as.
    #[arg(long)]
    range: String,
    /// Per-UID `XDG_RUNTIME_DIR` parent.
    #[arg(long, default_value = "/run/drv-apps")]
    runtime_base: PathBuf,
    /// Per-UID `HOME` parent.
    #[arg(long, default_value = "/var/lib/drv-apps")]
    home_base: PathBuf,
    /// The store: the only executable thing in an app's root.
    #[arg(long, default_value = "/nix/store")]
    store: PathBuf,
    /// The host's generated views of itself (`dev`, `dev-gpu`, `sys`, `sys-gpu`), written
    /// at boot. Bound into every app's root.
    #[arg(long, default_value = "/run/drv-host")]
    host_views: PathBuf,
    /// The host's resolv.conf, at `/run/host/resolv.conf` for a networked app.
    #[arg(long, default_value = "/etc/resolv.conf")]
    resolv: PathBuf,
}

/// The layout of `/run` as the system configuration makes it: what each feature of an app
/// means, in one place. Every app: the appd socket and the apps' Wayland socket, its own
/// runtime directory, the documents mount (writable: what it saves goes there).
const RUN_ALWAYS: &[&str] = &["/run/drv", "/run/drv-wayland"];
const RUN_DOCS: &str = "/run/drv-doc";
/// `bus`: the bridge's socket, which its private bus forwards to.
const RUN_BUS: &[&str] = &["/run/drv-bridge"];
/// `gpu`: the driver link into the store.
const RUN_GPU: &[&str] = &["/run/opengl-driver"];
/// `audio`: PipeWire's apps socket and the per-app PulseAudio servers.
const RUN_AUDIO: &[&str] = &["/run/drv-audio", "/run/drv-pulse"];

struct Forker {
    /// UIDs a request may name: `[start, start + count)`. Never root, never a service.
    start: u32,
    count: u32,
    runtime_base: PathBuf,
    home_base: PathBuf,
    store: PathBuf,
    host_views: PathBuf,
    resolv: PathBuf,
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
            let (request, _fds): (Request, Vec<OwnedFd>) = match seq::recv(&channel) {
                Ok(r) => r,
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(err) => return Err(err),
            };
            let response = match request {
                Request::Launch(launch) => match self.launch(&launch) {
                    Ok(pid) => Response::Forked { pid },
                    Err(err) => {
                        drv_os::say!("drv-forker: refused uid {}: {err}", launch.uid);
                        Response::Error(err)
                    }
                },
            };
            seq::send(&channel, &response, &[])?;
        }
    }

    fn launch(&self, launch: &Launch) -> Result<u32, String> {
        if launch.argv.is_empty() {
            return Err("empty argv".to_owned());
        }
        // The typed fields: the name is an identifier, the UID is ours to give, the closure
        // is store paths. After these, construction cannot fail for a security reason.
        let home = home_of(&launch.name)?;
        let uid = launch.uid;
        let privileged = privileged();
        // Unprivileged (tests): can only fork as ourselves, with no sandbox and no cgroup.
        let as_self = !privileged && uid == rustix::process::getuid().as_raw();
        if !as_self && !self.covers(uid) {
            return Err(format!("uid {uid} is outside the app range"));
        }
        if !as_self && !privileged {
            return Err("forker is unprivileged, can only fork as itself".to_owned());
        }
        // The one field that is a path: typed by syntax. Any store path is content an app
        // may be given; nothing outside the store is.
        for path in &launch.closure {
            self.store_path(path)?;
        }
        // One cgroup per app UID under our own delegated subtree, so killing an app is killing
        // a cgroup. Unprivileged (tests) has no subtree to write.
        let cgroup_procs = if privileged {
            Some(app_cgroup_procs(uid)?)
        } else {
            None
        };

        let mut command = Command::new(&launch.argv[0]);
        command.args(&launch.argv[1..]);
        command.env_clear().envs(launch.env.iter().cloned());
        command.stdin(Stdio::null());

        let gid = if as_self {
            rustix::process::getgid().as_raw()
        } else {
            uid
        };
        // The root and the Landlock ruleset are built here, in the parent, as fds; the child
        // only enters them. chdir into the 0700 home only once we are the UID (std's
        // current_dir would do it first, as us): in the child, below.
        let mut root = None;
        let mut home_dir = None;
        if !as_self {
            let runtime = self.owned_dir(&self.runtime_base, uid, gid)?;
            let state = self.owned_dir(&self.home_base, uid, gid)?;
            command.env("XDG_RUNTIME_DIR", &runtime).env("HOME", &home);
            home_dir = Some(home.clone());
            // Its /tmp outlives a launch (beside the runtime dirs, so gone with the boot): a
            // second launch of the app finds the first one's single-instance socket there.
            let tmp = self.owned_dir(&self.runtime_base.join("tmp"), uid, gid)?;
            // What its features mean under /run.
            let mut run = vec![RunEntry { path: &runtime, writable: true }, RunEntry { path: Path::new(RUN_DOCS), writable: true }];
            let features = [(true, RUN_ALWAYS), (launch.bus, RUN_BUS), (launch.gpu, RUN_GPU), (launch.audio, RUN_AUDIO)];
            for (on, paths) in features {
                if on {
                    run.extend(paths.iter().map(|p| RunEntry { path: Path::new(p), writable: false }));
                }
            }
            let prepared = AppRoot::prepare(&Spec {
                store: &self.store,
                resolv: &self.resolv,
                views: &self.host_views,
                gpu: launch.gpu,
                network: launch.network,
                run: &run,
                tmp: &tmp,
                home: &home,
                state: &state,
                uid,
                gid,
                closure: &launch.closure,
                jit: launch.jit,
            })
            .map_err(|e| format!("root: {e}"))?;
            if prepared.missing > 0 {
                drv_os::say!("drv-forker: {}: {} closure paths are not on this machine", launch.name, prepared.missing);
            }
            root = Some(prepared);
        }
        // Its own group and nothing else (set explicitly: gid 0 would see through hidepid).
        let all_gids = vec![Gid::from_raw(gid)];
        let switch_uid = privileged;

        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if let Some(root) = &root {
                    root.enter()?;
                }
                if let Some(procs) = &cgroup_procs {
                    // "0" means the writing process itself.
                    let mut procs = procs;
                    use std::io::Write as _;
                    procs.write_all(b"0")?;
                }
                if switch_uid {
                    // Locked for good: no root, no setuid fixups, no ambient raise, and
                    // (where the kernel knows them) exec restricted to files, not scripts.
                    set_securebits()?;
                    rustix::thread::set_thread_groups(&all_gids)?;
                    let g = Gid::from_raw(gid);
                    rustix::thread::set_thread_res_gid(g, g, g)?;
                    let u = Uid::from_raw(uid);
                    rustix::thread::set_thread_res_uid(u, u, u)?;
                    if rustix::process::getuid() != u || rustix::process::geteuid() != u {
                        return Err(io::Error::other("uid did not change"));
                    }
                }
                if let Some(home) = &home_dir {
                    rustix::process::chdir(home)?;
                }
                drop_all_capabilities()?;
                rustix::thread::set_no_new_privs(true)?;
                if let Some(root) = &root {
                    root.restrict()?;
                }
                Ok(())
            });
        }

        let spawned = command
            .spawn()
            .map_err(|err| format!("spawn {:?}: {err}", launch.argv[0]));
        let mut child = spawned?;
        let pid = child.id();
        let name = if launch.name.is_empty() { launch.argv[0].clone() } else { launch.name.clone() };
        self.running.lock().unwrap().insert(pid, uid);
        let running = Arc::clone(&self.running);
        // Reap it, or every launched app leaves a zombie under us.
        thread::spawn(move || {
            match child.wait() {
                Ok(status) => drv_os::say!("drv-forker: {name} (pid {pid}, uid {uid}) exited: {status}"),
                Err(err) => drv_os::say!("drv-forker: waiting for {name} (pid {pid}): {err}"),
            }
            running.lock().unwrap().remove(&pid);
        });
        Ok(pid)
    }

    /// `path` is `<store>/<one entry>`: a type check, no lookup.
    fn store_path(&self, path: &str) -> Result<(), String> {
        let ok = Path::new(path)
            .strip_prefix(&self.store)
            .ok()
            .and_then(|rest| rest.to_str())
            .is_some_and(|rest| !rest.is_empty() && !rest.contains('/') && !rest.starts_with('.'));
        if ok {
            Ok(())
        } else {
            Err(format!("{path:?}: not a store path"))
        }
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

/// `cgroup.procs` of `<our cgroup>/apps/app-<uid>`, created if needed. The supervisor made
/// `apps` ours (and keeps its `cgroup.kill`, which ends every app when the set restarts).
/// `/home/<name>`; the name is checked, not trusted.
fn home_of(name: &str) -> Result<PathBuf, String> {
    let ok = !name.is_empty()
        && !name.starts_with('.')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if !ok {
        return Err(format!("app name {name:?}: letters, digits, '-', '_', '.' only"));
    }
    Ok(Path::new("/home").join(name))
}

/// Between fork and exec, with CAP_SETPCAP still ours. Every bit locked.
fn set_securebits() -> io::Result<()> {
    const NOROOT: u32 = 1 << 0;
    const NO_SETUID_FIXUP: u32 = 1 << 2;
    const KEEP_CAPS_LOCKED: u32 = 1 << 5;
    const NO_CAP_AMBIENT_RAISE: u32 = 1 << 6;
    const EXEC_RESTRICT_FILE: u32 = 1 << 8;
    const EXEC_DENY_INTERACTIVE: u32 = 1 << 10;
    let locked = |bit: u32| bit | (bit << 1);
    let base = locked(NOROOT) | locked(NO_SETUID_FIXUP) | KEEP_CAPS_LOCKED | locked(NO_CAP_AMBIENT_RAISE);
    let exec = locked(EXEC_RESTRICT_FILE) | locked(EXEC_DENY_INTERACTIVE);
    // rustix's flag set predates the exec bits (6.14): retain them past its check.
    let set = |bits: u32| rustix::thread::set_capabilities_secure_bits(CapabilitiesSecureBits::from_bits_retain(bits));
    if set(base | exec).is_ok() {
        return Ok(());
    }
    // A kernel before 6.14 does not know the exec bits.
    set(base)?;
    Ok(())
}

fn app_cgroup_procs(uid: u32) -> Result<std::fs::File, String> {
    let dir = own_cgroup()?.join("apps").join(format!("app-{uid}"));
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
    let channel = drv_os::fds::take()
        .and_then(|mut fds| fds.socket("channel", drv_os::fds::Kind::SeqPacket))
        .map_err(|e| format!("the channel from the supervisor: {e}"))?;
    let (start, count) = parse_range(&args.range)?;
    let forker = Forker {
        start,
        count,
        runtime_base: args.runtime_base,
        home_base: args.home_base,
        store: args.store,
        host_views: args.host_views,
        resolv: args.resolv,
        running: Default::default(),
    };
    forker.serve(channel).map_err(|e| format!("channel: {e}"))
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
    use drv_policy::forker::Channel;

    use super::*;

    #[test]
    fn forks_as_ourselves_over_the_channel_and_refuses_the_rest() {
        let dir = std::env::temp_dir().join(format!("drv-forker-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let uid = rustix::process::getuid().as_raw();
        let forker = Forker {
            start: 0,
            count: 0,
            runtime_base: dir.join("run"),
            home_base: dir.join("home"),
            store: PathBuf::from("/nix/store"),
            host_views: dir.join("views"),
            resolv: dir.join("none"),
            running: Default::default(),
        };
        for view in ["dev", "sys"] {
            std::fs::create_dir_all(forker.host_views.join(view)).unwrap();
        }
        let (ours, theirs) = seq::pair().unwrap();
        let _forker = thread::spawn(move || forker.serve(theirs));
        let channel = Channel::new(ours);
        let path = std::env::var("PATH").unwrap();
        let launch = |uid, argv: Vec<&str>| Launch {
            name: "test".into(),
            uid,
            argv: argv.into_iter().map(String::from).collect(),
            env: vec![("PATH".into(), path.clone())],
            network: false,
            gpu: false,
            audio: false,
            bus: false,
            jit: false,
            closure: vec!["/nix/store/00000000000000000000000000000000-fine".into()],
        };

        let pid = channel.launch(&launch(uid, vec!["sh", "-c", "exit 0"])).unwrap();
        assert!(pid > 0);

        let err = channel.launch(&launch(uid.wrapping_add(1), vec!["sh"])).unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");

        for bad in ["/var/lib/drv-apps/100005", "/nix/store", "/nix/store/x/../../etc", "/nix/store/.hidden"] {
            let err = channel
                .launch(&Launch { closure: vec![bad.into()], ..launch(uid, vec!["sh"]) })
                .unwrap_err();
            assert!(err.to_string().contains("not a store path"), "{bad}: {err}");
        }
        let err = channel.launch(&Launch { name: "../x".into(), ..launch(uid, vec!["sh"]) }).unwrap_err();
        assert!(err.to_string().contains("app name"), "{err}");

        // The channel survives refusals: still answering.
        channel.launch(&launch(uid, vec!["sh", "-c", "exit 0"])).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
