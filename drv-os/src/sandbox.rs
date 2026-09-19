//! The private view of the filesystem every process we start gets, apps and the trusted set
//! alike: one primitive, planned before fork and applied between fork and exec.

use std::ffi::{CStr, CString};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// A process's private view of the filesystem, decided before fork and applied between fork
/// and exec (only syscalls on pre-built strings; nothing allocates there). Own UID plus this
/// is the floor everyone gets; what it may reach on top is groups, fds and sockets.
///
/// - a new mount namespace, so none of it leaks out, and unless the app was granted the network a
///   new network namespace with nothing in it;
/// - `/tmp` and `/dev/shm` are fresh tmpfs: no shared scratch space between apps. An app
///   may get its own `/tmp` instead, kept between its launches (`tmp`), so a second launch
///   finds the first (a browser's single-instance socket lives there);
/// - `/proc` shows only the app's own processes;
/// - `/run` is a fresh, read-only tmpfs holding only the exposed entries: no system D-Bus, no
///   identity socket unless exposed, no other app's runtime directory, no setuid wrappers.
pub struct Sandbox {
    /// `unshare(CLONE_NEWNET)` too: no interfaces at all.
    no_network: bool,
    /// Directories to create in the staging tmpfs, parents first.
    dirs: Vec<CString>,
    /// `(source, target)` bind mounts into the staging tmpfs.
    binds: Vec<(CString, CString)>,
    /// `(link target, link path)` symlinks recreated in the staging tmpfs.
    symlinks: Vec<(CString, CString)>,
    /// Bound over `/tmp` at the end, if the process gets a `/tmp` that outlives it.
    tmp: Option<CString>,
}

/// Under `/dev/shm`, always a fresh tmpfs of ours; `/tmp` may be the process's own.
const STAGE: &str = "/dev/shm/.run";

impl Sandbox {
    /// `expose`: entries under `/run` to keep (directories bind-mounted, symlinks recreated).
    /// `tmp`: a directory of the process's own to be its `/tmp`; `None` for a fresh tmpfs.
    pub fn plan(expose: &[PathBuf], network: bool, tmp: Option<&Path>) -> Result<Self, String> {
        let cstr = |p: &Path| {
            CString::new(p.as_os_str().as_bytes()).map_err(|_| format!("NUL in {}", p.display()))
        };
        let mut dirs = Vec::new();
        let mut binds = Vec::new();
        let mut symlinks = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for path in expose {
            let rel = path
                .strip_prefix("/run")
                .map_err(|_| format!("expose {}: not under /run", path.display()))?;
            if rel.as_os_str().is_empty() {
                return Err("expose /run: exposing everything defeats the sandbox".to_owned());
            }
            let staged = Path::new(STAGE).join(rel);
            // Parents inside the stage, outermost first.
            let mut parents: Vec<_> = staged.ancestors().skip(1).collect();
            parents.reverse();
            for parent in parents {
                if parent.starts_with(STAGE) && seen.insert(parent.to_owned()) {
                    dirs.push(cstr(parent)?);
                }
            }
            let meta = std::fs::symlink_metadata(path)
                .map_err(|e| format!("expose {}: {e}", path.display()))?;
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(path)
                    .map_err(|e| format!("readlink {}: {e}", path.display()))?;
                symlinks.push((cstr(&target)?, cstr(&staged)?));
            } else if meta.is_dir() {
                if seen.insert(staged.clone()) {
                    dirs.push(cstr(&staged)?);
                }
                binds.push((cstr(path)?, cstr(&staged)?));
            } else {
                return Err(format!(
                    "expose {}: only directories and symlinks",
                    path.display()
                ));
            }
        }
        Ok(Self {
            no_network: !network,
            dirs,
            binds,
            symlinks,
            tmp: tmp.map(cstr).transpose()?,
        })
    }

    /// Runs in the child with CAP_SYS_ADMIN still in hand. On failure the step's name goes to
    /// stderr (the forker's journal) and the errno comes back to the parent.
    pub fn apply(&self) -> io::Result<()> {
        // Written by hand rather than through a helper closure so every string is a literal.
        fn fail(step: &'static str) -> io::Result<()> {
            let err = io::Error::last_os_error();
            let msg = b"sandbox: ";
            // SAFETY: plain write(2) of static bytes.
            unsafe {
                libc::write(2, msg.as_ptr().cast(), msg.len());
                libc::write(2, step.as_ptr().cast(), step.len());
                libc::write(2, b"\n".as_ptr().cast(), 1);
            }
            Err(err)
        }
        let root = c"/";
        let tmpfs = c"tmpfs";
        let mode1777 = c"mode=1777";
        let mode0755 = c"mode=0755";
        let proc_ = c"proc";
        let hidepid = c"hidepid=invisible";
        let tmp = c"/tmp";
        let shm = c"/dev/shm";
        let procdir = c"/proc";
        let stage = c"/dev/shm/.run";
        let run = c"/run";
        let none: *const libc::c_char = std::ptr::null();
        let mnt = |src: &CStr,
                   dst: &CStr,
                   fstype: *const libc::c_char,
                   flags: libc::c_ulong,
                   data: *const libc::c_char| {
            // SAFETY: all pointers are valid C strings (or null where the kernel allows it).
            unsafe { libc::mount(src.as_ptr(), dst.as_ptr(), fstype, flags, data.cast()) }
        };
        let nodev = libc::MS_NOSUID | libc::MS_NODEV;
        let flags = libc::CLONE_NEWNS
            | if self.no_network {
                libc::CLONE_NEWNET
            } else {
                0
            };
        // SAFETY: syscalls only.
        unsafe {
            if libc::unshare(flags) != 0 {
                return fail("unshare(CLONE_NEWNS | CLONE_NEWNET)");
            }
        }
        if mnt(c"none", root, none, libc::MS_REC | libc::MS_PRIVATE, none) != 0 {
            return fail("make / private");
        }
        // Its own /tmp, or a fresh one. Now, while the bind's source is still in view: the
        // stage replaces /run below.
        match &self.tmp {
            Some(own) => {
                if mnt(own, tmp, none, libc::MS_BIND, none) != 0 {
                    return fail("bind own /tmp");
                }
                if mnt(c"none", tmp, none, libc::MS_REMOUNT | libc::MS_BIND | nodev, none) != 0 {
                    return fail("remount own /tmp nosuid");
                }
            }
            None => {
                if mnt(tmpfs, tmp, tmpfs.as_ptr(), nodev, mode1777.as_ptr()) != 0 {
                    return fail("tmpfs on /tmp");
                }
            }
        }
        if mnt(tmpfs, shm, tmpfs.as_ptr(), nodev, mode1777.as_ptr()) != 0 {
            return fail("tmpfs on /dev/shm");
        }
        if mnt(
            proc_,
            procdir,
            proc_.as_ptr(),
            nodev | libc::MS_NOEXEC,
            hidepid.as_ptr(),
        ) != 0
        {
            return fail("proc with hidepid");
        }
        // SAFETY: syscalls on static strings.
        unsafe {
            if libc::mkdir(stage.as_ptr(), 0o755) != 0 {
                return fail("mkdir stage");
            }
        }
        if mnt(tmpfs, stage, tmpfs.as_ptr(), nodev, mode0755.as_ptr()) != 0 {
            return fail("tmpfs on stage");
        }
        for dir in &self.dirs {
            // SAFETY: valid C string; EEXIST is fine (shared parents).
            let rc = unsafe { libc::mkdir(dir.as_ptr(), 0o755) };
            if rc != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return fail("mkdir in stage");
            }
        }
        for (src, dst) in &self.binds {
            if mnt(src, dst, none, libc::MS_BIND | libc::MS_REC, none) != 0 {
                return fail("bind mount into stage");
            }
        }
        for (target, link) in &self.symlinks {
            // SAFETY: valid C strings.
            if unsafe { libc::symlink(target.as_ptr(), link.as_ptr()) } != 0 {
                return fail("symlink in stage");
            }
        }
        if mnt(stage, run, none, libc::MS_MOVE, none) != 0 {
            return fail("move stage to /run");
        }
        // SAFETY: syscall on a static string; the stage directory is empty after the move.
        unsafe {
            libc::rmdir(stage.as_ptr());
        }
        // The tmpfs itself read-only; the bind mounts inside keep their own flags.
        if mnt(
            c"none",
            run,
            none,
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | nodev,
            none,
        ) != 0
        {
            return fail("remount /run read-only");
        }
        Ok(())
    }
}
