//! Serving side, shared by the identity daemon and the tests. One thread per connection.
//!
//! Anyone may connect: the socket is world-connectable and the peer's UID (`SO_PEERCRED`) is
//! what every answer is keyed on. What a peer may ask is the handler's decision.

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::{io, thread};

use crate::rpc::{self, Request, Response};
use crate::{AppPolicy, Grant, PolicyStore};

/// What a daemon must answer. `peer` is the asking process's UID.
pub trait Handler: Send + Sync {
    /// Policy for `uid`, asked by `peer`. Never fails for a peer asking about itself; other
    /// UIDs need the peer to hold [`Grant::Lookup`].
    fn lookup(&self, peer: u32, uid: u32) -> Result<AppPolicy, String>;
    /// Start the named app (see [`Request::Launch`]); returns the UID it runs as.
    fn launch(&self, peer: u32, app: &str) -> Result<u32, String>;
    /// The names `launch` accepts (see [`Request::Apps`]).
    fn apps(&self, peer: u32) -> Result<Vec<String>, String>;
    /// The peer's `Hello`, before the version goes back. A hook, not a decision.
    fn hello(&self, _peer: u32) {}
}

/// A file-backed store answers lookups and cannot launch anything.
impl Handler for PolicyStore {
    fn lookup(&self, peer: u32, uid: u32) -> Result<AppPolicy, String> {
        let asker = PolicyStore::lookup(self, peer);
        if peer != uid && !asker.has(Grant::Lookup) {
            return Err(format!("uid {peer} may not look up other uids"));
        }
        Ok((*PolicyStore::lookup(self, uid)).clone())
    }

    fn launch(&self, _peer: u32, _app: &str) -> Result<u32, String> {
        Err("this policy daemon does not launch apps".to_owned())
    }

    fn apps(&self, _peer: u32) -> Result<Vec<String>, String> {
        Err("this policy daemon does not launch apps".to_owned())
    }
}

pub fn serve<H: Handler + 'static>(listener: UnixListener, handler: Arc<H>) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept()?;
        let peer = match rustix::net::sockopt::socket_peercred(&stream) {
            Ok(peer) => peer.uid.as_raw(),
            Err(err) => {
                eprintln!("identity daemon: no peer credentials: {err}");
                continue;
            }
        };
        let handler = handler.clone();
        thread::spawn(move || {
            // A peer hanging up is the normal end of a connection.
            let _ = serve_connection(stream, peer, &*handler);
        });
    }
}

/// Answers requests from `peer` on `stream` until it closes or sends something malformed.
pub fn serve_connection(stream: UnixStream, peer: u32, handler: &dyn Handler) -> io::Result<()> {
    loop {
        let request: Request = match rpc::read_msg(&stream) {
            Ok(request) => request,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        };
        let response = match request {
            Request::Hello { .. } => {
                handler.hello(peer);
                Response::Hello {
                    version: rpc::VERSION,
                }
            }
            Request::Lookup { uid } => match handler.lookup(peer, uid) {
                Ok(policy) => Response::Policy(policy),
                Err(err) => Response::Error(err),
            },
            Request::Launch { app } => match handler.launch(peer, &app) {
                Ok(uid) => Response::Launched { uid },
                Err(err) => Response::Error(err),
            },
            Request::Apps => match handler.apps(peer) {
                Ok(apps) => Response::Apps(apps),
                Err(err) => Response::Error(err),
            },
        };
        rpc::write_msg(&stream, &response)?;
    }
}
