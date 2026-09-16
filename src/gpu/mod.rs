//! GPU process: everything that touches Mesa, GBM, EGL or KMS runs here, in a
//! separate process from the compositor core. See ARCH-gpu-process-split.
//!
//! The core never opens a render node and never maps client memory. It
//! validates and forwards buffer fds, then sends a scene tree per frame.

pub mod client;
pub mod protocol;
pub mod scene;
pub mod server;
pub mod testing;
pub mod transport;

#[cfg(test)]
mod tests;
