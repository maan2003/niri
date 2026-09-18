//! drv-appd's process: fd 3 is the wire from the supervisor (our socket into drv-authd
//! arrives on it), fd 4 the public listener, fd 5 the notice socket, fd 6 the channel to
//! drv-forker. Nothing here is found; all of it was put in place before we ran.

use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use clap::Parser;
use drv_appd::{load_config, Appd};
use drv_policy::forker::{Channel, Notice};
use drv_policy::wire::{self, Attach};
use drv_policy::seq;

const LISTENER_FD: i32 = 4;
const NOTICE_FD: i32 = 5;
const CHANNEL_FD: i32 = 6;

#[derive(Parser)]
#[command(name = "drv-appd", about = "The app daemon; runs under drv-supervisor")]
struct Args {
    /// The manifest.
    #[arg(long, default_value = "/etc/drv/appd.toml")]
    config: PathBuf,
}

fn run(args: Args) -> Result<(), String> {
    let wire = wire::take().ok_or("no wire on fd 3: drv-appd runs under drv-supervisor")?;
    let config = load_config(&args.config).map_err(|e| e.to_string())?;
    // SAFETY: the supervisor put these fds in place and nothing else owns them.
    let listener = UnixListener::from(unsafe { OwnedFd::from_raw_fd(LISTENER_FD) });
    let notices = unsafe { OwnedFd::from_raw_fd(NOTICE_FD) };
    let channel = unsafe { OwnedFd::from_raw_fd(CHANNEL_FD) };
    let base_env: Vec<(String, String)> = std::env::var("PATH")
        .map(|path| vec![("PATH".to_owned(), path)])
        .unwrap_or_default();
    let appd = Arc::new(Appd::new(config, Arc::new(Channel::new(channel)), base_env));

    let attached = appd.clone();
    thread::spawn(move || loop {
        match wire::recv_attach(&wire) {
            Ok((Attach::Auth, sock)) => {
                eprintln!("drv-appd: attached to drv-authd");
                attached.set_verifiers(sock);
            }
            Ok((Attach::Compositor, sock)) => {
                eprintln!("drv-appd: the compositor's launch channel attached");
                attached.serve_launcher("the compositor".to_owned(), sock);
            }
            Ok((other, _)) => eprintln!("drv-appd: ignoring {other:?} on the wire"),
            Err(err) => {
                eprintln!("drv-appd: the wire ended: {err}");
                return;
            }
        }
    });

    // Autostart follows the compositor: on every start of one, what is not running is
    // launched once its socket listens.
    let autostart = appd.clone();
    thread::spawn(move || loop {
        match seq::recv::<Notice>(&notices) {
            Ok((Notice::CompositorStarted, _)) => autostart.autostart(Duration::from_secs(60)),
            Err(err) => {
                eprintln!("drv-appd: notices from the supervisor ended: {err}");
                return;
            }
        }
    });

    // From here on: our fds and what arrives on them, new connections on the listener,
    // socketpairs for what a launch hands its app, threads, and reading files
    // (`/proc/net/unix` for autostart).
    if drv_os::seccomp::enabled() {
        let mut allow = drv_os::seccomp::Allowlist::base().map_err(|e| e.to_string())?;
        allow.read_files().map_err(|e| e.to_string())?;
        allow.accept();
        allow.allow(&[libc::SYS_socketpair]);
        allow.apply("drv-appd").map_err(|e| e.to_string())?;
        eprintln!("drv-appd: seccomp: syscall allowlist applied");
    }
    drv_policy::daemon::serve(listener, appd).map_err(|e| format!("serving: {e}"))
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("drv-appd: {err}");
            ExitCode::FAILURE
        }
    }
}
