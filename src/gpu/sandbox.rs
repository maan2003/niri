//! Confinement of the GPU process.
//!
//! The process starts with everything it will ever need: the socket to the core and the DRM
//! devices the core opened. Mesa initializes on those (loading drivers, opening render nodes),
//! then `lockdown` applies the shared seccomp allowlist (`drv_os::seccomp`) plus the DRM,
//! dma-buf and sync-file ioctls, and only then does the process answer the core. No open, no
//! socket, no exec. Files it needs later (cursor theme icons, the PipeWire socket) arrive as
//! fds on the requests that use them.

use anyhow::Context as _;
use drv_os::seccomp::Allowlist;

/// Environment variable the core forwards; `0` runs the GPU process unconfined (debugging).
pub const DISABLE_ENV: &str = "NIRI_GPU_SANDBOX";

pub fn enabled() -> bool {
    std::env::var_os(DISABLE_ENV).is_none_or(|v| v != "0")
}

/// Seccomp: after this, the process can only talk to the fds it already holds.
pub fn lockdown() -> anyhow::Result<()> {
    let mut allow = Allowlist::base().context("seccomp baseline")?;
    // 'd' is DRM, 'b' dma-buf, '>' sync_file.
    for ty in *b"db>" {
        allow.ioctl_type(ty).context("seccomp ioctl rule")?;
    }
    allow.apply("niri gpu-process").context("applying seccomp filter")?;
    debug!("seccomp: syscall allowlist applied");
    Ok(())
}
