use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::{env, fs};

use clap::Parser;
use niri_forker::{Allowed, Server};

#[derive(Parser)]
#[command(
    name = "niri-forker",
    about = "Start apps under their own UIDs on request"
)]
struct Args {
    /// `peer:start:count[:group,group]`: peer UID allowed to ask, the UID range it may ask
    /// for, and the supplementary groups it may hand out. Repeatable. A peer may always fork
    /// as itself.
    #[arg(long = "allow", required = true)]
    allowed: Vec<String>,
    /// Socket path (world-connectable; the `--allow` list is the gate). Ignored under systemd
    /// socket activation (`LISTEN_FDS`).
    #[arg(long, default_value = "/run/niri/forker.sock")]
    socket: PathBuf,
    /// Per-UID `XDG_RUNTIME_DIR` parent.
    #[arg(long, default_value = "/run/niri-apps")]
    runtime_base: PathBuf,
    /// Per-UID `HOME` parent.
    #[arg(long, default_value = "/var/lib/niri-apps")]
    home_base: PathBuf,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let mut allowed = Vec::new();
    for s in &args.allowed {
        match Allowed::parse(s) {
            Ok(a) => allowed.push(a),
            Err(err) => {
                eprintln!("niri-forker: --allow {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    let listener = if env::var("LISTEN_FDS").ok().as_deref() == Some("1") {
        // SAFETY: systemd hands us fd 3 as the listening socket and nothing else owns it.
        UnixListener::from(unsafe { OwnedFd::from_raw_fd(3) })
    } else {
        if let Some(parent) = args.socket.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::remove_file(&args.socket);
        let listener = match UnixListener::bind(&args.socket) {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!(
                    "niri-forker: cannot listen on {}: {err}",
                    args.socket.display()
                );
                return ExitCode::FAILURE;
            }
        };
        if let Err(err) = fs::set_permissions(&args.socket, fs::Permissions::from_mode(0o666)) {
            eprintln!("niri-forker: chmod {}: {err}", args.socket.display());
            return ExitCode::FAILURE;
        }
        listener
    };

    let server = Server {
        allowed,
        runtime_base: args.runtime_base,
        home_base: args.home_base,
    };
    if let Err(err) = server.serve(listener) {
        eprintln!("niri-forker: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
