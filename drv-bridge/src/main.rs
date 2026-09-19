//! The bridge's two processes: `serve`, the trusted side under the supervisor; `app`, the
//! shim on an app's private bus. See the crate doc and [`drv_bridge::wire`].

use std::path::PathBuf;
use std::sync::LazyLock;

use clap::{Parser, Subcommand};

mod access;
mod server;
mod shim;

/// `DRV_BRIDGE_TRACE=1`: log every call apps make.
pub static TRACE: LazyLock<bool> = LazyLock::new(|| std::env::var_os("DRV_BRIDGE_TRACE").is_some());

#[derive(Parser)]
#[command(about = "Desktop services for sandboxed apps, keyed on the peer UID")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run as its own user under the supervisor: fd `listener` is the apps' socket, fd
    /// `portal` the line to drv-portal, fd `appd` a launch channel that only opens URIs.
    /// Each connection is keyed on the peer UID.
    Serve {
        #[arg(long, env = "DRV_APPD_SOCKET")]
        appd: PathBuf,
    },
    /// Run in the app's UID on its private bus: claim the desktop names, answer the D-Bus
    /// there and speak `wire` to the server, then run the app.
    App {
        #[arg(long, env = drv_bridge::SOCKET_ENV)]
        socket: PathBuf,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Serve { appd } => server::serve(appd),
        Cmd::App { socket, command } => shim::run(socket, command),
    }
}
