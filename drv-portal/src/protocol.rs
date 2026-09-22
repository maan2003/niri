//! The bridge's line to drv-portal, and the ssh agent's: one postcard message per
//! `SOCK_SEQPACKET` datagram (`drv_policy::seq`). The asker picks the id; the answers carry
//! it back. Nothing here names a path or a screen the app could pick: the app says what it
//! wants, the person says which file, or which screen. The agent's two requests put the
//! authenticator's prompts in front of the person, where no app sees the PIN.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 6;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Kind {
    /// An existing file, read-only for the app.
    Open,
    /// A file to write, created if the person names a new one.
    Save { name: String },
}

/// A device an app may be let use: the microphone stands for any audio capture (sink
/// monitors included), the camera for any video source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Device {
    Microphone,
    Camera,
}

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
    /// Ask the person for a screen or a window to share with `app`. `Cast` answers when it
    /// streams; `Closed` follows when it ends on our side.
    Cast {
        id: u64,
        app: String,
        uid: u32,
        cursor: Cursor,
        /// What the app will take: whole screens, windows, or both.
        screens: bool,
        windows: bool,
        /// A token from an earlier `Cast` answer to this app: the same screen again, with
        /// no dialog, if the consent still stands.
        again: Option<String>,
    },
    /// Ask the person to let `app` use `device`. `Granted` answers; `Closed` follows when
    /// the person revokes it.
    Grant {
        id: u64,
        app: String,
        uid: u32,
        device: Device,
    },
    /// The ssh agent wants the authenticator's PIN for `app`'s use of it: `prompt` says
    /// what for. `Pin` answers, or `Cancelled`.
    Pin {
        id: u64,
        app: String,
        uid: u32,
        prompt: String,
    },
    /// The ssh agent waits for a touch on the authenticator for `app`. No answer but
    /// `Cancelled` if the person refuses; the agent's `Cancel` takes it down.
    Touch {
        id: u64,
        app: String,
        uid: u32,
        prompt: String,
    },
    /// The app withdrew the request, or closed its session: the dialog goes down, or the
    /// cast stops. No answer follows.
    Cancel { id: u64 },
    /// The app is gone: what the person allowed it ends with it.
    Forget { app: String, uid: u32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Hello { version: u32 },
    /// The pick, as paths under the documents mount that `uid` alone may open.
    Chosen { id: u64, paths: Vec<String> },
    /// The source streams on this PipeWire node. The bridge restricts the app's PipeWire
    /// connection to it.
    Cast {
        id: u64,
        node_id: u32,
        source: Source,
        /// The stream's size in pixels.
        width: i32,
        height: i32,
        /// Names this consent in a later `Cast { again }` by the same app.
        token: String,
    },
    /// The person allows the device, until `Closed`.
    Granted { id: u64 },
    /// The PIN the person typed for the agent.
    Pin { id: u64, pin: String },
    /// The cast ended: the person stopped it, or the screen went away. Or the person
    /// revoked the device.
    Closed { id: u64 },
    Cancelled { id: u64 },
    Failed { id: u64, reason: String },
}
