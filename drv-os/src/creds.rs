//! Credentials between fork and exec, for the trusted set (the supervisor's children keep
//! the capabilities listed for them) and for apps (the forker's child keeps none). Only
//! syscalls on the process's own credentials; allocation only for an error's text.

use rustix::thread::{CapabilitiesSecureBits, CapabilitySet, CapabilitySets, Gid, Uid};

/// Every bit locked, before anything else: no root, no setuid fixups, no ambient raise, and
/// (where the kernel knows them, 6.14) exec restricted to files, not scripts.
pub fn lock_securebits() -> Result<(), String> {
    const NOROOT: u32 = 1 << 0;
    const NO_SETUID_FIXUP: u32 = 1 << 2;
    const KEEP_CAPS_LOCKED: u32 = 1 << 5;
    const NO_CAP_AMBIENT_RAISE: u32 = 1 << 6;
    const EXEC_RESTRICT_FILE: u32 = 1 << 8;
    const EXEC_DENY_INTERACTIVE: u32 = 1 << 10;
    let locked = |bit: u32| bit | (bit << 1);
    let base =
        locked(NOROOT) | locked(NO_SETUID_FIXUP) | KEEP_CAPS_LOCKED | locked(NO_CAP_AMBIENT_RAISE);
    let exec = locked(EXEC_RESTRICT_FILE) | locked(EXEC_DENY_INTERACTIVE);
    // rustix's flag set predates the exec bits: retain them past its check.
    let set = |bits: u32| {
        rustix::thread::set_capabilities_secure_bits(CapabilitiesSecureBits::from_bits_retain(bits))
    };
    if set(base | exec).is_ok() {
        return Ok(());
    }
    set(base).map_err(|e| format!("securebits: {e}"))
}

/// Nothing exec'd from here on can gain a capability, whatever its file says.
pub fn empty_bounding_set() -> Result<(), String> {
    for cap in CapabilitySet::all().iter() {
        if cap.bits().count_ones() == 1
            && rustix::thread::capability_is_in_bounding_set(cap).map_err(|e| e.to_string())?
        {
            rustix::thread::remove_capability_from_bounding_set(cap)
                .map_err(|e| format!("bounding set: {e}"))?;
        }
    }
    Ok(())
}

pub fn drop_capability(cap: CapabilitySet) -> Result<(), String> {
    let mut caps = rustix::thread::capabilities(None).map_err(|e| e.to_string())?;
    caps.effective.remove(cap);
    caps.permitted.remove(cap);
    caps.inheritable.remove(cap);
    rustix::thread::set_capabilities(None, caps).map_err(|e| format!("capset: {e}"))
}

/// Become `uid`/`gid` with exactly `groups` and exactly `caps`: the bounding set is `caps`
/// (nothing exec'd from here on can have more), so are the permitted, effective, inheritable
/// and ambient sets (ambient, so they survive the exec). Works for root and for a caller
/// that holds `caps` itself plus SETUID and SETGID; with `caps` empty, also under locked
/// securebits (the forker's), since keepcaps is not touched.
pub fn switch_to(uid: Uid, gid: Gid, groups: &[Gid], caps: CapabilitySet) -> Result<(), String> {
    // The bounding set first, while CAP_SETPCAP is still effective.
    for cap in CapabilitySet::all().iter() {
        if cap.bits().count_ones() == 1
            && !caps.contains(cap)
            && rustix::thread::capability_is_in_bounding_set(cap).unwrap_or(false)
        {
            rustix::thread::remove_capability_from_bounding_set(cap)
                .map_err(|e| format!("bounding set: {e}"))?;
        }
    }
    if !caps.is_empty() {
        // Keep the permitted set across the uid change; it is narrowed to `caps` right after.
        rustix::thread::set_keep_capabilities(true).map_err(|e| format!("keepcaps: {e}"))?;
    }
    rustix::thread::set_thread_groups(groups).map_err(|e| format!("setgroups: {e}"))?;
    rustix::thread::set_thread_res_gid(gid, gid, gid).map_err(|e| format!("setresgid: {e}"))?;
    rustix::thread::set_thread_res_uid(uid, uid, uid).map_err(|e| format!("setresuid: {e}"))?;
    if rustix::process::getuid() != uid || rustix::process::geteuid() != uid {
        return Err("uid did not change".into());
    }
    rustix::thread::clear_ambient_capability_set().map_err(|e| format!("ambient: {e}"))?;
    rustix::thread::set_capabilities(
        None,
        CapabilitySets {
            effective: caps,
            permitted: caps,
            inheritable: caps,
        },
    )
    .map_err(|e| format!("capset: {e}"))?;
    for cap in caps.iter() {
        if cap.bits().count_ones() == 1 {
            rustix::thread::configure_capability_in_ambient_set(cap, true)
                .map_err(|e| format!("ambient {cap:?}: {e}"))?;
        }
    }
    if !caps.is_empty() {
        rustix::thread::set_keep_capabilities(false).map_err(|e| format!("keepcaps: {e}"))?;
    }
    Ok(())
}

/// `cap` gone from every set of this process and from its bounding set: nothing this process
/// or anything it starts can have it again (needs SETPCAP).
pub fn drop_for_good(cap: CapabilitySet) -> Result<(), String> {
    rustix::thread::remove_capability_from_bounding_set(cap)
        .map_err(|e| format!("bounding set: {e}"))?;
    drop_capability(cap)
}
