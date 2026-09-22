//! A trusted service asks the person something through the shell. Ids are the asker's;
//! each line has its own. Every string here comes from the asker, never from an app: the
//! shell shows what it is told.

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;

/// One thing to pick from: `key` comes back; `name` and `detail` are shown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub key: String,
    pub name: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Hello { version: u32 },
    /// "`app` wants to `what`", `note` under it: `Yes` or `Cancelled`.
    Confirm { id: u64, app: String, uid: u32, what: String, note: String },
    /// A secret typed at the shell, never in the app: `Secret` or `Cancelled`.
    Secret { id: u64, app: String, uid: u32, what: String, prompt: String },
    /// Shown until the asker's `Cancel`; `Cancelled` if the person refuses first.
    Touch { id: u64, app: String, uid: u32, what: String, prompt: String },
    /// One of `choices`: `Picked` or `Cancelled`. Sent again under the same id, it replaces
    /// the list while the dialog is up or waiting.
    Pick { id: u64, app: String, uid: u32, what: String, note: String, choices: Vec<Choice> },
    /// The asker withdrew it: the dialog goes down, no answer.
    Cancel { id: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Hello { version: u32 },
    Yes { id: u64 },
    Secret { id: u64, secret: String },
    Picked { id: u64, key: String },
    /// The person said no.
    Cancelled { id: u64 },
}
