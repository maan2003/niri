//! `drv-authd`: verifies the lock PIN and pushes the unlock to the compositor. Its peers come
//! down the spawner's wire, already connected: the compositor's connection, and one verifier
//! connection per lock app launch. Nothing on the filesystem, nobody to check.

use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use drv_auth::{Auth, Event, Outcome, Request, Response, Store};
use drv_policy::wire::{self, Attach};
use zeroize::Zeroize;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon, under drv-spawnd.
    Serve {
        #[arg(long, default_value = "/var/lib/drv-auth")]
        state_dir: PathBuf,
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
    idle_timeout: Duration,
}

fn spawn_verifier(shared: &Arc<Shared>, sock: OwnedFd) {
    let shared = shared.clone();
    thread::spawn(move || {
        if let Err(err) = serve_verifier(&shared, sock) {
            eprintln!("verifier connection: {err}");
        }
    });
}

fn serve_verifier(shared: &Shared, sock: OwnedFd) -> io::Result<()> {
    loop {
        let mut secret = match drv_auth::recv_msg::<Request>(&sock) {
            Ok(Request::Verify { secret }) => secret,
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

fn serve(state_dir: PathBuf, idle_timeout: u64) -> io::Result<()> {
    let wire = wire::take()
        .ok_or_else(|| io::Error::other("no wire on fd 3: drv-authd runs under drv-supervisor"))?;
    let store = Store::new(state_dir);
    if !store.has_pin() {
        eprintln!("no PIN enrolled: run `drv-authd set-pin`; every verify is refused until then");
    }
    let shared = Arc::new(Shared {
        auth: Mutex::new(Auth::new(store)),
        compositor: Mutex::new(None),
        idle_timeout: Duration::from_secs(idle_timeout),
    });
    loop {
        match wire::recv_attach(&wire) {
            Ok((Attach::Compositor, sock)) => {
                // The compositor only listens; the newest connection replaces the old.
                eprintln!("compositor attached");
                *shared.compositor.lock().unwrap() = Some(sock);
            }
            Ok((Attach::Verifier, sock)) => spawn_verifier(&shared, sock),
            Ok((Attach::Verifiers, sock)) => {
                // drv-appd: one `Verifier` per app it launches with auth.
                eprintln!("drv-appd attached");
                let shared = shared.clone();
                thread::spawn(move || loop {
                    match wire::recv_attach(&sock) {
                        Ok((Attach::Verifier, verifier)) => spawn_verifier(&shared, verifier),
                        Ok((other, _)) => eprintln!("ignoring {other:?} from drv-appd"),
                        Err(err) => {
                            eprintln!("drv-appd's socket ended: {err}");
                            return;
                        }
                    }
                });
            }
            Ok((other, _)) => eprintln!("ignoring {other:?} on the wire"),
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                return Err(io::Error::other("the spawner closed the wire"));
            }
            Err(err) => eprintln!("wire: {err}"),
        }
    }
}

fn main() {
    let args = Args::parse();
    let res = match args.cmd {
        Cmd::Serve {
            state_dir,
            idle_timeout,
        } => serve(state_dir, idle_timeout),
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
