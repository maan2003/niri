//! The asking side: ask the daemon, cache per UID, fail closed. There is no mode without a
//! daemon: nobody may do anything unless a daemon says so.

use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::rpc::{self, Request, Response};
use crate::AppPolicy;

/// How long one request may take. Lookups are answered from memory; a launch is one fork
/// request to the spawner. Anything slower is a stuck daemon, and stalling the compositor on it
/// is worse than failing.
const TIMEOUT: Duration = Duration::from_secs(2);

enum Source {
    /// Connect to this path, reconnecting after an error.
    Socket {
        path: PathBuf,
        conn: Option<UnixStream>,
    },
    /// One pre-connected stream, never reconnected (tests).
    Stream(Option<UnixStream>),
}

pub struct PolicyClient {
    source: Source,
    cache: HashMap<u32, Arc<AppPolicy>>,
}

impl PolicyClient {
    /// Connects now (so a missing daemon is caught at startup) and reconnects on demand later.
    pub fn connect(path: PathBuf) -> io::Result<Self> {
        let conn = open(&path)?;
        Ok(Self {
            source: Source::Socket {
                path,
                conn: Some(conn),
            },
            cache: HashMap::new(),
        })
    }

    /// A fresh connection to the same daemon, for another thread. Its own cache.
    pub fn reconnect(&self) -> io::Result<Self> {
        match &self.source {
            Source::Socket { path, .. } => Self::connect(path.clone()),
            Source::Stream(_) => Err(io::Error::other("cannot reconnect a test stream")),
        }
    }

    pub fn from_stream(stream: UnixStream) -> io::Result<Self> {
        hello(&stream)?;
        Ok(Self {
            source: Source::Stream(Some(stream)),
            cache: HashMap::new(),
        })
    }

    /// The daemon's answer for `uid`, cached. An error means the daemon could not be reached,
    /// refused us, or misbehaved; the caller decides what to do (the compositor uses
    /// [`AppPolicy::unknown`]).
    pub fn lookup(&mut self, uid: u32) -> io::Result<Arc<AppPolicy>> {
        if let Some(policy) = self.cache.get(&uid) {
            return Ok(policy.clone());
        }
        let policy = match self.request(&Request::Lookup { uid })? {
            Response::Policy(policy) => Arc::new(policy),
            Response::Error(err) => return Err(io::Error::other(err)),
            other => return Err(unexpected("lookup", other)),
        };
        self.cache.insert(uid, policy.clone());
        Ok(policy)
    }

    /// Asks the daemon to start the named app; returns the UID it runs as. A daemon-side
    /// refusal (unknown app, no spawner) comes back as an error too.
    pub fn launch(&mut self, app: String) -> io::Result<u32> {
        match self.request(&Request::Launch { app })? {
            Response::Launched { uid } => Ok(uid),
            Response::Error(err) => Err(io::Error::other(err)),
            other => Err(unexpected("launch", other)),
        }
    }

    /// The names `launch` accepts; only a launch channel answers.
    pub fn apps(&mut self) -> io::Result<Vec<String>> {
        match self.request(&Request::Apps)? {
            Response::Apps(apps) => Ok(apps),
            Response::Error(err) => Err(io::Error::other(err)),
            other => Err(unexpected("apps", other)),
        }
    }

    fn request(&mut self, request: &Request) -> io::Result<Response> {
        match &mut self.source {
            Source::Socket { path, conn } => {
                if conn.is_none() {
                    *conn = Some(open(path)?);
                }
                let res = roundtrip(conn.as_ref().unwrap(), request);
                if res.is_err() {
                    *conn = None;
                }
                res
            }
            Source::Stream(conn) => {
                let stream = conn
                    .as_ref()
                    .ok_or_else(|| io::Error::other("policy stream closed"))?;
                let res = roundtrip(stream, request);
                if res.is_err() {
                    *conn = None;
                }
                res
            }
        }
    }
}

fn unexpected(what: &str, response: Response) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("unexpected reply to {what}: {response:?}"),
    )
}

fn open(path: &PathBuf) -> io::Result<UnixStream> {
    let stream = UnixStream::connect(path)?;
    hello(&stream)?;
    Ok(stream)
}

fn hello(stream: &UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(TIMEOUT))?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    rpc::write_msg(
        stream,
        &Request::Hello {
            version: rpc::VERSION,
        },
    )?;
    match rpc::read_msg::<Response>(stream)? {
        Response::Hello { version } if version == rpc::VERSION => Ok(()),
        Response::Hello { version } => Err(io::Error::other(format!(
            "policy daemon speaks protocol {version}, we speak {}",
            rpc::VERSION
        ))),
        other => Err(unexpected("hello", other)),
    }
}

fn roundtrip(stream: &UnixStream, request: &Request) -> io::Result<Response> {
    rpc::write_msg(stream, request)?;
    rpc::read_msg::<Response>(stream)
}
