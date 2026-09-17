use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::{env, fs};

use clap::Parser;
use niri_identity::{load_config, Identity};
use niri_policy::daemon;

#[derive(Parser)]
#[command(
    name = "niri-identityd",
    about = "Identity daemon: app UIDs, policy, launching"
)]
struct Args {
    #[arg(long, default_value = "/etc/niri/identity.toml")]
    config: PathBuf,
    /// Socket the compositor connects to. Ignored under systemd socket activation
    /// (`LISTEN_FDS`). Defaults to `$XDG_RUNTIME_DIR/niri-identity.sock`.
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Forker socket. Overrides the config file.
    #[arg(long)]
    forker: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args = Args::parse();

    let config = match load_config(&args.config) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("niri-identityd: {err}");
            return ExitCode::FAILURE;
        }
    };
    let forker = args
        .forker
        .or_else(|| config.forker.clone())
        .unwrap_or_else(|| PathBuf::from("/run/niri/forker.sock"));
    // What every app starts with; the compositor adds its display variables per launch.
    let base_env: Vec<(String, String)> = ["PATH", "LANG", "TZ", "TERM"]
        .iter()
        .filter_map(|k| env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();

    let identity = Arc::new(Identity::new(config, forker, base_env));

    let listener = if env::var("LISTEN_FDS").ok().as_deref() == Some("1") {
        // SAFETY: systemd hands us fd 3 as the listening socket and nothing else owns it.
        UnixListener::from(unsafe { OwnedFd::from_raw_fd(3) })
    } else {
        let Some(path) = args.socket.or_else(socket_path) else {
            eprintln!("niri-identityd: --socket not given and XDG_RUNTIME_DIR is unset");
            return ExitCode::FAILURE;
        };
        let _ = fs::remove_file(&path);
        match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(err) => {
                eprintln!("niri-identityd: cannot listen on {}: {err}", path.display());
                return ExitCode::FAILURE;
            }
        }
    };

    if let Err(err) = daemon::serve(listener, identity) {
        eprintln!("niri-identityd: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn socket_path() -> Option<PathBuf> {
    env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("niri-identity.sock"))
}
