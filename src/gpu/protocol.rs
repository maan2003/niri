//! Messages between the compositor core and the GPU process.
//!
//! Every `Request` currently gets exactly one `Event` in reply. That keeps the
//! first slice simple; the frame path will become asynchronous later.
//! File descriptors travel out of band (SCM_RIGHTS) attached to the frame that
//! references them, in the order the request lists them.

use serde::{Deserialize, Serialize};

use super::scene::{BufferId, Rect, Scene};

pub const PROTOCOL_VERSION: u32 = 1;

/// A `wl_shm`-style buffer. One fd attached: the pool, sealed against shrinking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShmDesc {
    /// Size of the pool in bytes.
    pub size: usize,
    pub offset: i32,
    pub stride: i32,
    pub width: i32,
    pub height: i32,
    /// DRM fourcc.
    pub format: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaneDesc {
    pub offset: u32,
    pub stride: u32,
}

/// A dmabuf. `planes.len()` fds attached, in plane order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmabufDesc {
    pub width: i32,
    pub height: i32,
    /// DRM fourcc.
    pub format: u32,
    pub modifier: u64,
    pub planes: Vec<PlaneDesc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Request {
    RegisterShm { id: BufferId, desc: ShmDesc },
    RegisterDmabuf { id: BufferId, desc: DmabufDesc },
    /// The client committed new contents into an shm buffer.
    UpdateShm { id: BufferId, damage: Vec<Rect<i32>> },
    DestroyBuffer { id: BufferId },
    /// Render a scene and return the pixels. Screenshots, colour pick, tests.
    RenderToImage {
        scene: Scene,
        /// DRM fourcc of the returned pixels.
        format: u32,
    },
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub width: i32,
    pub height: i32,
    /// DRM fourcc.
    pub format: u32,
    /// Tightly packed rows, top-down.
    pub pixels: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    Ready { version: u32, renderer: String },
    Ack,
    Image(Image),
    Error { message: String },
    Done,
}
