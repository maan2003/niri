//! File-backed policy daemon: answers `Lookup { uid }` from a TOML policy file. Stand-in for
//! the identity daemon; speaks the same protocol.

use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::{env, fs};

use clap::Parser;
use niri_policy::{daemon, PolicyStore};

#[derive(Parser)]
#[command(
    name = "niri-policyd",
    about = "Serve a niri policy file over a Unix socket"
)]
struct Args {
    /// TOML policy file.
    #[arg(long)]
    policy: PathBuf,
    /// Socket path. Ignored under systemd socket activation (`LISTEN_FDS`). Defaults to
    /// `$XDG_RUNTIME_DIR/niri-policy.sock`.
    #[arg(long)]
    socket: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();

    let store = match PolicyStore::load(&args.policy) {
        Ok(store) => Arc::new(store),
        Err(err) => {
            eprintln!("niri-policyd: {}: {err}", args.policy.display());
            return ExitCode::FAILURE;
        }
    };

    let listener = if env::var("LISTEN_FDS").ok().as_deref() == Some("1") {
        // SAFETY: systemd hands us fd 3 as the listening socket and nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(3) };
        UnixListener::from(fd)
    } else {
        let path = match args.socket.or_else(default_socket_path) {
            Some(path) => path,
            None => {
                eprintln!("niri-policyd: --socket not given and XDG_RUNTIME_DIR is unset");
                return ExitCode::FAILURE;
            }
        };
        // A stale socket file from a previous run would make bind fail.
        let _ = fs::remove_file(&path);
        match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("niri-policyd: cannot listen on {}: {err}", path.display());
                return ExitCode::FAILURE;
            }
        }
    };

    if let Err(err) = daemon::serve(listener, store) {
        eprintln!("niri-policyd: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn default_socket_path() -> Option<PathBuf> {
    env::var_os("XDG_RUNTIME_DIR").map(|dir| PathBuf::from(dir).join("niri-policy.sock"))
}
