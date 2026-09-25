//! A user namespace made for one mapping, as `mount --map-users` makes it: a helper process
//! unshares one, the caller (root in the initial namespace, so it may write any map) fills
//! in the maps and keeps the namespace's handle, and the helper is gone. The handle is what
//! an idmapped mount takes (`mounts::set_idmap`); no process ever runs in the namespace.

use std::io;
use std::os::fd::OwnedFd;

use rustix::fs::{Mode, OFlags};

/// One mapping line, `uid_map`'s columns: `first` is the id in the namespace, `second` the
/// id outside it. An idmapped mount reads the namespace's map the other way round from a
/// process in it: what the filesystem says (`first`, drv-files' ids) appears as `second`
/// (the app's), and what the app writes goes back the same way. The caller must be
/// single-threaded (it forks).
pub fn map(first: (u32, u32), second: (u32, u32)) -> io::Result<OwnedFd> {
    // SAFETY: single-threaded by contract; the child only unshares and waits to be killed.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // SAFETY: the child is single-threaded too.
        if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
            std::process::exit(1);
        }
        loop {
            // SAFETY: plain pause.
            unsafe { libc::pause() };
        }
    }
    let result = fill(pid, first, second);
    // SAFETY: our own child, killed and reaped.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
    result
}

fn fill(pid: libc::pid_t, first: (u32, u32), second: (u32, u32)) -> io::Result<OwnedFd> {
    // The child's pid as /proc knows it: we may be PID 1 of a fresh pid namespace (the
    // forker's child is), where fork's answer means nothing to the host's /proc. A pidfd's
    // fdinfo says the pid in /proc's namespace.
    let pidfd = rustix::process::pidfd_open(
        rustix::process::Pid::from_raw(pid).ok_or_else(|| io::Error::other("pid 0"))?,
        rustix::process::PidfdFlags::empty(),
    )?;
    let info = std::fs::read_to_string(format!(
        "/proc/self/fdinfo/{}",
        std::os::fd::AsRawFd::as_raw_fd(&pidfd)
    ))?;
    let host_pid = info
        .lines()
        .find_map(|l| l.strip_prefix("Pid:"))
        .and_then(|p| p.trim().parse::<i64>().ok())
        .filter(|p| *p > 0)
        .ok_or_else(|| io::Error::other("pidfd's fdinfo has no Pid"))?;
    // The helper is in the namespace once its ns link differs from ours.
    let ours = std::fs::read_link("/proc/self/ns/user")?;
    let path = format!("/proc/{host_pid}/ns/user");
    for _ in 0..1000 {
        if std::fs::read_link(&path)? != ours {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    if std::fs::read_link(&path)? == ours {
        return Err(io::Error::other("the helper did not unshare"));
    }
    std::fs::write(
        format!("/proc/{host_pid}/uid_map"),
        format!("{} {} 1\n", first.0, second.0),
    )?;
    std::fs::write(
        format!("/proc/{host_pid}/gid_map"),
        format!("{} {} 1\n", first.1, second.1),
    )?;
    Ok(rustix::fs::open(
        &path,
        OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )?)
}
