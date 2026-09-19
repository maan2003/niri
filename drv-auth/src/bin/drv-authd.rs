//! `drv-authd`: verifies the lock PIN and pushes the unlock to the compositor. Its two peers
//! are fds from the supervisor, already connected: `compositor` (unlocks go there) and
//! `locker` (verifies come from there). Nothing on the filesystem, nobody to check.

use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::{Parser, Subcommand};
use drv_auth::{Auth, Event, Outcome, Request, Response, Store};
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
                drv_os::say!("PIN accepted; unlocking");
                let event = Event::Unlock {
                    idle_timeout_ms: shared.idle_timeout.as_millis() as u64,
                };
                let mut comp = shared.compositor.lock().unwrap();
                match comp.as_ref().map(|c| drv_auth::send_msg(c, &event)) {
                    Some(Ok(())) => Response::Granted,
                    Some(Err(err)) => {
                        drv_os::say!("compositor connection lost: {err}");
                        *comp = None;
                        Response::Error("compositor not connected".into())
                    }
                    None => Response::Error("compositor not connected".into()),
                }
            }
            Ok(Outcome::Denied { retry_after }) => {
                drv_os::say!("PIN rejected; retry after {retry_after:?}");
                Response::Denied {
                    retry_after_ms: retry_after.as_millis() as u64,
                }
            }
            Err(err) => {
                drv_os::say!("verify failed: {err}");
                Response::Error(err.to_string())
            }
        };
        drv_auth::send_msg(&sock, &reply)?;
    }
}

fn serve(state_dir: PathBuf, idle_timeout: u64) -> io::Result<()> {
    let mut fds = drv_os::fds::take()?;
    let compositor = fds.socket("compositor", drv_os::fds::Kind::SeqPacket)?;
    let locker = fds.socket("locker", drv_os::fds::Kind::SeqPacket)?;
    let store = Store::new(state_dir);
    if !store.has_pin() {
        drv_os::say!("no PIN enrolled: run `drv-authd set-pin`; every verify is refused until then");
    }
    // From here on: our two fds, threads, and the state directory.
    if drv_os::seccomp::enabled() {
        let mut allow = drv_os::seccomp::Allowlist::base()?;
        allow.write_files()?;
        allow.apply("drv-authd")?;
        drv_os::say!("seccomp: syscall allowlist applied");
    }
    let shared = Arc::new(Shared {
        auth: Mutex::new(Auth::new(store)),
        compositor: Mutex::new(Some(compositor)),
        idle_timeout: Duration::from_secs(idle_timeout),
    });
    drv_os::say!("serving the locker");
    serve_verifier(&shared, locker)?;
    // The locker is gone; the supervisor restarts the set, us included.
    Err(io::Error::other("the locker hung up"))
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
        drv_os::say!("drv-authd: {err}");
        std::process::exit(1);
    }
}
