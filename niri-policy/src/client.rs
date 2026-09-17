//! The compositor's side: ask the daemon, cache per UID, fail closed. There is no mode without
//! a daemon: nobody is trusted unless a daemon says so.

use std::collections::HashMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::rpc::{self, Request, Response};
use crate::AppPolicy;

/// How long one lookup may take. The daemon is local and answers from memory; anything slower
/// is a stuck daemon, and stalling the compositor on it is worse than failing closed.
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

    pub fn from_stream(stream: UnixStream) -> io::Result<Self> {
        hello(&stream)?;
        Ok(Self {
            source: Source::Stream(Some(stream)),
            cache: HashMap::new(),
        })
    }

    /// The daemon's answer for `uid`, cached. An error means the daemon could not be reached or
    /// misbehaved; the caller decides what to do (the compositor uses [`AppPolicy::unknown`]).
    pub fn lookup(&mut self, uid: u32) -> io::Result<Arc<AppPolicy>> {
        if let Some(policy) = self.cache.get(&uid) {
            return Ok(policy.clone());
        }
        let policy = Arc::new(self.ask(uid)?);
        self.cache.insert(uid, policy.clone());
        Ok(policy)
    }

    fn ask(&mut self, uid: u32) -> io::Result<AppPolicy> {
        match &mut self.source {
            Source::Socket { path, conn } => {
                if conn.is_none() {
                    *conn = Some(open(path)?);
                }
                let res = roundtrip(conn.as_ref().unwrap(), uid);
                if res.is_err() {
                    *conn = None;
                }
                res
            }
            Source::Stream(conn) => {
                let stream = conn
                    .as_ref()
                    .ok_or_else(|| io::Error::other("policy stream closed"))?;
                let res = roundtrip(stream, uid);
                if res.is_err() {
                    *conn = None;
                }
                res
            }
        }
    }
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
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected reply to hello: {other:?}"),
        )),
    }
}

fn roundtrip(stream: &UnixStream, uid: u32) -> io::Result<AppPolicy> {
    rpc::write_msg(stream, &Request::Lookup { uid })?;
    match rpc::read_msg::<Response>(stream)? {
        Response::Policy(policy) => Ok(policy),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected reply to lookup: {other:?}"),
        )),
    }
}
