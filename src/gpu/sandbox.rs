//! Confinement of the GPU process.
//!
//! The process starts with everything it will ever need: the socket to the core and the DRM
//! devices the core opened, on its command line. Mesa initializes on those (loading drivers,
//! opening render nodes), then `lockdown` applies a seccomp allowlist and only then does the
//! process answer the core. No open, no socket, no exec, ioctl only for DRM / dma-buf /
//! sync-file requests. Files it needs later (cursor theme icons, the PipeWire socket) arrive as
//! fds on the requests that use them.
//!
//! A syscall outside the allowlist traps to a SIGSYS handler that logs the syscall number and
//! fails the call with EPERM. Libraries then degrade instead of the process dying, and the log
//! tells us what to look at. (A trap while the caller has signals blocked still kills the
//! process; `clone3` is the known such case and gets ENOSYS from a plain errno filter.)

use std::ffi::c_int;
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::Context as _;

/// Environment variable the core forwards; `0` runs the GPU process unconfined (debugging).
pub const DISABLE_ENV: &str = "NIRI_GPU_SANDBOX";

pub fn enabled() -> bool {
    std::env::var_os(DISABLE_ENV).is_none_or(|v| v != "0")
}

/// Seccomp: after this, the process can only talk to the fds it already holds.
#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
pub fn lockdown() -> anyhow::Result<()> {
    use seccompiler::{BpfProgram, SeccompAction, SeccompFilter};

    install_sigsys_handler()?;

    #[cfg(target_arch = "x86_64")]
    let arch = seccompiler::TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")]
    let arch = seccompiler::TargetArch::aarch64;

    // glibc calls clone3 with every signal blocked, so a trap there would kill the process
    // instead of reaching the handler. A separate filter fails it with ENOSYS (the allowlist
    // lets it through; the kernel applies the strictest verdict), and glibc retries with
    // clone, which the allowlist restricts to threads.
    let clone3 = SeccompFilter::new(
        [(libc::SYS_clone3, Vec::new())].into_iter().collect(),
        SeccompAction::Allow,
        SeccompAction::Errno(libc::ENOSYS as u32),
        arch,
    )?;
    let bpf: BpfProgram = clone3.try_into()?;
    seccompiler::apply_filter_all_threads(&bpf)?;

    let filter = SeccompFilter::new(rules()?, SeccompAction::Trap, SeccompAction::Allow, arch)?;
    let bpf: BpfProgram = filter.try_into()?;
    seccompiler::apply_filter_all_threads(&bpf)?;
    debug!("seccomp: syscall allowlist applied");
    Ok(())
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn lockdown() -> anyhow::Result<()> {
    anyhow::bail!("seccomp sandbox is only implemented for x86_64 and aarch64")
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn rules() -> anyhow::Result<std::collections::BTreeMap<i64, Vec<seccompiler::SeccompRule>>> {
    use std::collections::BTreeMap;

    use libc::*;
    use seccompiler::{
        SeccompCmpArgLen as Len, SeccompCmpOp as Op, SeccompCondition as Cond, SeccompRule,
    };

    let mut map: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    // An entry without rules allows the syscall with any arguments.
    let unconditional: &[c_long] = &[
        // Talking to the core, DRM, PipeWire.
        SYS_read,
        SYS_write,
        SYS_readv,
        SYS_writev,
        SYS_pread64,
        SYS_pwrite64,
        SYS_preadv,
        SYS_pwritev,
        SYS_sendmsg,
        SYS_recvmsg,
        SYS_sendto,
        SYS_recvfrom,
        SYS_getsockopt,
        SYS_setsockopt,
        SYS_getsockname,
        SYS_getpeername,
        SYS_shutdown,
        SYS_close,
        SYS_close_range,
        SYS_fstat,
        SYS_lseek,
        SYS_fcntl,
        SYS_dup,
        SYS_dup3,
        SYS_ftruncate,
        SYS_fallocate,
        SYS_fsync,
        SYS_fdatasync,
        SYS_memfd_create,
        SYS_pipe2,
        // Memory.
        SYS_mmap,
        SYS_munmap,
        SYS_mprotect,
        SYS_mremap,
        SYS_madvise,
        SYS_msync,
        SYS_mincore,
        SYS_brk,
        SYS_membarrier,
        // Event loops.
        SYS_epoll_create1,
        SYS_epoll_ctl,
        SYS_epoll_pwait,
        SYS_epoll_pwait2,
        SYS_ppoll,
        SYS_pselect6,
        SYS_eventfd2,
        SYS_timerfd_create,
        SYS_timerfd_settime,
        SYS_timerfd_gettime,
        SYS_signalfd4,
        // Threads (Mesa compiler threads, PipeWire loops, PNG encoding).
        SYS_futex,
        SYS_get_robust_list,
        SYS_set_robust_list,
        SYS_rseq,
        SYS_sched_yield,
        SYS_sched_getaffinity,
        SYS_sched_setaffinity,
        SYS_sched_getparam,
        SYS_sched_getscheduler,
        SYS_getpriority,
        SYS_getrlimit,
        SYS_prlimit64,
        // Time, identity, misc.
        SYS_clock_gettime,
        SYS_clock_getres,
        SYS_clock_nanosleep,
        SYS_nanosleep,
        SYS_gettimeofday,
        SYS_getpid,
        SYS_gettid,
        SYS_getppid,
        SYS_getuid,
        SYS_geteuid,
        SYS_getgid,
        SYS_getegid,
        SYS_getrandom,
        SYS_uname,
        SYS_sysinfo,
        SYS_getcpu,
        SYS_restart_syscall,
        // Signals (the SIGSYS handler itself returns through rt_sigreturn).
        SYS_rt_sigaction,
        SYS_rt_sigprocmask,
        SYS_rt_sigreturn,
        SYS_rt_sigpending,
        SYS_rt_sigtimedwait,
        SYS_rt_sigsuspend,
        SYS_sigaltstack,
        // Exiting.
        SYS_exit,
        SYS_exit_group,
        // Fails with ENOSYS through the filter above; listed so this one does not trap it.
        SYS_clone3,
        #[cfg(target_arch = "x86_64")]
        SYS_poll,
        #[cfg(target_arch = "x86_64")]
        SYS_select,
        #[cfg(target_arch = "x86_64")]
        SYS_epoll_wait,
        #[cfg(target_arch = "x86_64")]
        SYS_epoll_create,
        #[cfg(target_arch = "x86_64")]
        SYS_dup2,
        #[cfg(target_arch = "x86_64")]
        SYS_pipe,
        #[cfg(target_arch = "x86_64")]
        SYS_eventfd,
        #[cfg(target_arch = "x86_64")]
        SYS_arch_prctl,
        #[cfg(target_arch = "x86_64")]
        SYS_time,
    ];
    for nr in unconditional {
        map.insert(*nr, Vec::new());
    }

    let mut rule = |nr: c_long, conds: Vec<Cond>| -> anyhow::Result<()> {
        map.entry(nr).or_default().push(SeccompRule::new(conds)?);
        Ok(())
    };

    // ioctl: only by request type. 'd' is DRM, 'b' dma-buf, '>' sync_file; the rest is for
    // sockets (PipeWire).
    const IOC_TYPE_MASK: u64 = 0xff00;
    for ty in *b"db>" {
        rule(
            SYS_ioctl,
            vec![Cond::new(
                1,
                Len::Dword,
                Op::MaskedEq(IOC_TYPE_MASK),
                (ty as u64) << 8,
            )?],
        )?;
    }
    for req in [FIONBIO, FIONREAD] {
        rule(SYS_ioctl, vec![Cond::new(1, Len::Dword, Op::Eq, req)?])?;
    }

    // Threads only, never processes.
    let thread = CLONE_THREAD as u64;
    rule(
        SYS_clone,
        vec![Cond::new(0, Len::Qword, Op::MaskedEq(thread), thread)?],
    )?;

    // Signals to ourselves only (abort, panics).
    let pid = std::process::id() as u64;
    rule(SYS_tgkill, vec![Cond::new(0, Len::Dword, Op::Eq, pid)?])?;

    // Thread and mapping names.
    for opt in [PR_SET_NAME, PR_GET_NAME, PR_SET_VMA] {
        rule(
            SYS_prctl,
            vec![Cond::new(0, Len::Dword, Op::Eq, opt as u64)?],
        )?;
    }

    // stat by fd only: glibc and Rust implement fstat via these with an empty path.
    let empty = AT_EMPTY_PATH as u64;
    rule(
        SYS_newfstatat,
        vec![Cond::new(3, Len::Dword, Op::MaskedEq(empty), empty)?],
    )?;
    rule(
        SYS_statx,
        vec![Cond::new(2, Len::Dword, Op::MaskedEq(empty), empty)?],
    )?;

    Ok(map)
}

/// How many denials get logged before going quiet (a library retrying in a loop).
const LOG_LIMIT: u32 = 200;
static LOGGED: AtomicU32 = AtomicU32::new(0);

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn install_sigsys_handler() -> anyhow::Result<()> {
    // SAFETY: plain sigaction setup; the handler only touches async-signal-safe things.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigsys as *const () as usize;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_NODEFER | libc::SA_ONSTACK;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGSYS, &action, std::ptr::null_mut()) != 0 {
            return Err(std::io::Error::last_os_error()).context("installing SIGSYS handler");
        }
    }
    Ok(())
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
extern "C" fn on_sigsys(_signal: c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    // SAFETY: the kernel passes a valid siginfo and ucontext. For SIGSYS, `si_syscall` is the
    // int at offset 24 on 64-bit Linux (after si_signo, si_errno, si_code, padding, and the
    // `si_call_addr` pointer). The libc crate has no accessor for it.
    let nr = unsafe { *info.cast::<u8>().add(24).cast::<c_int>() };
    let ret = -(libc::EPERM as i64);
    // SAFETY: writing the syscall return register of the interrupted context.
    unsafe {
        let ctx = &mut *ctx.cast::<libc::ucontext_t>();
        #[cfg(target_arch = "x86_64")]
        {
            ctx.uc_mcontext.gregs[libc::REG_RAX as usize] = ret;
        }
        #[cfg(target_arch = "aarch64")]
        {
            ctx.uc_mcontext.regs[0] = ret as u64;
        }
    }
    log_denied(nr);
}

/// Async-signal-safe: formats by hand and uses write(2) on stderr.
fn log_denied(nr: c_int) {
    if LOGGED.fetch_add(1, Ordering::Relaxed) >= LOG_LIMIT {
        return;
    }
    let prefix = b"niri gpu-process: seccomp blocked syscall ";
    let mut buf = [0u8; 64];
    let mut len = 0;
    buf[..prefix.len()].copy_from_slice(prefix);
    len += prefix.len();

    let mut digits = [0u8; 12];
    let mut n = nr.unsigned_abs();
    let mut d = 0;
    loop {
        digits[d] = b'0' + (n % 10) as u8;
        d += 1;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    if nr < 0 {
        buf[len] = b'-';
        len += 1;
    }
    while d > 0 {
        d -= 1;
        buf[len] = digits[d];
        len += 1;
    }
    buf[len] = b'\n';
    len += 1;
    // SAFETY: buf is valid for len bytes; write(2) is async-signal-safe.
    unsafe {
        libc::write(libc::STDERR_FILENO, buf.as_ptr().cast(), len);
    }
}
