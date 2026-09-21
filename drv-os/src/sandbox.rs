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


/// An app's root, assembled from the four sources of DESIGN-app-namespace: the store, the
/// host's generated views, the app's own state and its runtime entries. Planned before fork
/// (paths checked, every string built), applied between fork and exec with CAP_SYS_ADMIN
/// (syscalls on the prebuilt strings only), ending in `pivot_root`: the host root is gone.
pub struct Root {
    no_network: bool,
    steps: Vec<Step>,
}

/// What the plan turns into: one mount or directory at a time, paths already under the stage.
enum Step {
    Dir(CString),
    /// An empty file, for a file bound over it.
    File(CString),
    /// `src` bound at `dst`, then remounted with `flags` (read-only, nosuid, ...).
    Bind { src: CString, dst: CString, flags: libc::c_ulong },
    Symlink { target: CString, link: CString },
    Tmpfs { dst: CString, data: CString },
    Proc(CString),
}

/// What the forker knows about the app; everything else is fixed here.
pub struct RootSpec<'a> {
    /// `/nix/store`.
    pub store: &'a Path,
    /// The app's `/etc`, a store path.
    pub etc: Option<&'a Path>,
    /// The host's resolv.conf (its final target), bound over the app's when it has the network.
    pub resolv: Option<&'a Path>,
    /// `/run/drv-host`: `dev`, `dev-gpu`, `sys`, `sys-gpu`, written by the host at boot.
    pub views: &'a Path,
    pub gpu: bool,
    pub network: bool,
    /// Entries under `/run` to keep, the app's own runtime directory among them.
    pub run_expose: &'a [PathBuf],
    /// The app's `/tmp`, kept for the boot.
    pub tmp: &'a Path,
    /// The app's home, bound at its own path.
    pub home: &'a Path,
}

const NEW: &str = "/dev/shm/.root";

/// Collects the steps of a `Root`: directories once, mountpoints created before their binds.
struct Plan {
    steps: Vec<Step>,
    seen: std::collections::HashSet<PathBuf>,
}

fn cstr(p: &Path) -> Result<CString, String> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| format!("NUL in {}", p.display()))
}

fn staged(p: &Path) -> PathBuf {
    Path::new(NEW).join(p.strip_prefix("/").unwrap_or(p))
}

impl Plan {
    fn dir(&mut self, p: &Path) -> Result<(), String> {
        let mut parents: Vec<_> = p.ancestors().collect();
        parents.reverse();
        for parent in parents {
            if parent.starts_with(NEW) && parent != Path::new(NEW) && self.seen.insert(parent.to_owned()) {
                self.steps.push(Step::Dir(cstr(parent)?));
            }
        }
        Ok(())
    }

    fn bind(&mut self, src: &Path, dst: &Path, flags: libc::c_ulong) -> Result<(), String> {
        let meta = std::fs::symlink_metadata(src).map_err(|e| format!("{}: {e}", src.display()))?;
        if meta.is_dir() {
            self.dir(dst)?;
        } else {
            self.dir(dst.parent().unwrap())?;
            self.steps.push(Step::File(cstr(dst)?));
        }
        self.steps.push(Step::Bind { src: cstr(src)?, dst: cstr(dst)?, flags });
        Ok(())
    }
}

impl Root {
    pub fn plan(spec: &RootSpec<'_>) -> Result<Self, String> {
        let ro = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV;
        let ro_noexec = ro | libc::MS_NOEXEC;
        let rw_noexec = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;
        // Device nodes live here, so no MS_NODEV.
        let dev = libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NOEXEC;
        let mut plan = Plan { steps: Vec::new(), seen: Default::default() };
        // 1. The store.
        plan.bind(spec.store, &staged(spec.store), ro)?;
        // 2. Host views: /etc from the store, /dev and /sys from the boot-time generator.
        if let Some(etc) = spec.etc {
            let etc = etc.canonicalize().map_err(|e| format!("etc {}: {e}", etc.display()))?;
            if !etc.starts_with(spec.store) {
                return Err(format!("etc {}: not in the store", etc.display()));
            }
            plan.bind(&etc, &staged(Path::new("/etc")), ro_noexec)?;
            if spec.network && etc.join("resolv.conf").exists() {
                if let Some(resolv) = spec.resolv {
                    plan.bind(resolv, &staged(Path::new("/etc/resolv.conf")), ro_noexec)?;
                }
            }
        }
        let view = |name: &str| -> Result<PathBuf, String> {
            let p = spec.views.join(name);
            if !p.is_dir() {
                return Err(format!("host view {} is missing (drv-host-views not run?)", p.display()));
            }
            Ok(p)
        };
        plan.bind(&view("dev")?, &staged(Path::new("/dev")), dev)?;
        if spec.gpu {
            plan.bind(&view("dev-gpu")?.join("dri"), &staged(Path::new("/dev/dri")), dev)?;
        }
        let shm = staged(Path::new("/dev/shm"));
        plan.dir(&shm)?;
        plan.steps.push(Step::Tmpfs { dst: cstr(&shm)?, data: c"mode=1777".into() });
        plan.bind(&view(if spec.gpu { "sys-gpu" } else { "sys" })?, &staged(Path::new("/sys")), ro_noexec)?;
        // The kernel's view of the app's own processes.
        let proc_ = staged(Path::new("/proc"));
        plan.dir(&proc_)?;
        plan.steps.push(Step::Proc(cstr(&proc_)?));
        // 3. State: the home, at its own path, nothing else under /var.
        plan.bind(spec.home, &staged(spec.home), rw_noexec)?;
        // 4. Runtime: /tmp, and /run holding only the exposed entries.
        plan.bind(spec.tmp, &staged(Path::new("/tmp")), rw_noexec)?;
        let run = staged(Path::new("/run"));
        plan.dir(&run)?;
        for path in spec.run_expose {
            let rel = path
                .strip_prefix("/run")
                .map_err(|_| format!("expose {}: not under /run", path.display()))?;
            if rel.as_os_str().is_empty() {
                return Err("expose /run: exposing everything defeats the sandbox".to_owned());
            }
            let meta = std::fs::symlink_metadata(path)
                .map_err(|e| format!("expose {}: {e}", path.display()))?;
            let dst = run.join(rel);
            if meta.file_type().is_symlink() {
                let target = std::fs::read_link(path)
                    .map_err(|e| format!("readlink {}: {e}", path.display()))?;
                plan.dir(dst.parent().unwrap())?;
                plan.steps.push(Step::Symlink { target: cstr(&target)?, link: cstr(&dst)? });
            } else if meta.is_dir() {
                plan.bind(path, &dst, rw_noexec)?;
            } else {
                return Err(format!("expose {}: only directories and symlinks", path.display()));
            }
        }
        // Where the old root goes during the pivot.
        plan.dir(&staged(Path::new("/.old")))?;
        Ok(Self { no_network: !spec.network, steps: plan.steps })
    }

    /// Runs in the child with CAP_SYS_ADMIN. Every string is prebuilt; nothing allocates.
    pub fn apply(&self) -> io::Result<()> {
        fn fail(step: &'static str) -> io::Result<()> {
            let err = io::Error::last_os_error();
            let msg = b"root: ";
            // SAFETY: plain write(2) of static bytes.
            unsafe {
                libc::write(2, msg.as_ptr().cast(), msg.len());
                libc::write(2, step.as_ptr().cast(), step.len());
                libc::write(2, b"\n".as_ptr().cast(), 1);
            }
            Err(err)
        }
        let none: *const libc::c_char = std::ptr::null();
        let mnt = |src: *const libc::c_char, dst: &CStr, fstype: *const libc::c_char, flags: libc::c_ulong, data: *const libc::c_char| {
            // SAFETY: valid C strings or null where the kernel allows it.
            unsafe { libc::mount(src, dst.as_ptr(), fstype, flags, data.cast()) }
        };
        let tmpfs = c"tmpfs";
        let nodev = libc::MS_NOSUID | libc::MS_NODEV;
        let flags = libc::CLONE_NEWNS | if self.no_network { libc::CLONE_NEWNET } else { 0 };
        // SAFETY: syscalls only.
        unsafe {
            if libc::unshare(flags) != 0 {
                return fail("unshare");
            }
        }
        if mnt(c"none".as_ptr(), c"/", none, libc::MS_REC | libc::MS_PRIVATE, none) != 0 {
            return fail("make / private");
        }
        // A tmpfs of ours to stage in (over the host's /dev/shm, which we leave behind).
        if mnt(tmpfs.as_ptr(), c"/dev/shm", tmpfs.as_ptr(), nodev, c"mode=0755".as_ptr()) != 0 {
            return fail("tmpfs on /dev/shm");
        }
        let new = c"/dev/shm/.root";
        // SAFETY: static string.
        if unsafe { libc::mkdir(new.as_ptr(), 0o755) } != 0 {
            return fail("mkdir the new root");
        }
        if mnt(tmpfs.as_ptr(), new, tmpfs.as_ptr(), nodev, c"mode=0755".as_ptr()) != 0 {
            return fail("tmpfs for the new root");
        }
        for step in &self.steps {
            match step {
                Step::Dir(p) => {
                    // SAFETY: valid C string; EEXIST is fine (shared parents).
                    let rc = unsafe { libc::mkdir(p.as_ptr(), 0o755) };
                    if rc != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                        return fail("mkdir");
                    }
                }
                Step::File(p) => {
                    // The mountpoint may be there already (a file of a read-only view).
                    // SAFETY: valid C string, F_OK needs no buffer.
                    if unsafe { libc::access(p.as_ptr(), libc::F_OK) } == 0 {
                        continue;
                    }
                    // SAFETY: valid C string.
                    let fd = unsafe { libc::open(p.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_CLOEXEC, 0o644) };
                    if fd < 0 {
                        return fail("create file mountpoint");
                    }
                    // SAFETY: our fd.
                    unsafe { libc::close(fd) };
                }
                Step::Symlink { target, link } => {
                    // SAFETY: valid C strings.
                    if unsafe { libc::symlink(target.as_ptr(), link.as_ptr()) } != 0 {
                        return fail("symlink");
                    }
                }
                Step::Bind { src, dst, flags } => {
                    if mnt(src.as_ptr(), dst, none, libc::MS_BIND | libc::MS_REC, none) != 0 {
                        return fail("bind");
                    }
                    if mnt(none, dst, none, libc::MS_REMOUNT | libc::MS_BIND | *flags, none) != 0 {
                        return fail("remount bind");
                    }
                }
                Step::Tmpfs { dst, data } => {
                    if mnt(tmpfs.as_ptr(), dst, tmpfs.as_ptr(), nodev, data.as_ptr()) != 0 {
                        return fail("tmpfs");
                    }
                }
                Step::Proc(dst) => {
                    if mnt(c"proc".as_ptr(), dst, c"proc".as_ptr(), nodev | libc::MS_NOEXEC, c"hidepid=invisible".as_ptr()) != 0 {
                        return fail("proc");
                    }
                }
            }
        }
        // SAFETY: syscalls on static strings.
        unsafe {
            if libc::chdir(new.as_ptr()) != 0 {
                return fail("chdir to the new root");
            }
            if libc::syscall(libc::SYS_pivot_root, c".".as_ptr(), c".old".as_ptr()) != 0 {
                return fail("pivot_root");
            }
            if libc::chdir(c"/".as_ptr()) != 0 {
                return fail("chdir /");
            }
            if libc::umount2(c"/.old".as_ptr(), libc::MNT_DETACH) != 0 {
                return fail("detach the old root");
            }
            if libc::rmdir(c"/.old".as_ptr()) != 0 {
                return fail("rmdir /.old");
            }
        }
        // The root itself read-only: no new top-level entries.
        if mnt(none, c"/", none, libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | nodev, none) != 0 {
            return fail("remount / read-only");
        }
        Ok(())
    }
}
