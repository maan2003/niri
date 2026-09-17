use std::collections::HashMap;
use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use niri_bridge::{read_msg, write_msg, Request, Response, VERSION};
use niri_policy::{AppPolicy, PolicyClient};

#[derive(Parser)]
#[command(about = "Desktop services for sandboxed apps, keyed on the peer UID")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run as the human: accept apps, key each request on the peer UID, forward to the session.
    Serve {
        #[arg(long)]
        socket: PathBuf,
        #[arg(long, env = "NIRI_IDENTITY_SOCKET")]
        identity: PathBuf,
    },
    /// Run in the app's UID on its private bus: claim the desktop names, forward to the server,
    /// then run the app.
    App {
        #[arg(long, env = niri_bridge::SOCKET_ENV)]
        socket: PathBuf,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().cmd {
        Cmd::Serve { socket, identity } => serve(socket, identity),
        Cmd::App { socket, command } => app(socket, command),
    }
}

// ---------------------------------------------------------------- the human's side

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

struct Server {
    policy: Mutex<PolicyClient>,
    session: zbus::blocking::Connection,
}

fn serve(socket: PathBuf, identity: PathBuf) -> anyhow::Result<()> {
    let session = zbus::blocking::Connection::session().context("the human's session bus")?;
    let policy = PolicyClient::connect(identity).context("identity daemon")?;
    let server = Arc::new(Server {
        policy: Mutex::new(policy),
        session,
    });

    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| socket.display().to_string())?;
    // Any UID may connect; who they are is decided per connection, below.
    std::fs::set_permissions(&socket, Permissions::from_mode(0o666))?;
    for stream in listener.incoming() {
        let stream = stream?;
        let server = server.clone();
        std::thread::spawn(move || {
            if let Err(err) = server.connection(stream) {
                eprintln!("bridge: {err:#}");
            }
        });
    }
    Ok(())
}

impl Server {
    fn connection(&self, mut stream: UnixStream) -> anyhow::Result<()> {
        let uid = rustix::net::sockopt::socket_peercred(&stream)
            .context("peer credentials")?
            .uid
            .as_raw();
        let app = self
            .policy
            .lock()
            .unwrap()
            .lookup(uid)
            .context("identity lookup")?;
        if *app == AppPolicy::unknown() {
            bail!("refusing uid {uid}: not an app");
        }
        if read_msg::<u32>(&mut stream)? != VERSION {
            bail!("uid {uid}: wrong bridge version");
        }
        loop {
            let request = match read_msg::<Request>(&mut stream) {
                Ok(request) => request,
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(err) => return Err(err.into()),
            };
            let response = match self.handle(&app, request) {
                Ok(response) => response,
                Err(err) => Response::Error(format!("{err:#}")),
            };
            write_msg(&mut stream, &response)?;
        }
    }

    fn handle(&self, app: &AppPolicy, request: Request) -> anyhow::Result<Response> {
        match request {
            Request::Notify {
                replaces,
                summary,
                body,
            } => {
                let proxy = NotificationsProxyBlocking::new(&self.session)?;
                // The name comes from the manifest, never from the app. The body is where
                // daemons render markup, so the app's body goes in as plain text.
                let id = proxy.notify(
                    &app.name,
                    replaces,
                    app.icon.as_deref().unwrap_or(""),
                    &format!("{}: {summary}", app.name),
                    &plain(&body),
                    &[],
                    HashMap::new(),
                    -1,
                )?;
                Ok(Response::Notified { id })
            }
        }
    }
}

/// Markup-safe for notification daemons that render Pango markup.
fn plain(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ---------------------------------------------------------------- the app's side

struct Shim {
    socket: PathBuf,
}

impl Shim {
    fn call(&self, request: &Request) -> zbus::fdo::Result<Response> {
        let failed = |err: std::io::Error| zbus::fdo::Error::Failed(format!("bridge: {err}"));
        let mut conn = UnixStream::connect(&self.socket).map_err(failed)?;
        write_msg(&mut conn, &VERSION).map_err(failed)?;
        write_msg(&mut conn, request).map_err(failed)?;
        read_msg(&mut conn).map_err(failed)
    }
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Shim {
    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".to_owned()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "niri-bridge".to_owned(),
            "niri".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
            "1.2".to_owned(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        _app_name: &str,
        replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        _actions: Vec<String>,
        _hints: HashMap<String, zbus::zvariant::OwnedValue>,
        _expire_timeout: i32,
    ) -> zbus::fdo::Result<u32> {
        match self.call(&Request::Notify {
            replaces: replaces_id,
            summary: summary.to_owned(),
            body: body.to_owned(),
        })? {
            Response::Notified { id } => Ok(id),
            Response::Error(err) => Err(zbus::fdo::Error::Failed(err)),
        }
    }

    fn close_notification(&self, _id: u32) {}
}

fn app(socket: PathBuf, command: Vec<String>) -> anyhow::Result<()> {
    let _bus = zbus::blocking::connection::Builder::session()
        .context("the app's private bus")?
        .name("org.freedesktop.Notifications")?
        .serve_at("/org/freedesktop/Notifications", Shim { socket })?
        .build()
        .context("claiming org.freedesktop.Notifications")?;
    let status = Command::new(&command[0])
        .args(&command[1..])
        .status()
        .with_context(|| command[0].clone())?;
    std::process::exit(status.code().unwrap_or(1));
}
