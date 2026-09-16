//! Renderer-independent scene description sent from the core to the GPU process.
//!
//! Nodes are listed front-to-back, matching smithay's element convention.
//! Ids and commit counters are chosen by the core and stay stable across frames
//! so the GPU side can do damage tracking.

use serde::{Deserialize, Serialize};

/// Identity of a client buffer registered with the GPU process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BufferId(pub u64);

/// Stable identity of a scene node across frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Rect<T> {
    pub x: T,
    pub y: T,
    pub w: T,
    pub h: T,
}

impl<T> Rect<T> {
    pub fn new(x: T, y: T, w: T, h: T) -> Self {
        Self { x, y, w, h }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transform {
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Unspecified,
    Cursor,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Node {
    SolidColor {
        id: NodeId,
        commit: u64,
        /// Physical coordinates.
        geometry: Rect<i32>,
        /// Premultiplied RGBA.
        color: [f32; 4],
    },
    Surface {
        id: NodeId,
        commit: u64,
        buffer: BufferId,
        /// Physical coordinates.
        location: (f64, f64),
        buffer_scale: i32,
        transform: Transform,
        alpha: f32,
        /// Logical coordinates within the buffer, `None` for the whole buffer.
        src: Option<Rect<f64>>,
        /// Logical size to scale to, `None` for the buffer's own size.
        size: Option<(i32, i32)>,
        /// Buffer coordinates.
        opaque: Vec<Rect<i32>>,
        kind: Kind,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Scene {
    /// Physical size of the target.
    pub size: (i32, i32),
    pub scale: f64,
    pub transform: Transform,
    /// Front-to-back.
    pub nodes: Vec<Node>,
}
