//! Wire protocol of the identity daemon: one Unix socket, requests and responses in lock step,
//! each message a little-endian `u32` length followed by a postcard payload.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::AppPolicy;

/// Bumped on any incompatible change; the daemon answers `Hello` with its own version.
pub const VERSION: u32 = 4;

/// Frames larger than this are refused, so a misbehaving peer cannot make us allocate freely.
pub const MAX_FRAME: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Hello {
        version: u32,
    },
    /// Policy for a UID. Anyone may ask about their own UID; other UIDs need
    /// [`Grant::Lookup`](crate::Grant::Lookup).
    Lookup {
        uid: u32,
    },
    /// Start an app by manifest name. Not a privilege: anyone may ask. Arguments and
    /// environment come from the manifest and the daemon, never from here.
    Launch {
        app: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Hello {
        version: u32,
    },
    Policy(AppPolicy),
    /// The app was started under this UID.
    Launched {
        uid: u32,
    },
    /// The request was understood and refused (unknown app, not allowed, spawner down, ...).
    Error(String),
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
