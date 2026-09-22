//! What an app says on drv-files' socket, and hears back: one postcard message per
//! `SOCK_SEQPACKET` datagram (`drv_policy::seq`). The app says open or save; the person
//! says which file. Nothing here names a path the app could pick.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
/// Where apps find the socket.
pub const SOCKET: &str = "/run/drv/files.sock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Kind {
    /// An existing file, read-only for the app.
    Open,
    /// A file to write, created if the person names a new one; `name` is offered.
    Save { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToFiles {
    Hello { version: u32 },
    /// A file for the app: `Chosen`, `Cancelled` or `Failed`. The app numbers `req`.
    Choose { req: u64, kind: Kind },
    /// Withdraw a `Choose` still waiting on the person. No answer.
    Cancel { req: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FromFiles {
    Hello { version: u32 },
    /// Paths under the documents mount that the app's uid alone may open.
    Chosen { req: u64, paths: Vec<String> },
    /// The person said no, or the request was withdrawn.
    Cancelled { req: u64 },
    Failed { req: u64, reason: String },
}
