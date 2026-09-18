//! The command line onto drv-appd: what a launcher's desktop entries run, and a way
//! to look at policy from a shell. `launch` needs the launch channel a launcher inherits
//! (`DRV_LAUNCH_FD`, from drv-appd for apps with `launcher = true`); `lookup` talks to the
//! public socket like any other client. Nothing here is privileged.

use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use drv_policy::PolicyClient;

#[derive(Parser)]
#[command(name = "drv", about = "Talk to drv-appd")]
struct Cli {
    /// drv-appd's socket; `DRV_APPD_SOCKET` overrides the default.
    #[arg(long)]
    appd: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start an app by its manifest name, over the launch channel we inherited.
    Launch { app: String },
    /// Print the policy for a UID (default: our own).
    Lookup { uid: Option<u32> },
}

/// The channel drv-appd gave the launcher we run under, as `DRV_LAUNCH_FD` says.
fn launch_channel() -> std::io::Result<PolicyClient> {
    let fd = std::env::var(drv_policy::env::LAUNCH_FD)
        .ok()
        .and_then(|s| s.parse::<i32>().ok())
        .filter(|fd| *fd >= 0)
        .ok_or_else(|| {
            std::io::Error::other(format!(
                "no launch channel ({} unset): only an app with launcher = true may launch",
                drv_policy::env::LAUNCH_FD
            ))
        })?;
    // SAFETY: drv-appd put the channel on this fd and we are its only user in this process.
    let stream = UnixStream::from(unsafe { OwnedFd::from_raw_fd(fd) });
    PolicyClient::from_stream(stream)
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let appd = cli.appd.unwrap_or_else(|| {
        std::env::var_os(drv_policy::env::APPD_SOCKET)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/run/drv/appd.sock"))
    });
    let result = match cli.cmd {
        Cmd::Launch { app } => launch_channel().and_then(|mut client| {
            client
                .launch(app)
                .map(|uid| format!("launched as uid {uid}"))
        }),
        Cmd::Lookup { uid } => {
            let uid = uid.unwrap_or_else(|| rustix::process::getuid().as_raw());
            PolicyClient::connect(appd.clone())
                .map_err(|err| std::io::Error::other(format!("{}: {err}", appd.display())))
                .and_then(|mut client| client.lookup(uid))
                .map(|policy| format!("{policy:#?}"))
        }
    };
    match result {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("drv: {err}");
            ExitCode::FAILURE
        }
    }
}
