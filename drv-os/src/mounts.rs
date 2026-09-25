//! The new mount API (5.2+), the few calls an app root needs: a filesystem instance or a
//! detached clone of a host path is a file descriptor, with its flags set exactly, until it
//! is attached somewhere. `mount_setattr` is raw: rustix 1.1 has no wrapper yet.

use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::Path;

pub use rustix::mount::MountAttrFlags as Attr;
use rustix::mount::{
    fsconfig_create, fsconfig_set_string, fsmount, fsopen, move_mount, open_tree, FsMountFlags,
    FsOpenFlags, MountAttrFlags, MoveMountFlags, OpenTreeFlags,
};

const SYS_MOUNT_SETATTR: libc::c_long = 442;
const AT_RECURSIVE: libc::c_int = 0x8000;

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

/// A new filesystem instance: `fsopen`, options, `fsmount`.
pub fn new_fs(fs: &str, options: &[(&str, &str)], attrs: MountAttrFlags) -> io::Result<OwnedFd> {
    let fsfd = fsopen(fs, FsOpenFlags::FSOPEN_CLOEXEC)?;
    for (k, v) in options {
        fsconfig_set_string(&fsfd, *k, *v)?;
    }
    fsconfig_create(&fsfd)?;
    Ok(fsmount(&fsfd, FsMountFlags::FSMOUNT_CLOEXEC, attrs)?)
}

/// A detached copy of what is at `path` under `dir` (and beneath), with exactly `attrs` of
/// the four flags set on all of it: a clone keeps its source's flags (nodev from /run, say),
/// so the ones not asked for are cleared. Private, too: a clone of a shared mount is a peer
/// of its source, and a mount made inside it later (an app's `/dev/shm` inside its `/dev`
/// view) would appear on the host as well.
pub fn clone_tree(dir: impl AsFd, path: &Path, attrs: MountAttrFlags) -> io::Result<OwnedFd> {
    let fd = open_tree(
        dir,
        path,
        OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::AT_RECURSIVE,
    )?;
    set_attrs(&fd, attrs, true)?;
    Ok(fd)
}

/// Exactly `attrs` of the four flags, and private propagation, on `mount` (and beneath).
pub fn set_attrs(mount: &impl AsFd, attrs: MountAttrFlags, recursive: bool) -> io::Result<()> {
    use MountAttrFlags as A;
    let four =
        A::MOUNT_ATTR_RDONLY | A::MOUNT_ATTR_NOSUID | A::MOUNT_ATTR_NODEV | A::MOUNT_ATTR_NOEXEC;
    let attr = MountAttr {
        attr_set: attrs.bits() as u64,
        attr_clr: (four - attrs).bits() as u64,
        propagation: libc::MS_PRIVATE as u64,
        userns_fd: 0,
    };
    let flags = libc::AT_EMPTY_PATH | if recursive { AT_RECURSIVE } else { 0 };
    // SAFETY: attr outlives the call; size is the struct's.
    let rc = unsafe {
        libc::syscall(
            SYS_MOUNT_SETATTR,
            mount.as_fd().as_raw_fd(),
            c"".as_ptr(),
            flags,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `mount` (a detached clone, not yet attached) shown through `userns`'s mapping: a file the
/// filesystem says belongs to an id the namespace maps appears as the id it maps to, and
/// writes go the other way. The person's directory, owned by drv-files, becomes the app's
/// own inside its root and stays drv-files' on disk.
pub fn set_idmap(mount: &impl AsFd, userns: &impl AsFd) -> io::Result<()> {
    let attr = MountAttr {
        attr_set: MountAttrFlags::MOUNT_ATTR_IDMAP.bits() as u64,
        attr_clr: 0,
        propagation: 0,
        userns_fd: userns.as_fd().as_raw_fd() as u64,
    };
    // SAFETY: attr outlives the call; size is the struct's.
    let rc = unsafe {
        libc::syscall(
            SYS_MOUNT_SETATTR,
            mount.as_fd().as_raw_fd(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            &attr as *const MountAttr,
            std::mem::size_of::<MountAttr>(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `what` (a detached mount) onto `path`, which must exist.
pub fn attach(what: OwnedFd, path: &Path) -> io::Result<()> {
    Ok(move_mount(
        &what,
        "",
        rustix::fs::CWD,
        path,
        MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
    )?)
}
