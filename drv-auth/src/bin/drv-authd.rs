//! `drv-authd`: verifies the lock PIN and pushes the unlock to the compositor.

use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use drv_auth::{Auth, Event, Outcome, Request, Response, Role, Store, VERSION};
use rustix::net::{SocketFlags, accept_with};
use zeroize::Zeroize;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon.
    Serve {
        #[arg(long, default_value = drv_auth::DEFAULT_SOCKET)]
        socket: PathBuf,
        #[arg(long, default_value = "/var/lib/drv-auth")]
        state_dir: PathBuf,
        /// The lock app's user: the only one that may verify.
        #[arg(long)]
        verifier_user: String,
        /// The compositor's user: the only one that receives unlocks.
        #[arg(long)]
        compositor_user: String,
        /// Seconds of inactivity before the compositor locks again.
        #[arg(long, default_value_t = 300)]
        idle_timeout: u64,
    },
    /// Enrol a PIN read from stdin (until newline or EOF).
    SetPin {
        #[arg(long, default_value = "/var/lib/drv-auth")]
        state_dir: PathBuf,
    },
}

struct Shared {
    auth: Mutex<Auth>,
    compositor: Mutex<Option<OwnedFd>>,
    verifier_uid: u32,
    compositor_uid: u32,
    idle_timeout: Duration,
}

fn user_id(name: &str) -> io::Result<u32> {
    let c = std::ffi::CString::new(name).map_err(io::Error::other)?;
    // SAFETY: getpwnam returns a pointer to static storage or null; only read once here.
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    if pw.is_null() {
        return Err(io::Error::other(format!("no such user: {name}")));
    }
    Ok(unsafe { (*pw).pw_uid })
}

fn serve_conn(shared: &Shared, sock: OwnedFd) -> io::Result<()> {
    let uid = drv_auth::peer_uid(&sock)?;
    let role = match drv_auth::recv_msg::<Request>(&sock)? {
        Request::Hello { version, role } if version == VERSION => role,
        Request::Hello { version, .. } => {
            drv_auth::send_msg(
                &sock,
                &Response::Error(format!("version {version} unsupported")),
            )?;
            return Ok(());
        }
        _ => {
            drv_auth::send_msg(&sock, &Response::Error("hello first".into()))?;
            return Ok(());
        }
    };
    let allowed = match role {
        Role::Verifier => uid == shared.verifier_uid,
        Role::Compositor => uid == shared.compositor_uid,
    };
    if !allowed {
        eprintln!("refusing uid {uid} as {role:?}");
        drv_auth::send_msg(
            &sock,
            &Response::Error(format!("uid {uid} may not be {role:?}")),
        )?;
        return Ok(());
    }
    drv_auth::send_msg(&sock, &Response::Hello { version: VERSION })?;

    if role == Role::Compositor {
        // The compositor only listens; keep the newest connection, drop the old.
        eprintln!("compositor connected");
        *shared.compositor.lock().unwrap() = Some(sock);
        return Ok(());
    }

    loop {
        let mut secret = match drv_auth::recv_msg::<Request>(&sock) {
            Ok(Request::Verify { secret }) => secret,
            Ok(_) => {
                drv_auth::send_msg(&sock, &Response::Error("unexpected request".into()))?;
                continue;
            }
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        };
        let outcome = shared.auth.lock().unwrap().verify(&secret);
        secret.zeroize();
        let reply = match outcome {
            Ok(Outcome::Granted) => {
                eprintln!("PIN accepted; unlocking");
                let event = Event::Unlock {
                    idle_timeout_ms: shared.idle_timeout.as_millis() as u64,
                };
                let mut comp = shared.compositor.lock().unwrap();
                match comp.as_ref().map(|c| drv_auth::send_msg(c, &event)) {
                    Some(Ok(())) => Response::Granted,
                    Some(Err(err)) => {
                        eprintln!("compositor connection lost: {err}");
                        *comp = None;
                        Response::Error("compositor not connected".into())
                    }
                    None => Response::Error("compositor not connected".into()),
                }
            }
            Ok(Outcome::Denied { retry_after }) => {
                eprintln!("PIN rejected; retry after {retry_after:?}");
                Response::Denied {
                    retry_after_ms: retry_after.as_millis() as u64,
                }
            }
            Err(err) => {
                eprintln!("verify failed: {err}");
                Response::Error(err.to_string())
            }
        };
        drv_auth::send_msg(&sock, &reply)?;
    }
}

fn serve(
    socket: PathBuf,
    state_dir: PathBuf,
    verifier_user: String,
    compositor_user: String,
    idle_timeout: u64,
) -> io::Result<()> {
    let verifier_uid = user_id(&verifier_user)?;
    let compositor_uid = user_id(&compositor_user)?;
    if verifier_uid == compositor_uid || verifier_uid == 0 || compositor_uid == 0 {
        return Err(io::Error::other(
            "verifier and compositor must be distinct non-root users",
        ));
    }
    let store = Store::new(state_dir);
    if !store.has_pin() {
        eprintln!("no PIN enrolled: run `drv-authd set-pin`; every verify is refused until then");
    }
    let shared = Arc::new(Shared {
        auth: Mutex::new(Auth::new(store)),
        compositor: Mutex::new(None),
        verifier_uid,
        compositor_uid,
        idle_timeout: Duration::from_secs(idle_timeout),
    });
    let listener = drv_auth::listen(&socket)?;
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666))?;
    eprintln!("listening on {}", socket.display());
    loop {
        let conn = match accept_with(&listener, SocketFlags::CLOEXEC) {
            Ok(conn) => conn,
            Err(err) => {
                eprintln!("accept: {err}");
                thread::sleep(Duration::from_millis(100));
                continue;
            }
        };
        let shared = shared.clone();
        thread::spawn(move || {
            if let Err(err) = serve_conn(&shared, conn) {
                eprintln!("connection: {err}");
            }
        });
    }
}

fn main() {
    let args = Args::parse();
    let res = match args.cmd {
        Cmd::Serve {
            socket,
            state_dir,
            verifier_user,
            compositor_user,
            idle_timeout,
        } => serve(
            socket,
            state_dir,
            verifier_user,
            compositor_user,
            idle_timeout,
        ),
        Cmd::SetPin { state_dir } => {
            let mut pin = Vec::new();
            io::stdin().read_to_end(&mut pin).and_then(|_| {
                while pin.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                    pin.pop();
                }
                if pin.is_empty() {
                    return Err(io::Error::other("empty PIN"));
                }
                Store::new(state_dir).set_pin(&pin)
            })
        }
    };
    if let Err(err) = res {
        eprintln!("drv-authd: {err}");
        std::process::exit(1);
    }
}
