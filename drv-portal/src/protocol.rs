//! The bridge's line to drv-portal: one postcard message per `SOCK_SEQPACKET` datagram
//! (`drv_policy::seq`). The bridge picks the id; the answers carry it back. Nothing here
//! names a path or a screen the app could pick: the app says what it wants, the person
//! says which file, or which screen.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Kind {
    /// An existing file, read-only for the app.
    Open,
    /// A file to write, created if the person names a new one.
    Save { name: String },
}

/// How the app wants the pointer in a screencast (the portal's `cursor_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cursor {
    Hidden,
    Embedded,
    Metadata,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Hello { version: u32 },
    /// Ask the person for a file for `app` (its manifest name), which runs as `uid`.
    Choose {
        id: u64,
        app: String,
        uid: u32,
        title: String,
        kind: Kind,
    },
    /// Ask the person for a screen to share with `app`. `Cast` answers when it streams;
    /// `Closed` follows when it ends on our side.
    Cast {
        id: u64,
        app: String,
        uid: u32,
        cursor: Cursor,
    },
    /// The app withdrew the request, or closed its session: the dialog goes down, or the
    /// cast stops. No answer follows.
    Cancel { id: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Hello { version: u32 },
    /// The pick, as paths under the documents mount that `uid` alone may open.
    Chosen { id: u64, paths: Vec<String> },
    /// The screen streams on this PipeWire node. The bridge restricts the app's PipeWire
    /// connection to it.
    Cast {
        id: u64,
        node_id: u32,
        output: String,
        width: i32,
        height: i32,
    },
    /// The cast ended: the person stopped it, or the screen went away.
    Closed { id: u64 },
    Cancelled { id: u64 },
    Failed { id: u64, reason: String },
}
