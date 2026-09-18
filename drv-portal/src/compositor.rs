//! drv-portal's line to the compositor (the supervisor's `compositor`/`portal` pair, a
//! `SOCK_SEQPACKET`, postcard via `drv_policy::seq`). The portal starts and stops casts
//! by its own ids after the person has consented; the compositor tells it the PipeWire node
//! and when a cast ends. The compositor trusts the line: only the portal holds it.

use serde::{Deserialize, Serialize};

pub use crate::protocol::Cursor;

pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Output {
    /// The connector name, what `Start` names.
    pub name: String,
    pub make: String,
    pub model: String,
    /// Logical size; `(0, 0)` while the output is off.
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToCompositor {
    Hello { version: u32 },
    /// Answered with `Outputs`.
    Outputs,
    /// Stream this output; `Started` names the node, or `Stopped` says it could not.
    Start {
        cast: u64,
        output: String,
        cursor: Cursor,
    },
    Stop { cast: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToPortal {
    Hello { version: u32 },
    Outputs(Vec<Output>),
    Started { cast: u64, node_id: u32 },
    /// The cast is gone, whoever ended it. Also the answer to a `Start` that failed.
    Stopped { cast: u64 },
}
