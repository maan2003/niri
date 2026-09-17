//! What crosses from an app's private session bus to the human's session.
//!
//! `niri-bridge serve` runs as the human. Every request is keyed on the peer UID
//! (`SO_PEERCRED`) and the identity daemon's answer for it; the app never names itself.
//! `niri-bridge app` runs in the app's UID on its private bus and claims the desktop names
//! apps expect (`org.freedesktop.Notifications`); it is a convenience, never a boundary.

pub use niri_policy::rpc::{read_msg, write_msg};
use serde::{Deserialize, Serialize};

/// Where apps find the server's socket.
pub const SOCKET_ENV: &str = "NIRI_BRIDGE_SOCKET";

/// First message from the shim; the server drops anything else.
pub const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Notify {
        replaces: u32,
        summary: String,
        body: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Response {
    Notified { id: u32 },
    Error(String),
}
