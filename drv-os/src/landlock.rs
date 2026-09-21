//! Landlock, raw: what the forker's child puts itself under before it becomes the app. Rules are paths with access bits;
//! a rule on a directory covers everything beneath it, bind mounts included. Nothing here
//! is permissive: a kernel without Landlock is an error, not a warning.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

pub const EXECUTE: u64 = 1 << 0;
pub const WRITE_FILE: u64 = 1 << 1;
pub const READ_FILE: u64 = 1 << 2;
pub const READ_DIR: u64 = 1 << 3;
pub const REMOVE_DIR: u64 = 1 << 4;
pub const REMOVE_FILE: u64 = 1 << 5;
pub const MAKE_CHAR: u64 = 1 << 6;
pub const MAKE_DIR: u64 = 1 << 7;
pub const MAKE_REG: u64 = 1 << 8;
pub const MAKE_SOCK: u64 = 1 << 9;
pub const MAKE_FIFO: u64 = 1 << 10;
pub const MAKE_BLOCK: u64 = 1 << 11;
pub const MAKE_SYM: u64 = 1 << 12;
pub const REFER: u64 = 1 << 13;
pub const TRUNCATE: u64 = 1 << 14;
pub const IOCTL_DEV: u64 = 1 << 15;

/// Read only: files, listings.
pub const READ: u64 = READ_FILE | READ_DIR;
/// What a file (not a directory) can be given.
const FILE_BITS: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;

const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

const RULE_PATH_BENEATH: u32 = 1;
const CREATE_RULESET_VERSION: u32 = 1 << 0;

const SYS_CREATE_RULESET: libc::c_long = 444;
const SYS_ADD_RULE: libc::c_long = 445;
const SYS_RESTRICT_SELF: libc::c_long = 446;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneath {
    allowed_access: u64,
    parent_fd: i32,
}

pub struct Ruleset {
    fd: OwnedFd,
    /// Every filesystem access the kernel knows; anything not allowed by a rule is denied.
    handled: u64,
}

impl Ruleset {
    /// A ruleset handling every filesystem access this kernel has, with abstract sockets and
    /// signals scoped to the domain where the kernel can (ABI 6).
    pub fn new() -> io::Result<Self> {
        // SAFETY: the version query takes no attr.
        let abi = unsafe { libc::syscall(SYS_CREATE_RULESET, std::ptr::null::<RulesetAttr>(), 0usize, CREATE_RULESET_VERSION) };
        if abi < 0 {
            return Err(io::Error::new(io::Error::last_os_error().kind(), "no Landlock in this kernel"));
        }
        let mut handled = (1 << 13) - 1; // ABI 1: up to MAKE_SYM
        if abi >= 2 {
            handled |= REFER;
        }
        if abi >= 3 {
            handled |= TRUNCATE;
        }
        if abi >= 5 {
            handled |= IOCTL_DEV;
        }
        let attr = RulesetAttr {
            handled_access_fs: handled,
            handled_access_net: 0,
            scoped: if abi >= 6 { SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL } else { 0 },
        };
        // Older kernels want the shorter struct.
        let size = if abi >= 6 { 24 } else if abi >= 4 { 16 } else { 8 };
        // SAFETY: attr outlives the call; size is what the kernel expects for this ABI.
        let fd = unsafe { libc::syscall(SYS_CREATE_RULESET, &attr as *const RulesetAttr, size as usize, 0u32) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a fresh fd we own.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as i32) };
        Ok(Self { fd, handled })
    }

    /// Everything the kernel handles: read, write, create, remove, rename.
    pub fn all(&self) -> u64 {
        self.handled
    }

    /// `access` at `path` and beneath. A file gets only what a file can have. A path that is
    /// not there is skipped: a closure entry not built on this machine is not a hole.
    pub fn allow(&self, path: &Path, access: u64) -> io::Result<bool> {
        struct Cwd;
        impl AsRawFd for Cwd {
            fn as_raw_fd(&self) -> i32 {
                libc::AT_FDCWD
            }
        }
        self.allow_at(&Cwd, path, access)
    }

    /// The same, `path` relative to `dir` (a detached tree's fd, say): the rule is on the
    /// inode, where the tree ends up mounted does not matter.
    pub fn allow_at(&self, dir: &impl AsRawFd, path: &Path, access: u64) -> io::Result<bool> {
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("NUL in {}", path.display())))?;
        // O_PATH: the inode, not an open file; std's OpenOptions wants read or write.
        // SAFETY: valid C string and fd.
        let fd = unsafe { libc::openat(dir.as_raw_fd(), c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
        if fd < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::NotFound {
                return Ok(false);
            }
            return Err(io::Error::new(e.kind(), format!("{}: {e}", path.display())));
        }
        // SAFETY: a fresh fd we own.
        let file = unsafe { File::from_raw_fd(fd) };
        let is_dir = file.metadata()?.is_dir();
        let mut allowed = access & self.handled;
        if !is_dir {
            allowed &= FILE_BITS;
        }
        if allowed == 0 {
            return Ok(true);
        }
        let rule = PathBeneath { allowed_access: allowed, parent_fd: file.as_raw_fd() };
        // SAFETY: rule and the fd outlive the call.
        let rc = unsafe { libc::syscall(SYS_ADD_RULE, self.fd.as_raw_fd(), RULE_PATH_BENEATH, &rule as *const PathBeneath, 0u32) };
        drop::<File>(file);
        if rc != 0 {
            return Err(io::Error::new(io::Error::last_os_error().kind(), format!("rule for {}: {}", path.display(), io::Error::last_os_error())));
        }
        Ok(true)
    }

    /// From here on, this process and its descendants. Needs no_new_privs.
    pub fn restrict_self(&self) -> io::Result<()> {
        // SAFETY: plain syscall on our fd.
        if unsafe { libc::syscall(SYS_RESTRICT_SELF, self.fd.as_raw_fd(), 0u32) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
