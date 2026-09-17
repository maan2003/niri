//! Serving side, shared by the identity daemon and the tests. One thread per connection.

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::{io, thread};

use crate::rpc::{self, Request, Response};
use crate::{AppPolicy, PolicyStore};

/// What a daemon must answer. `lookup` never fails: an unknown UID gets the daemon's default.
pub trait Handler: Send + Sync {
    fn lookup(&self, uid: u32) -> AppPolicy;
    /// Start the named app (see [`Request::Launch`]); returns the UID it runs as.
    fn launch(&self, app: &str, env: &[(String, String)]) -> Result<u32, String>;
}

/// A file-backed store answers lookups and cannot launch anything.
impl Handler for PolicyStore {
    fn lookup(&self, uid: u32) -> AppPolicy {
        (*PolicyStore::lookup(self, uid)).clone()
    }

    fn launch(&self, _app: &str, _env: &[(String, String)]) -> Result<u32, String> {
        Err("this policy daemon does not launch apps".to_owned())
    }
}

/// Serves our own UID only: the compositor and the daemon belong to the same human, and
/// nothing else may look up policy or launch. Socket permissions say the same; this does not
/// rely on them.
pub fn serve<H: Handler + 'static>(listener: UnixListener, handler: Arc<H>) -> io::Result<()> {
    let me = rustix::process::getuid();
    loop {
        let (stream, _) = listener.accept()?;
        match rustix::net::sockopt::socket_peercred(&stream) {
            Ok(peer) if peer.uid == me => {}
            Ok(peer) => {
                eprintln!("policy daemon: refusing uid {}", peer.uid.as_raw());
                continue;
            }
            Err(err) => {
                eprintln!("policy daemon: no peer credentials: {err}");
                continue;
            }
        }
        let handler = handler.clone();
        thread::spawn(move || {
            // A peer hanging up is the normal end of a connection.
            let _ = serve_connection(stream, &*handler);
        });
    }
}

/// Answers requests on `stream` until it closes or sends something malformed.
pub fn serve_connection(stream: UnixStream, handler: &dyn Handler) -> io::Result<()> {
    loop {
        let request: Request = match rpc::read_msg(&stream) {
            Ok(request) => request,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        };
        let response = match request {
            Request::Hello { .. } => Response::Hello {
                version: rpc::VERSION,
            },
            Request::Lookup { uid } => Response::Policy(handler.lookup(uid)),
            Request::Launch { app, env } => match handler.launch(&app, &env) {
                Ok(uid) => Response::Launched { uid },
                Err(err) => Response::Error(err),
            },
        };
        rpc::write_msg(&stream, &response)?;
    }
}
