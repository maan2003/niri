//! What the shim on an app's private bus says to the server, and hears back: one postcard
//! message per `SOCK_SEQPACKET` datagram (`drv_policy::seq`), fds by `SCM_RIGHTS`. This is
//! the whole of what an app can say to the trusted side: a few strings and numbers of fixed
//! shape. The D-Bus the app speaks ends in the shim, which runs as the app.
//!
//! The shim picks `req`; the answer carries it back. A cast session is a `session` the shim
//! numbers too; `CastClosed` names it when the person or the compositor ends the cast.

use serde::{Deserialize, Serialize};

pub use drv_portal::protocol::{Cursor, Kind as Chooser, Source};

pub const VERSION: u32 = 1;

/// Text the server passes on to the person (titles, notifications) is cut here.
pub const MAX_TEXT: usize = 2048;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToServer {
    Hello { version: u32 },
    /// A file for the app: `Files`, `Cancelled` or `Failed`.
    Choose { req: u64, title: String, kind: Chooser },
    /// Share a screen or a window: the person picks. `Cast` when it streams, then
    /// `CastClosed { session }` when it ends; or `Cancelled` or `Failed` instead.
    Cast {
        req: u64,
        session: u64,
        cursor: Cursor,
        screens: bool,
        windows: bool,
        /// A token from an earlier `Cast` answer: the same source again, with no dialog.
        again: Option<String>,
    },
    /// A PipeWire connection that sees the session's node: `Remote` with the fd, or `Failed`.
    CastRemote { req: u64, session: u64 },
    /// The session is over: the cast stops, its remotes are cut. No answer.
    CastClose { session: u64 },
    /// May the app use the camera? `Granted` or `Cancelled`; asked once per run.
    Camera { req: u64 },
    /// A PipeWire connection that sees the cameras: `Remote` or `Failed`.
    CameraRemote { req: u64 },
    /// Is there a camera at all? `Present`.
    CameraPresent { req: u64 },
    /// Open a URI with its manifest handler: `Done` or `Failed`.
    Open { req: u64, uri: String },
    /// A notification, under the app's manifest name: `Notified` or `Failed`.
    Notify { req: u64, replaces: u32, summary: String, body: String },
    /// Withdraw a `Choose` or `Cast` still waiting on the person. No answer.
    Cancel { req: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToShim {
    Hello { version: u32 },
    /// Paths under the documents mount, the app's UID alone may open them.
    Files { req: u64, paths: Vec<String> },
    Cast {
        req: u64,
        node_id: u32,
        source: Source,
        width: i32,
        height: i32,
        /// Names this consent in a later `Cast { again }`.
        token: String,
    },
    CastClosed { session: u64 },
    /// One fd rides along.
    Remote { req: u64 },
    Granted { req: u64 },
    Present { req: u64, present: bool },
    Done { req: u64 },
    Notified { req: u64, id: u32 },
    /// The person said no, or the request was withdrawn.
    Cancelled { req: u64 },
    Failed { req: u64, reason: String },
}

impl ToShim {
    /// The request this answers, if it answers one.
    pub fn req(&self) -> Option<u64> {
        match self {
            ToShim::Files { req, .. }
            | ToShim::Cast { req, .. }
            | ToShim::Remote { req }
            | ToShim::Granted { req }
            | ToShim::Present { req, .. }
            | ToShim::Done { req }
            | ToShim::Notified { req, .. }
            | ToShim::Cancelled { req }
            | ToShim::Failed { req, .. } => Some(*req),
            ToShim::Hello { .. } | ToShim::CastClosed { .. } => None,
        }
    }
}

/// `s` cut to `MAX_TEXT` bytes on a character boundary.
pub fn clip(s: &str) -> &str {
    if s.len() <= MAX_TEXT {
        return s;
    }
    let mut end = MAX_TEXT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
