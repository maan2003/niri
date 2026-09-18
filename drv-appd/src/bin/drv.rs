//! The command line onto drv-appd's public socket: a way to look at policy from a shell.
//! Launching is not here: launch authority is an fd the supervisor hands a launcher, and a
//! shell has none.

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
    /// Print the policy for a UID (default: our own).
    Lookup { uid: Option<u32> },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let appd = cli.appd.unwrap_or_else(|| {
        std::env::var_os(drv_policy::env::APPD_SOCKET)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/run/drv/appd.sock"))
    });
    let result = match cli.cmd {
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
