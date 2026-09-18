//! Confinement of the compositor core.
//!
//! By the time the event loop starts, the core holds everything it was given: the links to
//! drv-seatd, drv-appd, drv-forker, drv-portal and the GPU process (all from the supervisor)
//! and its Wayland and IPC listeners. `lockdown` then applies the shared seccomp allowlist
//! (`drv_os::seccomp`). What stays open after that: accepting clients, reading files (config,
//! keymaps, cursor themes), anonymous files (smithay's keymap copy for clients on an old
//! wl_keyboard), connecting to Unix sockets (PipeWire, once per cast), DRM, dma-buf and evdev
//! ioctls on fds drv-seatd handed over, and unlinking (the Wayland socket and its lock file
//! go away when the listener drops at exit; the runtime dir is the only place the core can
//! write). No new processes: when the GPU process dies the core exits and the supervisor
//! restarts the set. Nothing gets created on disk, so a screenshot to a path fails (the
//! clipboard copy works).

use anyhow::Context as _;
use drv_os::seccomp::Allowlist;

pub fn lockdown() -> anyhow::Result<()> {
    if !drv_os::seccomp::enabled() {
        warn!("seccomp disabled ({}=0)", drv_os::seccomp::DISABLE_ENV);
        return Ok(());
    }
    let mut allow = Allowlist::base().context("seccomp baseline")?;
    allow.read_files().context("seccomp file rules")?;
    allow.anonymous_files().context("seccomp file rules")?;
    allow.connect_unix().context("seccomp socket rules")?;
    allow.accept();
    allow.allow(&[libc::SYS_unlinkat]);
    #[cfg(target_arch = "x86_64")]
    allow.allow(&[libc::SYS_unlink]);
    // 'd' is DRM, 'b' dma-buf, '>' sync_file, 'E' evdev (libinput).
    for ty in *b"db>E" {
        allow.ioctl_type(ty).context("seccomp ioctl rule")?;
    }
    allow.apply("niri").context("applying seccomp filter")?;
    info!("seccomp: syscall allowlist applied");
    Ok(())
}
