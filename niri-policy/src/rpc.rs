//! Wire protocol between the compositor and the policy daemon: one Unix socket, requests and
//! responses in lock step, each message a little-endian `u32` length followed by a postcard
//! payload.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::AppPolicy;

/// Bumped on any incompatible change; the daemon answers `Hello` with its own version.
pub const VERSION: u32 = 1;

/// Frames larger than this are refused, so a misbehaving peer cannot make us allocate freely.
pub const MAX_FRAME: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Hello {
        version: u32,
    },
    /// Policy for a UID. The daemon answers `Response::Policy` for every UID, using its default
    /// record for ones it does not know.
    Lookup {
        uid: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Hello { version: u32 },
    Policy(AppPolicy),
}

pub fn write_msg<T: Serialize>(mut w: impl Write, msg: &T) -> io::Result<()> {
    let payload = postcard::to_stdvec(msg).map_err(|e| io::Error::other(e.to_string()))?;
    if payload.len() > MAX_FRAME {
        return Err(io::Error::other("message too large"));
    }
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(&payload)?;
    w.flush()
}

pub fn read_msg<T: DeserializeOwned>(mut r: impl Read) -> io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other("message too large"));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    postcard::from_bytes(&buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}
