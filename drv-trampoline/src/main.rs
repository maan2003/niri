//! The first thing that runs as an app, inside its finished root, with no privilege
//! (DESIGN-app-namespace, "State"): it makes the app's declared state directories under
//! `$HOME/.state`, links them from HOME, links the HOME defaults from the store, and execs the
//! command. The system configuration puts it in front of an app's command; the forker knows
//! nothing of it. A bug here is worth exactly one app.

use std::io;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use clap::Parser;

#[derive(Parser)]
#[command(name = "drv-trampoline", about = "Link an app's state into HOME, then exec it")]
struct Args {
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
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is not set")?);
    let state_root = home.join(".state");
    for entry in &args.state {
        if entry.is_absolute() || entry.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            return Err(format!("state {}: not a plain relative path", entry.display()));
        }
        let target = state_root.join(entry);
        std::fs::create_dir_all(&target).map_err(|e| format!("state {}: {e}", target.display()))?;
        link(&target, &home.join(entry))?;
    }
    if let Some(files) = &args.files {
        link_tree(files, files, &home)?;
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
