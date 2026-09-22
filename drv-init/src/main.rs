//! The first thing that runs as an app, inside its finished root, with no privilege
//! (DESIGN-app-namespace, "Who does what"): it fills the app's `/etc` from the store, makes
//! the declared state directories under `$HOME/.state`, links them from HOME, links the HOME
//! defaults from the store, forks the app and stays as PID 1 of its namespace: reaps, passes
//! signals on, ends with the app's status. The system configuration puts it in front of
//! every app's command; the forker knows nothing of it. A bug here is worth exactly one app.

use std::convert::Infallible;
use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "drv-init",
    about = "Link an app's /etc and state, run it, be its init"
)]
struct Args {
    /// A store path: the app's `/etc`, linked entry by entry into the empty `/etc` it was given.
    #[arg(long)]
    etc: Option<PathBuf>,
    /// A path under HOME that persists: made under `$HOME/.state`, linked from HOME. Repeatable.
    #[arg(long = "state")]
    state: Vec<PathBuf>,
    /// A store path: HOME defaults, linked into HOME entry by entry.
    #[arg(long)]
    files: Option<PathBuf>,
    /// The app.
    #[arg(last = true, required = true)]
    argv: Vec<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let name = args.argv[0].clone();
    match run(args) {
        Ok(never) => match never {},
        Err(err) => {
            drv_os::say!("drv-init: {name}: {err}");
            ExitCode::from(125)
        }
    }
}

/// Links, then the app in a child and this process as its init. Returns only on a failure
/// before the fork.
fn run(args: Args) -> Result<Infallible, String> {
    if let Some(etc) = &args.etc {
        for entry in std::fs::read_dir(etc).map_err(|e| format!("{}: {e}", etc.display()))? {
            let entry = entry.map_err(|e| format!("{}: {e}", etc.display()))?;
            link(&entry.path(), &Path::new("/etc").join(entry.file_name()))?;
        }
    }
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
    let state_root = home.join(".state");
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
        let target = state_root.join(entry);
        std::fs::create_dir_all(&target).map_err(|e| format!("state {}: {e}", target.display()))?;
        link(&target, &home.join(entry))?;
    }
    if let Some(files) = &args.files {
        link_tree(files, files, &home)?;
    }
    // SAFETY: single-threaded; the child only execs or exits.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", io::Error::last_os_error()));
    }
    if pid == 0 {
        let err = Command::new(&args.argv[0]).args(&args.argv[1..]).exec();
        drv_os::say!("drv-init: exec {}: {err}", args.argv[0]);
        std::process::exit(126);
    }
    init(pid)
}

/// PID 1 of the app's namespace: reaps whatever gets orphaned, passes the signals it is sent
/// on to the app, and ends when the app does, with its status. A PID 1 cannot be killed by a
/// signal from inside its namespace, its own included, so a signal death of the app becomes
/// exit status 128 + signal here.
fn init(app: libc::pid_t) -> ! {
    use std::sync::atomic::{AtomicI32, Ordering};
    static APP: AtomicI32 = AtomicI32::new(0);
    extern "C" fn forward(sig: libc::c_int) {
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
    let app = rustix::process::Pid::from_raw(app);
    loop {
        match rustix::process::waitpid(None, rustix::process::WaitOptions::empty()) {
            Ok(Some((pid, status))) if Some(pid) == app => {
                let code = match (status.exit_status(), status.terminating_signal()) {
                    (Some(code), _) => code as i32,
                    (_, Some(sig)) => 128 + sig,
                    _ => 1,
                };
                std::process::exit(code);
            }
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => {}
            Err(_) => std::process::exit(0),
        }
    }
}

/// `link` -> `target`, parents made; something already there is left alone.
fn link(target: &Path, link: &Path) -> Result<(), String> {
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    match std::os::unix::fs::symlink(target, link) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
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
