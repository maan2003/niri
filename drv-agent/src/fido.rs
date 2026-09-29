//! The FIDO door (`/run/drv/fido`, a `SOCK_SEQPACKET` socket in every app's root): one
//! WebAuthn ceremony per connection, for the UIDs whose manifest record lists the origin
//! under `fido`. One datagram each way. The request and the answer carry the JSON of the
//! linux-credentials portal (WebAuthn's `PublicKeyCredential*OptionsJSON` in, a
//! `PublicKeyCredential` out, buffers as unpadded base64url), which the shim moves between
//! the app's bus and here without reading it.

use serde::{Deserialize, Serialize};

pub const SOCKET: &str = "/run/drv/fido";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// `navigator.credentials.create()`: `public_key` is the creation options JSON.
    Create { origin: String, public_key: String },
    /// `navigator.credentials.get()`: `public_key` is the request options JSON.
    Get { origin: String, public_key: String },
}

impl Request {
    pub fn origin(&self) -> &str {
        match self {
            Self::Create { origin, .. } | Self::Get { origin, .. } => origin,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Reply {
    /// The `PublicKeyCredential` JSON: a registration or an authentication response.
    Credential { json: String },
    /// Nothing came of it: the origin is not the app's, no authenticator, the person
    /// refused, the authenticator said no.
    Failed { reason: String },
}
