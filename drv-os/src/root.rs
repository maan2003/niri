//! A process's root, built by the process itself after the fork, from handles: a fresh
//! tmpfs pivoted in as `/`, detached clones of the host's trees and new filesystem instances
//! attached at paths inside it, the host's root stacked beneath where no path lookup reaches
//! it until `finish` detaches it. The same few calls for the trusted set (the supervisor's
//! child) and for apps (the forker's child); what differs is the list of what goes where.

use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;

use rustix::fs::{Mode, OFlags};

use crate::mounts::{attach, new_fs, set_attrs, Attr};

/// A mount namespace of our own (and a network namespace with nothing in it, if asked), the
/// old root made private. Handles into the old root are taken after this, not before: one
/// from before the unshare points into the parent's namespace, which nothing may be cloned
/// from.
///
/// # Safety
/// The caller must be single-threaded (`unshare(CLONE_NEWNS)` is).
pub unsafe fn unshare(new_net: bool) -> Result<(), String> {
    use rustix::mount::MountPropagationFlags as P;
    use rustix::thread::UnshareFlags as U;
    let flags = if new_net {
        U::NEWNS | U::NEWNET
    } else {
        U::NEWNS
    };
    // SAFETY: the caller's.
    unsafe { rustix::thread::unshare_unsafe(flags) }.map_err(|e| format!("unshare: {e}"))?;
    rustix::mount::mount_change("/", P::PRIVATE | P::REC).map_err(|e| format!("make private: {e}"))
}

/// A fresh tmpfs, made our root: hung on `base` (a directory of the old root) for the moment
/// it takes. From here every path resolves inside the new root; the old one is stacked
/// beneath, reachable through handles taken earlier and nothing else.
pub fn pivot(base: &Path) -> Result<(), String> {
    let root = new_fs(
        "tmpfs",
        &[("mode", "0755")],
        Attr::MOUNT_ATTR_NOSUID | Attr::MOUNT_ATTR_NODEV,
    )
    .map_err(|e| format!("root tmpfs: {e}"))?;
    attach(root, base).map_err(|e| format!("attach root at {}: {e}", base.display()))?;
    rustix::process::chdir(base).map_err(|e| format!("chdir {}: {e}", base.display()))?;
    rustix::process::pivot_root(".", ".").map_err(|e| format!("pivot_root: {e}"))?;
    rustix::process::chdir("/").map_err(|e| format!("chdir /: {e}"))
}

/// `what` (a detached mount) at `at`, made as a directory first.
pub fn mount(what: OwnedFd, at: &Path) -> Result<(), String> {
    std::fs::create_dir_all(at).map_err(|e| format!("mkdir {}: {e}", at.display()))?;
    attach(what, at).map_err(|e| format!("mount {}: {e}", at.display()))
}

/// A symlink at `at` to `target` (a `/run` entry that is a link into the store, say).
pub fn symlink(target: &Path, at: &Path) -> Result<(), String> {
    if let Some(parent) = at.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::os::unix::fs::symlink(target, at).map_err(|e| format!("symlink {}: {e}", at.display()))
}

/// The old root detached, and the top level of the new one given `attrs` (read-only, noexec:
/// nothing new at the top level, ever, and nothing runs from it).
pub fn finish(attrs: Attr) -> Result<(), String> {
    rustix::mount::unmount(".", rustix::mount::UnmountFlags::DETACH)
        .map_err(|e| format!("detach old root: {e}"))?;
    let root = rustix::fs::open("/", OFlags::PATH | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| format!("open /: {e}"))?;
    set_attrs(&root.as_fd(), attrs, false).map_err(|e| format!("root attrs: {e}"))
}
