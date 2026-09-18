//! The command line onto the identity daemon: what a launcher's desktop entries run, and a way
//! to look at policy from a shell. Talks to the socket like any other client; nothing here is
//! privileged.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use drv_policy::PolicyClient;

#[derive(Parser)]
#[command(name = "drv", about = "Talk to the identity daemon")]
struct Cli {
    /// Identity socket; `DRV_IDENTITY_SOCKET` overrides the default.
    #[arg(long)]
    identity: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start an app by its manifest name.
    Launch { app: String },
    /// Print the policy for a UID (default: our own).
    Lookup { uid: Option<u32> },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let identity = cli.identity.unwrap_or_else(|| {
        std::env::var_os(drv_policy::env::IDENTITY_SOCKET)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/run/drv/identity.sock"))
    });
    let mut client = match PolicyClient::connect(identity.clone()) {
        Ok(client) => client,
        Err(err) => {
            eprintln!("drv: {}: {err}", identity.display());
            return ExitCode::FAILURE;
        }
    };
    let result = match cli.cmd {
        Cmd::Launch { app } => client
            .launch(app)
            .map(|uid| format!("launched as uid {uid}")),
        Cmd::Lookup { uid } => {
            let uid = uid.unwrap_or_else(|| rustix::process::getuid().as_raw());
            client.lookup(uid).map(|policy| format!("{policy:#?}"))
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
