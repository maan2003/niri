//! Users, groups, fds and owned directories: the handful of libc calls every privileged piece
//! needs, in one place.

pub mod creds;
pub mod fds;
pub mod landlock;
pub mod mounts;
pub mod root;
pub mod seccomp;
pub mod userns;

use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};

use rustix::fs::FileType;

/// `eprintln!` for daemons: one `write(2)` per line. `eprintln!` writes every fragment of
/// the format string on its own, and journald has split a line between two of them.
#[macro_export]
macro_rules! say {
    ($($arg:tt)*) => {{
        use ::std::io::Write as _;
        let line = format!("{}\n", format_args!($($arg)*));
        let _ = ::std::io::stderr().lock().write_all(line.as_bytes());
    }};
}

/// `getgrnam_r`, so the command line and requests can use group names.
pub fn group_id(name: &str) -> Result<u32, String> {
    let cname = CString::new(name).map_err(|_| format!("bad group name {name:?}"))?;
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 16 * 1024];
    let mut result: *mut libc::group = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the call; buf outlives the use of `grp`.
    let rc = unsafe {
        libc::getgrnam_r(
            cname.as_ptr(),
            &mut grp,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        return Err(format!(
            "getgrnam {name:?}: {}",
            io::Error::from_raw_os_error(rc)
        ));
    }
    if result.is_null() {
        return Err(format!("no such group {name:?}"));
    }
    Ok(grp.gr_gid)
}

/// `getpwnam_r`: a user's uid and primary gid.
pub fn user_ids(name: &str) -> Result<(u32, u32), String> {
    let cname = CString::new(name).map_err(|_| format!("bad user name {name:?}"))?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the call; buf outlives the use of `pwd`.
    let rc = unsafe {
        libc::getpwnam_r(
            cname.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        return Err(format!(
            "getpwnam {name:?}: {}",
            io::Error::from_raw_os_error(rc)
        ));
    }
    if result.is_null() {
        return Err(format!("no such user {name:?}"));
    }
    Ok((pwd.pw_uid, pwd.pw_gid))
}

/// Every group of a user (`getgrouplist`), for services that keep their supplementary groups.
pub fn user_groups(name: &str, gid: u32) -> Result<Vec<u32>, String> {
    let cname = CString::new(name).map_err(|_| format!("bad user name {name:?}"))?;
    let mut n: libc::c_int = 64;
    loop {
        let mut groups = vec![0 as libc::gid_t; n as usize];
        // SAFETY: the buffer holds `n` gids; the call writes at most that many.
        let rc = unsafe { libc::getgrouplist(cname.as_ptr(), gid, groups.as_mut_ptr(), &mut n) };
        if rc >= 0 {
            groups.truncate(n as usize);
            return Ok(groups);
        }
        if n as usize <= groups.len() {
            return Err(format!("getgrouplist {name:?} failed"));
        }
    }
}

/// `fcntl(F_DUPFD_CLOEXEC)` at 10 or above, so a `dup2` onto a low target is never a same-fd
/// no-op (which would keep close-on-exec set).
pub fn dup_high(fd: i32) -> Result<i32, String> {
    // SAFETY: plain fcntl on an fd we own.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10) };
    if new < 0 {
        return Err(format!("dup fd {fd}: {}", io::Error::last_os_error()));
    }
    Ok(new)
}

/// Creates `path` (parents too) owned by `uid:gid` with `mode`, or fixes an existing one.
/// Our own cgroup v2 directory.
pub fn own_cgroup() -> Result<PathBuf, String> {
    let own = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| format!("/proc/self/cgroup: {e}"))?;
    // cgroup v2: a single line "0::/path".
    let path = own
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or_else(|| "not on cgroup v2".to_owned())?;
    Ok(PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

/// `path` exists, is a directory, and is `uid:gid` with `mode`: made by a tmpfiles rule, not
/// by us (we chown nothing).
pub fn check_owned_dir(path: &Path, uid: u32, gid: u32, mode: u32) -> Result<(), String> {
    let st = rustix::fs::lstat(path)
        .map_err(|err| format!("{}: {err} (a tmpfiles rule makes it)", path.display()))?;
    if !FileType::from_raw_mode(st.st_mode).is_dir() {
        return Err(format!("{}: not a directory", path.display()));
    }
    if st.st_uid != uid || st.st_gid != gid || st.st_mode & 0o7777 != mode {
        return Err(format!(
            "{}: {}:{} mode {:o}, wanted {uid}:{gid} mode {mode:o} (a tmpfiles rule makes it)",
            path.display(),
            st.st_uid,
            st.st_gid,
            st.st_mode & 0o7777
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_groups_and_users() {
        assert_eq!(group_id("root").unwrap(), 0);
        assert!(group_id("no-such-group-xyz").is_err());
        assert_eq!(user_ids("root").unwrap(), (0, 0));
        assert!(user_ids("no-such-user-xyz").is_err());
    }
}
