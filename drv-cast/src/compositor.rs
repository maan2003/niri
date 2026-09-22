//! drv-cast's line to the compositor (the supervisor's `compositor`/`cast` pair, a
//! `SOCK_SEQPACKET`, postcard via `drv_policy::seq`). drv-cast starts and stops casts by
//! its own ids after the person has consented; the compositor tells it the PipeWire node
//! and when a cast ends. The compositor trusts the line: only drv-cast holds it.

use serde::{Deserialize, Serialize};

pub use crate::wire::{Cursor, Source};

pub const VERSION: u32 = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output {
    /// The connector name, what `Source::Screen` names.
    pub name: String,
    pub make: String,
    pub model: String,
    /// Logical size; `(0, 0)` while the output is off.
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Window {
    /// The compositor's id for the window, what `Source::Window` names.
    pub id: u64,
    /// What the window calls itself: the app chose it.
    pub title: String,
    /// The manifest name of the app whose window it is: the compositor knows.
    pub app: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToCompositor {
    Hello { version: u32 },
    /// Answered with `Outputs`.
    Outputs,
    /// Answered with `Windows`.
    Windows,
    /// Stream this source; `Started` names the node, or `Stopped` says it could not.
    Start {
        cast: u64,
        source: Source,
        cursor: Cursor,
    },
    Stop { cast: u64 },
    /// Which apps hold the microphone and the camera right now, by manifest name, for the
    /// on-screen indicator. Sent whenever the set changes; empty lists clear it.
    Devices { mic: Vec<String>, camera: Vec<String> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FromCompositor {
    Hello { version: u32 },
    Outputs(Vec<Output>),
    Windows(Vec<Window>),
    /// The stream is up, `width` by `height` pixels.
    Started { cast: u64, node_id: u32, width: i32, height: i32 },
    /// The cast is gone, whoever ended it. Also the answer to a `Start` that failed.
    Stopped { cast: u64 },
    /// The person's kill switch: every device they allowed is revoked (the casts get
    /// `Stopped` each).
    Revoke,
}
