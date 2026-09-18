//! The bridge's line to drv-portal: one postcard message per `SOCK_SEQPACKET` datagram
//! (`drv_policy::seq`). The bridge picks the id; the answer carries it back. Nothing here
//! names a path the app could pick: the app says what it wants, the person says which file.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Kind {
    /// An existing file, read-only for the app.
    Open,
    /// A file to write, created if the person names a new one.
    Save { name: String },
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
    /// The app withdrew the request; no answer follows.
    Cancel { id: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Hello { version: u32 },
    /// The pick, as paths under the documents mount that `uid` alone may open.
    Chosen { id: u64, paths: Vec<String> },
    Cancelled { id: u64 },
    Failed { id: u64, reason: String },
}
