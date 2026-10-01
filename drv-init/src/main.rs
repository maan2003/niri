//! The first thing that runs as an app, inside the root the forker built for it, with no
//! privilege (DESIGN-app-namespace, "Who does what"). The forker left a root tmpfs the app
//! owns, with the store, the device and sysfs views, proc, the doors and `/state` mounted in
//! it; this makes the rest: `/tmp`, `/etc` (its own account, the resolver the forker handed
//! over, the rest linked from the store), the runtime directory, HOME (of the run, with the
//! declared state directories linked from `/state`, or `/state` itself), the HOME defaults
//! from the store, the links at fixed places (`/bin/sh`), the person's folders. Then it puts
//! itself under the app's Landlock rules, the syscall denylist and MDWE, forks the app and
//! stays as PID 1 of its namespace: reaps, passes signals on, ends with the app's status.
//!
//! Everything it does is said by the app's run file (`services.drv.mkApp` writes it): an
//! executable in the store whose first line names this program as its interpreter and whose
//! rest is JSON, `Run` below. The manifest launches that file and nothing else; drv-appd and
//! the forker know neither this program nor what the file says. Arguments after the file (the
//! OpenURI portal's URI) go to the app. A bug here is worth exactly one app.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use drv_os::landlock::{self, Ruleset};
use serde::Deserialize;

/// HOME of the run: gone with the app.
const HOME_RUN: &str = "/home/app";
/// What persists, mounted by the forker: `/var/lib/drv-apps/<uid>` on the host.
const STATE: &str = "/state";
/// Where the forker put the person's folders the app was given.
const FILES: &str = "/files";
/// `XDG_RUNTIME_DIR`: of the run, like `/tmp`.
const RUNTIME: &str = "/run/app";
/// The doors, bound read-only by the forker, and the documents mount inside, which the app
/// writes (the files it was given).
const DOCS: &str = "/run/drv/doc";
const STORE: &str = "/nix/store";
const DAEMON_SOCKET: &str = "/nix/var/nix/daemon-socket";
/// The manifest's name for the app, in the environment drv-appd gives it: its account.
const APP_ENV: &str = "DRV_APP";

/// The run file's JSON. Every path is in the store.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct Run {
    /// The app's `/etc`, linked entry by entry (the account, the resolver and the name
    /// service switch are made here).
    #[serde(default)]
    etc: Option<PathBuf>,
    /// The account's shell (`/etc/passwd`).
    shell: String,
    /// Paths under HOME that persist: made under `/state`, linked from HOME. Nothing with
    /// `home = "persist"`, where HOME is `/state`.
    #[serde(default)]
    state: Vec<PathBuf>,
    /// HOME defaults, linked into HOME entry by entry.
    #[serde(default)]
    files: Option<PathBuf>,
    /// A daemon: started again when it exits, until this init is told to stop.
    #[serde(default)]
    restart: bool,
    /// `run`: HOME is /home/app, of the run. `persist`: HOME is /state, the app's whole home
    /// persists.
    #[serde(default)]
    home: Home,
    /// A file listing the store paths the app may open (closureInfo's store-paths).
    #[serde(default)]
    closure: Option<PathBuf>,
    /// Links at fixed places (absolute paths in the root) to targets in the store.
    #[serde(default)]
    links: BTreeMap<String, String>,
    /// The app makes code at runtime (a JIT): no MDWE for it.
    #[serde(default)]
    jit: bool,
    /// The app may make user namespaces (a browser's own sandbox).
    #[serde(default)]
    userns: bool,
    /// The app runs nix: the whole store readable, the daemon's socket where nix looks.
    #[serde(default)]
    nix: bool,
    /// The app's own environment, over what drv-appd gave.
    #[serde(default)]
    env: BTreeMap<String, String>,
    /// The app.
    command: Vec<String>,
}

#[derive(Deserialize, Default, PartialEq)]
#[serde(rename_all = "kebab-case")]
enum Home {
    #[default]
    Run,
    Persist,
}

/// The run file: its first line names us, the rest is `Run`.
fn read_run(path: &Path) -> Result<Run, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let json = match text.strip_prefix("#!") {
        Some(rest) => rest.split_once('\n').map_or("", |(_, json)| json),
        None => &text,
    };
    serde_json::from_str(json).map_err(|e| format!("{}: {e}", path.display()))
}

fn main() -> ExitCode {
    // Run as the run file's interpreter, we are named after the file: name ourselves.
    let _ = rustix::thread::set_name(c"drv-init");
    let argv: Vec<String> = std::env::args().collect();
    let name = std::env::var(APP_ENV).unwrap_or_else(|_| "app".to_owned());
    let Some(file) = argv.get(1) else {
        drv_os::say!("drv-init: {name}: no run file");
        return ExitCode::from(125);
    };
    let run_ = match read_run(Path::new(file)) {
        Ok(mut run_) => {
            run_.command.extend(argv[2..].iter().cloned());
            run_
        }
        Err(err) => {
            drv_os::say!("drv-init: {name}: {err}");
            return ExitCode::from(125);
        }
    };
    match run(&name, run_) {
        Ok(never) => match never {},
        Err(err) => {
            drv_os::say!("drv-init: {name}: {err}");
            ExitCode::from(125)
        }
    }
}

/// The root, the rules, then the app in a child and this process as its init. Returns only
/// on a failure before the fork.
fn run(name: &str, args: Run) -> Result<Infallible, String> {
    if args.command.is_empty() {
        return Err("the run file names no command".to_owned());
    }
    let persist = args.home == Home::Persist;
    let mut fds = drv_os::fds::take().map_err(|e| format!("fds from the forker: {e}"))?;
    // The forker hands the host's resolver to a networked app and to no other.
    let resolv = fds.file("resolv").ok();
    // 1. The directories of the run, in the root tmpfs the forker left us.
    make_dir("/tmp", 0o1777)?;
    make_dir("/etc", 0o755)?;
    make_dir(RUNTIME, 0o700)?;
    let home = if persist {
        PathBuf::from(STATE)
    } else {
        make_dir(HOME_RUN, 0o700)?;
        PathBuf::from(HOME_RUN)
    };
    // 2. /etc: the account (the manifest's name, this uid), the name service switch (DNS
    // for a networked app), the rest from the store, the resolver from the host.
    let (uid, gid) = (
        rustix::process::getuid().as_raw(),
        rustix::process::getgid().as_raw(),
    );
    write_etc(
        "passwd",
        &format!(
            "app-{name}:x:{uid}:{gid}:{name}:{}:{}\n",
            home.display(),
            args.shell
        ),
    )?;
    write_etc("group", &format!("app-{name}:x:{gid}:\n"))?;
    write_etc(
        "nsswitch.conf",
        &format!(
            "passwd: files\ngroup: files\nhosts: files{}\n",
            if resolv.is_some() { " dns" } else { "" }
        ),
    )?;
    write_etc("hosts", "127.0.0.1 localhost\n::1 localhost\n")?;
    // Of this app, not the host's: D-Bus and the toolkits want one.
    write_etc("machine-id", &format!("{uid:032x}\n"))?;
    if let Some(etc) = &args.etc {
        for entry in std::fs::read_dir(etc).map_err(|e| format!("{}: {e}", etc.display()))? {
            let entry = entry.map_err(|e| format!("{}: {e}", etc.display()))?;
            link(&entry.path(), &Path::new("/etc").join(entry.file_name()))?;
        }
    }
    if let Some(fd) = resolv {
        copy_to(fd, Path::new("/etc/resolv.conf"))?;
    }
    // 3. The links at fixed places: into the store only.
    for (at, target) in &args.links {
        let (at, target) = (Path::new(at), Path::new(target));
        if !at.is_absolute() || !target.starts_with(STORE) {
            return Err(format!(
                "link {}: not absolute or not into the store",
                at.display()
            ));
        }
        link(target, at)?;
    }
    // 4. HOME: what persists, then the defaults.
    if !persist {
        for entry in &args.state {
            if entry.is_absolute()
                || entry
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(format!(
                    "state {}: not a plain relative path",
                    entry.display()
                ));
            }
            let target = Path::new(STATE).join(entry);
            std::fs::create_dir_all(&target)
                .map_err(|e| format!("state {}: {e}", target.display()))?;
            link(&target, &home.join(entry))?;
        }
    }
    if let Some(files) = &args.files {
        link_tree(files, files, &home)?;
    }
    // The person's folders the forker mounted: each linked from HOME.
    let folders = match std::fs::read_dir(FILES) {
        Ok(entries) => entries
            .map(|e| e.map(|e| e.file_name()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("{FILES}: {e}"))?,
        Err(_) => Vec::new(),
    };
    for folder in &folders {
        link(&Path::new(FILES).join(folder), &home.join(folder))?;
    }
    // 5. The rules, on this process and so on the app: what it may open, which syscalls it
    // may not make, no writable and executable memory. no_new_privs is the forker's doing.
    restrict(&args, persist, !folders.is_empty())?;
    let spawn = move || -> Result<libc::pid_t, String> {
        // SAFETY: single-threaded; the child only execs or exits.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(format!("fork: {}", io::Error::last_os_error()));
        }
        if pid == 0 {
            let err = Command::new(&args.command[0])
                .args(&args.command[1..])
                .envs(&args.env)
                .env("HOME", &home)
                .env("XDG_RUNTIME_DIR", RUNTIME)
                .current_dir(&home)
                .exec();
            drv_os::say!("drv-init: exec {}: {err}", args.command[0]);
            std::process::exit(126);
        }
        Ok(pid)
    };
    let pid = spawn()?;
    init(pid, if args.restart { Some(spawn) } else { None })
}

/// The app's Landlock domain (read and execute on its closure, or on the whole store with
/// `nix`; the views and the doors read-only, the documents and its own directories
/// writable; nothing else exists), the syscall denylist, MDWE.
fn restrict(args: &Run, persist: bool, folders: bool) -> Result<(), String> {
    let rules = Ruleset::new().map_err(|e| format!("landlock: {e}"))?;
    let all = rules.all();
    let allow = |path: &str, access: u64| {
        rules
            .allow(Path::new(path), access)
            .map_err(|e| format!("landlock {path}: {e}"))
    };
    if args.nix {
        allow(STORE, landlock::READ | landlock::EXECUTE)?;
        allow(DAEMON_SOCKET, landlock::READ)?;
    } else if let Some(list) = &args.closure {
        let text = std::fs::read_to_string(list).map_err(|e| format!("{}: {e}", list.display()))?;
        let mut missing = 0;
        for path in text.lines().filter(|l| !l.is_empty()) {
            if !allow(path, landlock::READ | landlock::EXECUTE)? {
                missing += 1;
            }
        }
        if missing > 0 {
            drv_os::say!("drv-init: {missing} closure paths are not on this machine");
        }
    }
    allow(
        "/dev",
        landlock::READ | landlock::WRITE_FILE | landlock::IOCTL_DEV,
    )?;
    allow("/dev/shm", all & !landlock::EXECUTE)?;
    allow("/sys", landlock::READ)?;
    allow("/proc", landlock::READ | landlock::WRITE_FILE)?;
    // The doors, read-only; the documents mount inside, the app's to write; the rest of
    // `/run` (the runtime directory's parent) listable.
    allow("/run", landlock::READ)?;
    allow(DOCS, all)?;
    for path in ["/tmp", "/etc", RUNTIME, STATE] {
        allow(path, all)?;
    }
    if folders {
        allow(FILES, all)?;
    }
    if !persist {
        allow("/home", all)?;
    }
    rules
        .restrict_self()
        .map_err(|e| format!("landlock: {e}"))?;
    drv_os::seccomp::refuse_app_doors(args.userns).map_err(|e| e.to_string())?;
    if !args.jit {
        drv_os::creds::refuse_exec_gain()?;
    }
    Ok(())
}

/// PID 1 of the app's namespace: reaps whatever gets orphaned, passes the signals it is sent
/// on to the app, and ends when the app does, with its status. A PID 1 cannot be killed by a
/// signal from inside its namespace, its own included, so a signal death of the app becomes
/// exit status 128 + signal here. With `restart` (a daemon), the app is started again two
/// seconds after it ends, unless a signal to stop came in first.
fn init(app: libc::pid_t, restart: Option<impl Fn() -> Result<libc::pid_t, String>>) -> ! {
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    static APP: AtomicI32 = AtomicI32::new(0);
    static STOP: AtomicBool = AtomicBool::new(false);
    extern "C" fn forward(sig: libc::c_int) {
        if matches!(
            sig,
            libc::SIGTERM | libc::SIGINT | libc::SIGHUP | libc::SIGQUIT
        ) {
            STOP.store(true, Ordering::Relaxed);
        }
        let pid = APP.load(Ordering::Relaxed);
        if pid > 0 {
            // SAFETY: async-signal-safe.
            unsafe { libc::kill(pid, sig) };
        }
    }
    APP.store(app, Ordering::Relaxed);
    for sig in [
        libc::SIGTERM,
        libc::SIGINT,
        libc::SIGHUP,
        libc::SIGQUIT,
        libc::SIGUSR1,
        libc::SIGUSR2,
    ] {
        // SAFETY: a zeroed sigaction with a handler is a valid one; forward is signal-safe.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = forward as *const () as usize;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigaction(sig, &sa, std::ptr::null_mut());
        }
    }
    let mut app = rustix::process::Pid::from_raw(app);
    loop {
        match rustix::process::waitpid(None, rustix::process::WaitOptions::empty()) {
            Ok(Some((pid, status))) if Some(pid) == app => {
                let code = match (status.exit_status(), status.terminating_signal()) {
                    (Some(code), _) => code as i32,
                    (_, Some(sig)) => 128 + sig,
                    _ => 1,
                };
                let Some(spawn) = &restart else {
                    std::process::exit(code)
                };
                if STOP.load(Ordering::Relaxed) {
                    std::process::exit(code);
                }
                drv_os::say!("drv-init: the app exited ({code}); starting it again in 2s");
                std::thread::sleep(std::time::Duration::from_secs(2));
                if STOP.load(Ordering::Relaxed) {
                    std::process::exit(code);
                }
                match spawn() {
                    Ok(pid) => {
                        APP.store(pid, Ordering::Relaxed);
                        app = rustix::process::Pid::from_raw(pid);
                    }
                    Err(err) => {
                        drv_os::say!("drv-init: {err}");
                        std::process::exit(code);
                    }
                }
            }
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => std::process::exit(0),
        }
    }
}

/// A directory with exactly `mode` (the umask does not apply), parents made; one already
/// there (a mountpoint of the forker's, say) is left alone.
fn make_dir(path: &str, mode: u32) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("mkdir {path}: {e}"))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("chmod {path}: {e}"))
}

/// An entry of /etc written here, mode 0644.
fn write_etc(name: &str, text: &str) -> Result<(), String> {
    std::fs::write(Path::new("/etc").join(name), text).map_err(|e| format!("/etc/{name}: {e}"))
}

/// `fd`'s contents as the file at `to`, mode 0644.
fn copy_to(fd: OwnedFd, to: &Path) -> Result<(), String> {
    let mut from = std::fs::File::from(fd);
    let mut file = std::fs::File::create(to).map_err(|e| format!("{}: {e}", to.display()))?;
    io::copy(&mut from, &mut file).map_err(|e| format!("{}: {e}", to.display()))?;
    Ok(())
}

/// `link` -> `target`, parents made. Something already there is left alone, unless it is a
/// link of ours from an earlier run (into the store, to something else): a persisted home
/// keeps them, the store paths behind them change with the system.
fn link(target: &Path, link: &Path) -> Result<(), String> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    match std::os::unix::fs::symlink(target, link) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => match std::fs::read_link(link) {
            Ok(old) if old != target && old.starts_with(STORE) => {
                std::fs::remove_file(link).map_err(|e| format!("{}: {e}", link.display()))?;
                std::os::unix::fs::symlink(target, link)
                    .map_err(|e| format!("link {}: {e}", link.display()))
            }
            _ => Ok(()),
        },
        Err(e) => Err(format!("link {}: {e}", link.display())),
    }
}

/// Every file of the defaults tree, linked at the same place under HOME.
fn link_tree(root: &Path, dir: &Path, home: &Path) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        let rel = path.strip_prefix(root).map_err(|e| e.to_string())?;
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            link_tree(root, &path, home)?;
        } else {
            link(&path, &home.join(rel))?;
        }
    }
    Ok(())
}
