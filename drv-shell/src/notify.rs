//! What an app says on the shell's notification socket, and hears back. The shell shows
//! `summary` and `body` under the app's manifest name, as text.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
/// Text from the app is cut here before it is shown.
pub const MAX_TEXT: usize = 2048;
/// Where apps find the socket.
pub const SOCKET: &str = "/run/drv-shell/notify.sock";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToShell {
    Hello { version: u32 },
    /// `Notified` with the notification's id, or `Failed`. `replaces`, if not 0, is an
    /// earlier id of this app's to update in place.
    Notify { req: u64, replaces: u32, summary: String, body: String },
    /// Takes one of this app's notifications down. No answer.
    Close { id: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FromShell {
    Hello { version: u32 },
    Notified { req: u64, id: u32 },
    Failed { req: u64, reason: String },
}
