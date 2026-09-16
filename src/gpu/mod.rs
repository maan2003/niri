//! Out-of-process rendering: the core records renderer commands, the GPU process
//! (the only thing that touches Mesa) replays them.

pub mod client;
pub mod convert;
pub mod drm;
pub mod exec;
pub mod gl;
pub mod protocol;
pub mod remote;
pub mod server;
pub mod testing;
pub mod transport;

#[cfg(test)]
mod tests;
