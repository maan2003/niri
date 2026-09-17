//! Serving side, shared by `niri-policyd` and the tests. One thread per connection; the store
//! is read-only after load.

use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::{io, thread};

use crate::rpc::{self, Request, Response};
use crate::PolicyStore;

pub fn serve(listener: UnixListener, store: Arc<PolicyStore>) -> io::Result<()> {
    loop {
        let (stream, _) = listener.accept()?;
        let store = store.clone();
        thread::spawn(move || {
            // A peer hanging up is the normal end of a connection.
            let _ = serve_connection(stream, &store);
        });
    }
}

/// Answers requests on `stream` until it closes or sends something malformed.
pub fn serve_connection(stream: UnixStream, store: &PolicyStore) -> io::Result<()> {
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
            Request::Lookup { uid } => Response::Policy((*store.lookup(uid)).clone()),
        };
        rpc::write_msg(&stream, &response)?;
    }
}
