//! Wire protocol of the identity daemon: one Unix socket, requests and responses in lock step,
//! each message a little-endian `u32` length followed by a postcard payload.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::AppPolicy;

/// Bumped on any incompatible change; the daemon answers `Hello` with its own version.
pub const VERSION: u32 = 7;

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
    /// Start an app by manifest name. Only on a launch channel: a socketpair the supervisor
    /// made between drv-appd and a launcher it started (the compositor). The public socket
    /// refuses it. Arguments and environment come from the manifest and the daemon, never
    /// from here.
    Launch {
        app: String,
    },
    /// The names `Launch` accepts, for a menu. Only on a launch channel.
    Apps,
    /// Start the app whose manifest declares the URI's scheme (`opens`), with the URI as its
    /// one extra argument. Only on a launch channel (the bridge's, for the OpenURI portal).
    /// The daemon refuses anything but a well-formed absolute URI it has a handler for.
    Open {
        uri: String,
    },
}

/// The scheme of an absolute URI, lowercased, if `uri` is one we would pass along: ASCII
/// printable only, no spaces, at most 8 KiB, `scheme:` as RFC 3986 spells it. Anything
/// else is not a URI to us, whatever the app meant by it.
pub fn uri_scheme(uri: &str) -> Option<String> {
    if uri.len() > 8192 || !uri.bytes().all(|b| b.is_ascii_graphic()) {
        return None;
    }
    let (scheme, _) = uri.split_once(':')?;
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        return None;
    }
    Some(scheme.to_ascii_lowercase())
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
    /// The launchable app names, in manifest order.
    Apps(Vec<String>),
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
