//! drv-appd's process. Its fds come from the supervisor by name: `listener` (the public
//! socket, lookups only), `channel` (to drv-forker), `compositor` and `menu` (the launch
//! channels of the two launchers). Nothing here is found; all of it was put in place before
//! we ran.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use clap::Parser;
use drv_appd::{load_config, Appd};
use drv_os::fds::Kind;
use drv_policy::forker::Channel;

#[derive(Parser)]
#[command(name = "drv-appd", about = "The app daemon; runs under drv-supervisor")]
struct Args {
    /// The manifest.
    #[arg(long, default_value = "/etc/drv/appd.toml")]
    config: PathBuf,
}

fn run(args: Args) -> Result<(), String> {
    let mut fds = drv_os::fds::take().map_err(|e| format!("fds from the supervisor: {e}"))?;
    let listener = fds.listener("listener").map_err(|e| e.to_string())?;
    let channel = fds.socket("channel", Kind::SeqPacket).map_err(|e| e.to_string())?;
    let compositor = fds.socket("compositor", Kind::Stream).map_err(|e| e.to_string())?;
    let menu = fds.socket("menu", Kind::Stream).map_err(|e| e.to_string())?;
    let config = load_config(&args.config).map_err(|e| e.to_string())?;
    let base_env: Vec<(String, String)> = std::env::var("PATH")
        .map(|path| vec![("PATH".to_owned(), path)])
        .unwrap_or_default();
    let appd = Arc::new(Appd::new(config, Arc::new(Channel::new(channel)), base_env));

    appd.serve_launcher("the compositor".to_owned(), compositor);
    appd.serve_launcher("the menu".to_owned(), menu);

    // Autostart once the compositor's socket listens; the apps live as long as the set does.
    let autostart = appd.clone();
    thread::spawn(move || autostart.autostart(Duration::from_secs(60)));

    // From here on: our fds and what arrives on them, new connections on the listener,
    // threads, and reading files (`/proc/net/unix` for autostart).
    if drv_os::seccomp::enabled() {
        let mut allow = drv_os::seccomp::Allowlist::base().map_err(|e| e.to_string())?;
        allow.read_files().map_err(|e| e.to_string())?;
        allow.accept();
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
