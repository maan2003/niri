//! Serving side, shared by the identity daemon and the tests. One thread per connection.

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::{io, thread};

use crate::rpc::{self, Request, Response};
use crate::{AppPolicy, PolicyStore};

/// What a daemon must answer. `lookup` never fails: an unknown UID gets the daemon's default.
pub trait Handler: Send + Sync {
    fn lookup(&self, uid: u32) -> AppPolicy;
    /// Start `command` (see [`Request::Launch`]); returns the UID it runs as.
    fn launch(&self, command: &[String], env: &[(String, String)]) -> Result<u32, String>;
}

/// A file-backed store answers lookups and cannot launch anything.
impl Handler for PolicyStore {
    fn lookup(&self, uid: u32) -> AppPolicy {
        (*PolicyStore::lookup(self, uid)).clone()
    }

    fn launch(&self, _command: &[String], _env: &[(String, String)]) -> Result<u32, String> {
        Err("this policy daemon does not launch apps".to_owned())
    }
}

pub fn serve<H: Handler + 'static>(listener: UnixListener, handler: Arc<H>) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept()?;
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
            Request::Launch { command, env } => match handler.launch(&command, &env) {
                Ok(uid) => Response::Launched { uid },
                Err(err) => Response::Error(err),
            },
        };
        rpc::write_msg(&stream, &response)?;
    }
}
