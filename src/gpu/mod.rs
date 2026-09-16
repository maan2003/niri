//! Out-of-process rendering: the core records renderer commands, the GPU process
//! (the only thing that touches Mesa) replays them.

#[cfg(feature = "xdp-gnome-screencast")]
pub mod cast;
pub mod client;
pub mod convert;
pub mod cursor;
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
