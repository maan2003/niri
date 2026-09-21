//! An app's root (DESIGN-app-namespace), built before the fork as a detached mount tree and
//! a Landlock ruleset, both file descriptors. The forker holds the capabilities, so it is the
//! one that must never run complicated code after fork: here the complicated part (paths,
//! checks, allocation, errors) happens in the parent with ordinary Rust, and the child does
//! a dozen syscalls on the fds (`enter`, then `restrict`). The new mount API (5.2+) allows
//! attaching a detached tree into a namespace made later; Landlock rules are bound to
//! inodes, so rules added here hold after the pivot.

use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{chownat, mkdirat, symlinkat, AtFlags, Gid, Mode, OFlags, Uid};
use rustix::mount::{
    fsconfig_create, fsconfig_set_string, fsmount, fsopen, mount_change, move_mount, open_tree,
    unmount, FsMountFlags, FsOpenFlags, MountAttrFlags, MountPropagationFlags, MoveMountFlags,
    OpenTreeFlags, UnmountFlags,
};
use rustix::process::{chdir, pivot_root};
use rustix::thread::{unshare_unsafe, UnshareFlags};

use crate::landlock::{self, Ruleset};

/// Not in rustix 1.1 yet.
const SYS_MOUNT_SETATTR: libc::c_long = 442;
const AT_RECURSIVE: libc::c_int = 0x8000;

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

fn ctx<T>(r: io::Result<T>, what: impl FnOnce() -> String) -> io::Result<T> {
    r.map_err(|e| io::Error::new(e.kind(), format!("{}: {e}", what())))
}

/// A new filesystem instance: `fsopen`, options, `fsmount`.
fn new_fs(fs: &str, options: &[(&str, &str)], attrs: MountAttrFlags) -> io::Result<OwnedFd> {
    let fsfd = fsopen(fs, FsOpenFlags::FSOPEN_CLOEXEC)?;
    for (k, v) in options {
        fsconfig_set_string(&fsfd, *k, *v)?;
    }
    fsconfig_create(&fsfd)?;
    Ok(fsmount(&fsfd, FsMountFlags::FSMOUNT_CLOEXEC, attrs)?)
}

/// A detached copy of what is at `path` (and beneath), with `attrs` set on all of it.
fn clone_tree(path: &Path, attrs: MountAttrFlags) -> io::Result<OwnedFd> {
    let fd = open_tree(
        rustix::fs::CWD,
        path,
        OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::OPEN_TREE_CLOEXEC | OpenTreeFlags::AT_RECURSIVE,
    )?;
    set_attrs(&fd, attrs, true)?;
    Ok(fd)
}

/// Exactly `attrs` of the four: a clone keeps its source's flags (nodev from /run, say), so
/// the ones not asked for are cleared.
fn set_attrs(mount: &OwnedFd, attrs: MountAttrFlags, recursive: bool) -> io::Result<()> {
    use MountAttrFlags as A;
    let four = A::MOUNT_ATTR_RDONLY | A::MOUNT_ATTR_NOSUID | A::MOUNT_ATTR_NODEV | A::MOUNT_ATTR_NOEXEC;
    let attr = MountAttr {
        attr_set: attrs.bits() as u64,
        attr_clr: (four - attrs).bits() as u64,
        propagation: 0,
        userns_fd: 0,
    };
    let flags = libc::AT_EMPTY_PATH | if recursive { AT_RECURSIVE } else { 0 };
    // SAFETY: attr outlives the call; size is the struct's.
    let rc = unsafe {
        libc::syscall(SYS_MOUNT_SETATTR, mount.as_raw_fd(), c"".as_ptr(), flags, &attr as *const MountAttr, std::mem::size_of::<MountAttr>())
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `what` (a detached mount) onto `rel` inside the tree at `into`.
fn attach(what: OwnedFd, into: &OwnedFd, rel: &Path) -> io::Result<()> {
    Ok(move_mount(&what, "", into, rel, MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH)?)
}

/// Every component of `rel` under `dir`, made if missing (EEXIST is fine: a directory of a
/// read-only view already there).
fn mkdir_p(dir: &OwnedFd, rel: &Path) -> io::Result<()> {
    let mut so_far = PathBuf::new();
    for part in rel.components() {
        so_far.push(part);
        match mkdirat(dir, &so_far, Mode::from_raw_mode(0o755)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// An entry of the host's `/run` an app sees.
pub struct RunEntry<'a> {
    pub path: &'a Path,
    pub writable: bool,
}

/// What the forker knows about the app; everything else is fixed here.
pub struct Spec<'a> {
    /// `/nix/store`.
    pub store: &'a Path,
    /// The host's resolv.conf, at `/run/host/resolv.conf` inside when the app has the network.
    pub resolv: &'a Path,
    /// `/run/drv-host`: `dev`, `dev-gpu`, `sys`, `sys-gpu`, written by the host at boot.
    pub views: &'a Path,
    pub gpu: bool,
    pub network: bool,
    /// Entries under `/run` to keep (the app's own runtime directory among them).
    pub run: &'a [RunEntry<'a>],
    /// The app's `/tmp`, kept for the boot.
    pub tmp: &'a Path,
    /// HOME: `/home/<name>`, a fresh size-capped tmpfs owned by the UID.
    pub home: &'a Path,
    /// What the app keeps between runs (`/var/lib/drv-apps/<uid>`), bound at `<home>/.state`.
    pub state: &'a Path,
    pub uid: libc::uid_t,
    pub gid: libc::gid_t,
    /// The store paths the app may open. Already checked to be store paths.
    pub closure: &'a [String],
    /// A JIT inside: no MDWE.
    pub jit: bool,
}

/// The finished root and ruleset, waiting for a child to enter them.
pub struct AppRoot {
    tree: OwnedFd,
    ruleset: Ruleset,
    no_network: bool,
    jit: bool,
    /// Closure paths listed but not present on this machine.
    pub missing: usize,
}

impl AppRoot {
    pub fn prepare(spec: &Spec<'_>) -> io::Result<Self> {
        use MountAttrFlags as A;
        let ro = A::MOUNT_ATTR_RDONLY | A::MOUNT_ATTR_NOSUID | A::MOUNT_ATTR_NODEV;
        let ro_noexec = ro | A::MOUNT_ATTR_NOEXEC;
        let rw_noexec = A::MOUNT_ATTR_NOSUID | A::MOUNT_ATTR_NODEV | A::MOUNT_ATTR_NOEXEC;
        // Device nodes live here, so no NODEV.
        let dev = A::MOUNT_ATTR_RDONLY | A::MOUNT_ATTR_NOSUID | A::MOUNT_ATTR_NOEXEC;
        let rel = |p: &Path| p.strip_prefix("/").unwrap_or(p).to_path_buf();
        let own = |fd: &OwnedFd| -> io::Result<()> {
            Ok(chownat(fd, "", Some(Uid::from_raw(spec.uid)), Some(Gid::from_raw(spec.gid)), AtFlags::EMPTY_PATH)?)
        };

        let root = ctx(new_fs("tmpfs", &[("mode", "0755")], A::MOUNT_ATTR_NOSUID | A::MOUNT_ATTR_NODEV), || "root tmpfs".into())?;
        let rules = Ruleset::new()?;
        let all = rules.all();

        // 1. The store: one read-only bind; what may be opened in it is the closure.
        let store = rel(spec.store);
        mkdir_p(&root, &store)?;
        attach(ctx(clone_tree(spec.store, ro), || "store".into())?, &root, &store)?;
        let mut missing = 0;
        for path in spec.closure {
            if !rules.allow(Path::new(path), landlock::READ | landlock::EXECUTE)? {
                missing += 1;
            }
        }

        // 2. Host views: /dev and /sys from the boot-time generator, the live resolv.conf.
        let view = |name: &str, attrs| {
            ctx(clone_tree(&spec.views.join(name), attrs), || format!("host view {name} (drv-host-views not run?)"))
        };
        mkdir_p(&root, Path::new("dev"))?;
        attach(view("dev", dev)?, &root, Path::new("dev"))?;
        if spec.gpu {
            attach(view("dev-gpu/dri", dev)?, &root, Path::new("dev/dri"))?;
        }
        attach(new_fs("tmpfs", &[("mode", "1777")], A::MOUNT_ATTR_NOSUID | A::MOUNT_ATTR_NODEV)?, &root, Path::new("dev/shm"))?;
        rules.allow_at(&root, Path::new("dev"), landlock::READ | landlock::WRITE_FILE | landlock::IOCTL_DEV)?;
        rules.allow_at(&root, Path::new("dev/shm"), all)?;
        mkdir_p(&root, Path::new("sys"))?;
        attach(view(if spec.gpu { "sys-gpu" } else { "sys" }, ro_noexec)?, &root, Path::new("sys"))?;
        rules.allow_at(&root, Path::new("sys"), landlock::READ)?;
        // The kernel's view of the app's own processes. Its own instance (each proc mount is
        // one since 5.8), so the rule must come from this one.
        mkdir_p(&root, Path::new("proc"))?;
        attach(ctx(new_fs("proc", &[("hidepid", "invisible")], rw_noexec), || "proc".into())?, &root, Path::new("proc"))?;
        rules.allow_at(&root, Path::new("proc"), landlock::READ | landlock::WRITE_FILE)?;
        mkdir_p(&root, Path::new("run"))?;
        if spec.network {
            mkdir_p(&root, Path::new("run/host"))?;
            let f = rustix::fs::openat(&root, "run/host/resolv.conf", OFlags::CREATE | OFlags::WRONLY | OFlags::CLOEXEC, Mode::from_raw_mode(0o644))?;
            drop(f);
            attach(ctx(clone_tree(spec.resolv, ro_noexec), || "resolv.conf".into())?, &root, Path::new("run/host/resolv.conf"))?;
            rules.allow_at(&root, Path::new("run/host"), landlock::READ)?;
        }

        // 3. The app's own: /etc (filled by the linker from the store), HOME (a tmpfs with
        // what persists at .state inside), /tmp. The rules before the chowns: once a
        // directory is the app's 0700, we cannot look inside.
        let etc = ctx(new_fs("tmpfs", &[("mode", "0755")], rw_noexec), || "etc tmpfs".into())?;
        rules.allow_at(&etc, Path::new("."), all)?;
        own(&etc)?;
        mkdir_p(&root, Path::new("etc"))?;
        attach(etc, &root, Path::new("etc"))?;
        let home = ctx(new_fs("tmpfs", &[("mode", "0700"), ("size", "256m")], rw_noexec), || "home tmpfs".into())?;
        mkdir_p(&home, Path::new(".state"))?;
        attach(ctx(clone_tree(spec.state, rw_noexec), || "state".into())?, &home, Path::new(".state"))?;
        rules.allow_at(&home, Path::new("."), all)?;
        own(&home)?;
        let home_rel = rel(spec.home);
        mkdir_p(&root, &home_rel)?;
        attach(home, &root, &home_rel)?;
        mkdir_p(&root, Path::new("tmp"))?;
        attach(ctx(clone_tree(spec.tmp, rw_noexec), || "tmp".into())?, &root, Path::new("tmp"))?;
        rules.allow_at(&root, Path::new("tmp"), all)?;

        // 4. /run: exactly the entries the app's features call for.
        for entry in spec.run {
            let path = entry.path;
            let r = path
                .strip_prefix("/run")
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("{}: not under /run", path.display())))?;
            let dst = Path::new("run").join(r);
            let meta = ctx(std::fs::symlink_metadata(path), || format!("run entry {}", path.display()))?;
            if meta.file_type().is_symlink() {
                // The driver link: a symlink into the store, remade as one.
                let target = std::fs::read_link(path)?;
                mkdir_p(&root, dst.parent().unwrap())?;
                symlinkat(&target, &root, &dst)?;
            } else {
                mkdir_p(&root, &dst)?;
                attach(ctx(clone_tree(path, rw_noexec), || format!("run entry {}", path.display()))?, &root, &dst)?;
            }
            rules.allow_at(&root, &dst, if entry.writable { all } else { landlock::READ })?;
        }
        // Nothing new at the top level.
        set_attrs(&root, ro, false)?;
        Ok(Self { tree: root, ruleset: rules, no_network: !spec.network, jit: spec.jit, missing })
    }

    /// In the child, with CAP_SYS_ADMIN: a new mount namespace with this tree as its root.
    /// Syscalls on the fd and static strings only.
    pub fn enter(&self) -> io::Result<()> {
        let mut flags = UnshareFlags::NEWNS;
        if self.no_network {
            flags |= UnshareFlags::NEWNET;
        }
        // SAFETY: the child is single-threaded, between fork and exec.
        unsafe { unshare_unsafe(flags)? };
        mount_change("/", MountPropagationFlags::PRIVATE | MountPropagationFlags::REC)?;
        // Any directory of ours to hang the tree on for the moment it takes to pivot.
        move_mount(self.tree.as_fd(), "", rustix::fs::CWD, "/dev/shm", MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH)?;
        chdir("/dev/shm")?;
        // new_root and put_old the same: the old root lands on top, then goes.
        pivot_root(".", ".")?;
        unmount(".", UnmountFlags::DETACH)?;
        chdir("/")?;
        Ok(())
    }

    /// In the child, as the app, after no_new_privs: the Landlock domain and W^X memory.
    pub fn restrict(&self) -> io::Result<()> {
        self.ruleset.restrict_self()?;
        if !self.jit {
            const PR_SET_MDWE: libc::c_int = 65;
            const PR_MDWE_REFUSE_EXEC_GAIN: libc::c_ulong = 1;
            // SAFETY: plain prctl.
            if unsafe { libc::prctl(PR_SET_MDWE, PR_MDWE_REFUSE_EXEC_GAIN, 0, 0, 0) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}
