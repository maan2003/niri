//! Users, groups, fds and owned directories: the handful of libc calls every privileged piece
//! needs, in one place.

use std::ffi::CString;
use std::io;
use std::path::Path;

use rustix::fs::{Mode, OFlags};

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
pub fn ensure_owned_dir(path: &Path, uid: u32, gid: u32, mode: u32) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    match rustix::fs::mkdir(path, Mode::from_raw_mode(mode)) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(err) => return Err(format!("mkdir {}: {err}", path.display())),
    }
    let fd = rustix::fs::open(path, OFlags::DIRECTORY | OFlags::NOFOLLOW, Mode::empty())
        .map_err(|err| format!("open {}: {err}", path.display()))?;
    rustix::fs::fchown(
        &fd,
        Some(rustix::process::Uid::from_raw(uid)),
        Some(rustix::process::Gid::from_raw(gid)),
    )
    .map_err(|err| format!("chown {}: {err}", path.display()))?;
    rustix::fs::fchmod(&fd, Mode::from_raw_mode(mode))
        .map_err(|err| format!("chmod {}: {err}", path.display()))
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
