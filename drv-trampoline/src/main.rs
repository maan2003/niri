//! The last thing that runs before an app, as the app (DESIGN-app-namespace, "The
//! trampoline"): no capabilities, no privilege, inside the finished root. It makes the
//! declared state directories and links them into HOME, links the HOME defaults from the
//! store, puts itself under Landlock (the app's closure and what the forker listed, nothing
//! else), refuses writable-then-executable memory, and execs the command. A bug here is
//! worth exactly one app.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::Parser;
use drv_os::landlock::{self, Ruleset};

#[derive(Parser)]
#[command(name = "drv-trampoline", about = "Prepare and exec an app, as the app")]
struct Args {
    /// HOME: a fresh tmpfs of ours, with what persists at `.state`.
    #[arg(long)]
    home: PathBuf,
    /// A path under HOME that persists: made under `.state`, linked from HOME. Repeatable.
    #[arg(long = "state")]
    state: Vec<PathBuf>,
    /// The store paths the app may open: a file listing them, one per line.
    #[arg(long)]
    closure: Option<PathBuf>,
    /// A store path: HOME defaults, linked into HOME entry by entry.
    #[arg(long)]
    files: Option<PathBuf>,
    /// Readable. Repeatable.
    #[arg(long = "read")]
    read: Vec<PathBuf>,
    /// Fully writable. Repeatable.
    #[arg(long = "rw")]
    rw: Vec<PathBuf>,
    /// Device nodes: read, write, ioctl. Repeatable.
    #[arg(long = "dev")]
    dev: Vec<PathBuf>,
    /// The app makes code at runtime: no MDWE.
    #[arg(long)]
    jit: bool,
    /// The app.
    #[arg(last = true, required = true)]
    argv: Vec<String>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let name = args.argv[0].clone();
    match run(args) {
        Ok(err) => {
            drv_os::say!("drv-trampoline: exec {name}: {err}");
            ExitCode::from(126)
        }
        Err(err) => {
            drv_os::say!("drv-trampoline: {name}: {err}");
            ExitCode::from(125)
        }
    }
}

/// Returns only if the exec failed.
fn run(args: Args) -> Result<io::Error, String> {
    let state_root = args.home.join(".state");
    for entry in &args.state {
        let target = state_root.join(entry);
        std::fs::create_dir_all(&target).map_err(|e| format!("state {}: {e}", target.display()))?;
        link(&target, &args.home.join(entry))?;
    }
    if let Some(files) = &args.files {
        link_tree(files, files, &args.home)?;
    }

    let rules = Ruleset::new().map_err(|e| format!("landlock: {e}"))?;
    let all = rules.all();
    let allow = |path: &Path, access: u64| rules.allow(path, access).map_err(|e| format!("landlock: {e}"));
    if let Some(closure) = &args.closure {
        let list = std::fs::read_to_string(closure).map_err(|e| format!("closure {}: {e}", closure.display()))?;
        let mut missing = 0;
        for line in list.lines().filter(|l| !l.is_empty()) {
            if !allow(Path::new(line), landlock::READ | landlock::EXECUTE)? {
                missing += 1;
            }
        }
        if missing > 0 {
            drv_os::say!("drv-trampoline: {missing} closure paths are not on this machine");
        }
    }
    for path in &args.read {
        allow(path, landlock::READ)?;
    }
    // /proc: the app writes its own oom_score_adj and the like.
    allow(Path::new("/proc"), landlock::READ | landlock::WRITE_FILE)?;
    for path in &args.dev {
        allow(path, landlock::READ | landlock::WRITE_FILE | landlock::IOCTL_DEV)?;
    }
    for path in args.rw.iter().chain(std::iter::once(&args.home)) {
        allow(path, all)?;
    }
    rules.restrict_self().map_err(|e| format!("landlock: {e}"))?;

    if !args.jit {
        const PR_SET_MDWE: libc::c_int = 65;
        const PR_MDWE_REFUSE_EXEC_GAIN: libc::c_ulong = 1;
        // SAFETY: plain prctl.
        if unsafe { libc::prctl(PR_SET_MDWE, PR_MDWE_REFUSE_EXEC_GAIN, 0, 0, 0) } != 0 {
            return Err(format!("mdwe: {}", io::Error::last_os_error()));
        }
    }

    Ok(Command::new(&args.argv[0]).args(&args.argv[1..]).exec())
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
