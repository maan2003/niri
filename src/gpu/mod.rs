//! Out-of-process rendering: the core describes each frame as a scene of nodes, the GPU
//! process (the only thing that touches Mesa) tracks damage and draws it.

#[cfg(feature = "xdp-gnome-screencast")]
pub mod cast;
pub mod client;
pub mod convert;
pub mod cursor;
pub mod draw;
pub mod drm;
pub mod exec;
pub mod gl;
pub mod protocol;
pub mod record;
pub mod remote;
pub mod scene;
pub mod server;
pub mod testing;
pub mod transport;

#[cfg(test)]
mod tests;
