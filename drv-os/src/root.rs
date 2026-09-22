//! A process's root, built from handles: a fresh tmpfs pivoted in as `/`, detached clones of
//! the host's trees and new filesystem instances attached at paths inside it, the host's
//! root stacked beneath where no path lookup reaches it until `finish` detaches it. One
//! primitive for the trusted set and for apps: the supervisor plans a member's root before
//! the fork and applies it in the child (syscalls on pre-built strings, nothing allocates
//! there); the forker applies the fixed part, adds what the request decides with `mount`,
//! then finishes.

use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use rustix::fs::{Mode, OFlags, CWD};
use rustix::mount::{move_mount, MoveMountFlags};

use crate::mounts::{new_fs, set_attrs, Attr};

/// The plan: what goes where, in order.
pub struct Root {
    base: CString,
    root: OwnedFd,
    new_net: bool,
    steps: Vec<Step>,
    dirs: std::collections::HashSet<CString>,
}

enum Step {
    /// A directory to make (parents come first; one already there is fine).
    Dir(CString),
    /// A detached mount and where it goes.
    Mount(OwnedFd, CString),
    /// A symlink: target, path.
    Link(CString, CString),
}

fn cstr(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_bytes()).map_err(|_| format!("NUL in {}", path.display()))
}

impl Root {
    /// `base`: a directory of the current root the new one hangs on for the moment of the
    /// pivot. `new_net`: a network namespace of its own too, with nothing in it.
    pub fn new(base: &Path, new_net: bool) -> Result<Self, String> {
        let root = new_fs(
            "tmpfs",
            &[("mode", "0755")],
            Attr::MOUNT_ATTR_NOSUID | Attr::MOUNT_ATTR_NODEV,
        )
        .map_err(|e| format!("root tmpfs: {e}"))?;
        Ok(Self {
            base: cstr(base)?,
            root,
            new_net,
            steps: Vec::new(),
            dirs: Default::default(),
        })
    }

    fn dir(&mut self, path: &Path) -> Result<(), String> {
        let mut parents: Vec<&Path> = path.ancestors().filter(|p| *p != Path::new("/")).collect();
        parents.reverse();
        for p in parents {
            let c = cstr(p)?;
            if self.dirs.insert(c.clone()) {
                self.steps.push(Step::Dir(c));
            }
        }
        Ok(())
    }

    /// `what` (a detached mount) at `at`, made as a directory first.
    pub fn mount(&mut self, what: OwnedFd, at: &Path) -> Result<(), String> {
        self.dir(at)?;
        self.steps.push(Step::Mount(what, cstr(at)?));
        Ok(())
    }

    /// A symlink at `at` to `target` (a `/run` entry that is a link into the store, say).
    pub fn symlink(&mut self, target: &Path, at: &Path) -> Result<(), String> {
        if let Some(parent) = at.parent() {
            self.dir(parent)?;
        }
        self.steps.push(Step::Link(cstr(target)?, cstr(at)?));
        Ok(())
    }

    /// A mount namespace of our own (and a network namespace, if asked), the old root made
    /// private. Separate from `build` so handles into the old root can be taken in between:
    /// one from before the unshare points into the parent's namespace, which nothing may be
    /// cloned from.
    ///
    /// # Safety
    /// The caller must be single-threaded (`unshare(CLONE_NEWNS)` is).
    pub unsafe fn unshare(&self) -> io::Result<()> {
        use rustix::thread::UnshareFlags as U;
        let flags = if self.new_net {
            U::NEWNS | U::NEWNET
        } else {
            U::NEWNS
        };
        // SAFETY: the caller's.
        unsafe { rustix::thread::unshare_unsafe(flags) }.map_err(|e| step("unshare", e))?;
        use rustix::mount::MountPropagationFlags as P;
        rustix::mount::mount_change(c"/", P::PRIVATE | P::REC).map_err(|e| step("make private", e))
    }

    /// The pivot and the steps. From then on every path resolves inside the new root; the
    /// old one is stacked beneath, reachable through handles taken earlier and nothing else.
    pub fn build(&self) -> io::Result<()> {
        attach(&self.root, &self.base).map_err(|e| step("attach root", e))?;
        rustix::process::chdir(self.base.as_c_str()).map_err(|e| step("chdir base", e))?;
        rustix::process::pivot_root(c".", c".").map_err(|e| step("pivot_root", e))?;
        rustix::process::chdir(c"/").map_err(|e| step("chdir /", e))?;
        for s in &self.steps {
            match s {
                Step::Dir(d) => match rustix::fs::mkdir(d.as_c_str(), Mode::from_raw_mode(0o755)) {
                    Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                    Err(e) => return Err(at("mkdir", d, e)),
                },
                Step::Mount(fd, p) => attach(fd, p).map_err(|e| at("mount", p, e))?,
                Step::Link(target, p) => rustix::fs::symlink(target.as_c_str(), p.as_c_str())
                    .map_err(|e| at("symlink", p, e))?,
            }
        }
        Ok(())
    }
}

/// `what` onto `path`, which must exist.
fn attach(what: impl AsFd, path: &CStr) -> rustix::io::Result<()> {
    move_mount(
        what,
        c"",
        CWD,
        path,
        MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
    )
}

/// After `build`: `what` (a detached mount) at `at`, made first. Allocates: for the forker's
/// per-request mounts, in a single-threaded child.
pub fn mount(what: OwnedFd, at: &Path) -> Result<(), String> {
    std::fs::create_dir_all(at).map_err(|e| format!("mkdir {}: {e}", at.display()))?;
    crate::mounts::attach(what, at).map_err(|e| format!("mount {}: {e}", at.display()))
}

/// The old root detached, and the top level of the new one given `attrs` (read-only, noexec:
/// nothing new at the top level, ever, and nothing runs from it).
pub fn finish(attrs: Attr) -> io::Result<()> {
    rustix::mount::unmount(c".", rustix::mount::UnmountFlags::DETACH)
        .map_err(|e| step("detach old root", e))?;
    let root = rustix::fs::open(c"/", OFlags::PATH | OFlags::CLOEXEC, Mode::empty())
        .map_err(|e| step("open /", e))?;
    set_attrs(&root, attrs, false).map_err(|e| step("root attrs", e))
}

/// The failing step named on stderr (the caller may be between fork and exec, where the
/// error travels as a bare errno), then the error.
fn step(what: &str, e: impl Into<io::Error>) -> io::Error {
    say(what, None);
    e.into()
}

fn at(what: &str, path: &CStr, e: rustix::io::Errno) -> io::Error {
    say(what, Some(path));
    e.into()
}

fn say(what: &str, path: Option<&CStr>) {
    let err = rustix::stdio::stderr();
    let _ = rustix::io::write(err, b"root: ");
    let _ = rustix::io::write(err, what.as_bytes());
    if let Some(p) = path {
        let _ = rustix::io::write(err, b" ");
        let _ = rustix::io::write(err, p.to_bytes());
    }
    let _ = rustix::io::write(err, b"\n");
}
