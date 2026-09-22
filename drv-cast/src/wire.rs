//! What an app says on drv-cast's socket, and hears back: one postcard message per
//! `SOCK_SEQPACKET` datagram (`drv_policy::seq`), fds by `SCM_RIGHTS`. The app says what it
//! wants to share or use; the person says which screen, and whether. Nothing here names a
//! screen the app could pick.
//!
//! The app numbers `req` and `session`; the answer carries `req` back. `CastClosed` names
//! the session when the person or the compositor ends a cast.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
/// Where apps find the socket.
pub const SOCKET: &str = "/run/drv-cast/cast.sock";

/// What a cast shows: a whole screen by connector name, or one window by the compositor's
/// id for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    Screen(String),
    Window(u64),
}

/// How the app wants the pointer in a screencast (the portal's `cursor_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cursor {
    Hidden,
    Embedded,
    Metadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToCast {
    Hello { version: u32 },
    /// Share a screen or a window: the person picks. `Cast` when it streams, then
    /// `CastClosed { session }` when it ends; or `Cancelled` or `Failed` instead.
    Cast {
        req: u64,
        session: u64,
        cursor: Cursor,
        /// What the app will take: whole screens, windows, or both.
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
    /// Withdraw a `Cast` or `Camera` still waiting on the person. No answer.
    Cancel { req: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FromCast {
    Hello { version: u32 },
    Cast {
        req: u64,
        node_id: u32,
        source: Source,
        /// The stream's size in pixels.
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
    /// The person said no, or the request was withdrawn.
    Cancelled { req: u64 },
    Failed { req: u64, reason: String },
}

impl FromCast {
    /// The request this answers, if it answers one.
    pub fn req(&self) -> Option<u64> {
        match self {
            FromCast::Cast { req, .. }
            | FromCast::Remote { req }
            | FromCast::Granted { req }
            | FromCast::Present { req, .. }
            | FromCast::Cancelled { req }
            | FromCast::Failed { req, .. } => Some(*req),
            FromCast::Hello { .. } | FromCast::CastClosed { .. } => None,
        }
    }
}
