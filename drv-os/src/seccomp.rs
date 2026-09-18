//! One seccomp allowlist for the leaves: a baseline every confined daemon needs (its fds,
//! memory, threads, time, signals) plus a few additions per process. Anything else traps to a
//! handler that logs the syscall number and fails the call with EPERM, so libraries degrade
//! instead of the process dying, and the journal says what to look at. (A trap while the
//! caller has signals blocked still kills the process; `clone3` is the known such case and
//! gets ENOSYS from a plain errno filter.) Needs no_new_privs, which the supervisor sets.

use std::collections::BTreeMap;
use std::ffi::c_int;
use std::io;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};

use libc::*;
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen as Len, SeccompCmpOp as Op,
    SeccompCondition as Cond, SeccompFilter, SeccompRule,
};

/// `DRV_SECCOMP=0` in a leaf's environment (from the supervisor's command line, nowhere else)
/// runs it unconfined, for debugging.
pub const DISABLE_ENV: &str = "DRV_SECCOMP";

pub fn enabled() -> bool {
    std::env::var_os(DISABLE_ENV).is_none_or(|v| v != "0")
}

fn err(e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("seccomp: {e}"))
}

/// Syscalls a process may make; an entry without conditions allows any arguments.
pub struct Allowlist {
    map: BTreeMap<i64, Vec<SeccompRule>>,
}

impl Allowlist {
    /// What every leaf needs: talking on the fds it already holds, memory, event loops,
    /// threads (never processes), time, signals to itself, exiting. No open, no socket, no
    /// exec, ioctl only to switch fds nonblocking.
    pub fn base() -> io::Result<Self> {
        let mut this = Self {
            map: BTreeMap::new(),
        };
        this.allow(&[
            // Talking on our fds.
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
            // Threads.
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
            // Fails with ENOSYS through the errno filter; listed so this one does not trap it.
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
        ]);
        // Nonblocking fds and pending byte counts.
        for req in [FIONBIO, FIONREAD] {
            this.ioctl(req)?;
        }
        // Threads only, never processes.
        let thread = CLONE_THREAD as u64;
        this.allow_when(
            SYS_clone,
            vec![Cond::new(0, Len::Qword, Op::MaskedEq(thread), thread).map_err(err)?],
        )?;
        // Signals to ourselves only (abort, panics).
        let pid = std::process::id() as u64;
        this.allow_when(
            SYS_tgkill,
            vec![Cond::new(0, Len::Dword, Op::Eq, pid).map_err(err)?],
        )?;
        // Thread and mapping names.
        for opt in [PR_SET_NAME, PR_GET_NAME, PR_SET_VMA] {
            this.allow_when(
                SYS_prctl,
                vec![Cond::new(0, Len::Dword, Op::Eq, opt as u64).map_err(err)?],
            )?;
        }
        // stat by fd only: glibc and Rust implement fstat via these with an empty path.
        let empty = AT_EMPTY_PATH as u64;
        this.allow_when(
            SYS_newfstatat,
            vec![Cond::new(3, Len::Dword, Op::MaskedEq(empty), empty).map_err(err)?],
        )?;
        this.allow_when(
            SYS_statx,
            vec![Cond::new(2, Len::Dword, Op::MaskedEq(empty), empty).map_err(err)?],
        )?;
        Ok(this)
    }

    /// Allows these with any arguments (replacing any conditions on them).
    pub fn allow(&mut self, syscalls: &[c_long]) -> &mut Self {
        for nr in syscalls {
            self.map.insert(*nr, Vec::new());
        }
        self
    }

    /// Allows `nr` when `conds` hold (on top of other conditions already allowed for it). A
    /// syscall already allowed unconditionally stays so.
    pub fn allow_when(&mut self, nr: c_long, conds: Vec<Cond>) -> io::Result<&mut Self> {
        match self.map.get_mut(&nr) {
            Some(rules) if rules.is_empty() => {}
            Some(rules) => rules.push(SeccompRule::new(conds).map_err(err)?),
            None => {
                self.map.insert(nr, vec![SeccompRule::new(conds).map_err(err)?]);
            }
        }
        Ok(self)
    }

    /// A listening socket: accepting connections (peer credentials come with getsockopt).
    pub fn accept(&mut self) -> &mut Self {
        self.allow(&[
            SYS_accept4,
            #[cfg(target_arch = "x86_64")]
            SYS_accept,
        ])
    }

    /// One ioctl request.
    pub fn ioctl(&mut self, request: c_ulong) -> io::Result<&mut Self> {
        self.allow_when(
            SYS_ioctl,
            vec![Cond::new(1, Len::Dword, Op::Eq, request as u64).map_err(err)?],
        )
    }

    /// Every ioctl of one `_IOC_TYPE` byte (`b'd'` is DRM).
    pub fn ioctl_type(&mut self, ty: u8) -> io::Result<&mut Self> {
        const IOC_TYPE_MASK: u64 = 0xff00;
        self.allow_when(
            SYS_ioctl,
            vec![Cond::new(1, Len::Dword, Op::MaskedEq(IOC_TYPE_MASK), (ty as u64) << 8).map_err(err)?],
        )
    }

    /// Files by path, read-only: `openat` without write, create, truncate or append, stat,
    /// readlink, access, directory listing. For fonts, config and `/proc` tables.
    pub fn read_files(&mut self) -> io::Result<&mut Self> {
        const O_TMPFILE_BIT: u64 = 0o20000000; // __O_TMPFILE; O_TMPFILE itself carries O_DIRECTORY
        let writing = (O_WRONLY | O_RDWR | O_CREAT | O_TRUNC | O_APPEND) as u64 | O_TMPFILE_BIT;
        self.allow_when(
            SYS_openat,
            vec![Cond::new(2, Len::Dword, Op::MaskedEq(writing), 0).map_err(err)?],
        )?;
        self.allow(&[
            SYS_newfstatat,
            SYS_statx,
            SYS_readlinkat,
            SYS_faccessat,
            SYS_faccessat2,
            SYS_getdents64,
            #[cfg(target_arch = "x86_64")]
            SYS_stat,
            #[cfg(target_arch = "x86_64")]
            SYS_lstat,
            #[cfg(target_arch = "x86_64")]
            SYS_readlink,
            #[cfg(target_arch = "x86_64")]
            SYS_access,
            #[cfg(target_arch = "x86_64")]
            SYS_getdents,
        ]);
        #[cfg(target_arch = "x86_64")]
        self.allow_when(
            SYS_open,
            vec![Cond::new(1, Len::Dword, Op::MaskedEq(writing), 0).map_err(err)?],
        )?;
        Ok(self)
    }

    /// Files by path, read-write: a daemon's own state directory (open, rename, unlink, mkdir,
    /// chmod) on top of [`Self::read_files`].
    pub fn write_files(&mut self) -> io::Result<&mut Self> {
        self.read_files()?;
        self.allow(&[
            SYS_openat,
            SYS_renameat,
            SYS_renameat2,
            SYS_unlinkat,
            SYS_mkdirat,
            SYS_fchmodat,
            SYS_fchmod,
            #[cfg(target_arch = "x86_64")]
            SYS_open,
            #[cfg(target_arch = "x86_64")]
            SYS_rename,
            #[cfg(target_arch = "x86_64")]
            SYS_unlink,
            #[cfg(target_arch = "x86_64")]
            SYS_mkdir,
            #[cfg(target_arch = "x86_64")]
            SYS_chmod,
        ]);
        Ok(self)
    }

    /// Installs the filter on every thread. `tag` prefixes the denial log lines.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    pub fn apply(self, tag: &'static str) -> io::Result<()> {
        install_sigsys_handler(tag)?;

        #[cfg(target_arch = "x86_64")]
        let arch = seccompiler::TargetArch::x86_64;
        #[cfg(target_arch = "aarch64")]
        let arch = seccompiler::TargetArch::aarch64;

        // glibc calls clone3 with every signal blocked, so a trap there would kill the process
        // instead of reaching the handler. This filter fails it with ENOSYS (the allowlist
        // lets it through; the kernel applies the strictest verdict), and glibc retries with
        // clone, which the allowlist restricts to threads.
        let clone3 = SeccompFilter::new(
            [(SYS_clone3, Vec::new())].into_iter().collect(),
            SeccompAction::Allow,
            SeccompAction::Errno(ENOSYS as u32),
            arch,
        )
        .map_err(err)?;
        let bpf: BpfProgram = clone3.try_into().map_err(err)?;
        seccompiler::apply_filter_all_threads(&bpf).map_err(err)?;

        let filter = SeccompFilter::new(self.map, SeccompAction::Trap, SeccompAction::Allow, arch)
            .map_err(err)?;
        let bpf: BpfProgram = filter.try_into().map_err(err)?;
        seccompiler::apply_filter_all_threads(&bpf).map_err(err)?;
        Ok(())
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    pub fn apply(self, _tag: &'static str) -> io::Result<()> {
        Err(io::Error::other("seccomp sandbox is only implemented for x86_64 and aarch64"))
    }
}

/// How many denials get logged before going quiet (a library retrying in a loop).
const LOG_LIMIT: u32 = 200;
static LOGGED: AtomicU32 = AtomicU32::new(0);
static TAG_PTR: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());
static TAG_LEN: AtomicUsize = AtomicUsize::new(0);

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
fn install_sigsys_handler(tag: &'static str) -> io::Result<()> {
    TAG_PTR.store(tag.as_ptr().cast_mut(), Ordering::Relaxed);
    TAG_LEN.store(tag.len(), Ordering::Relaxed);
    // SAFETY: plain sigaction setup; the handler only touches async-signal-safe things.
    unsafe {
        let mut action: sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigsys as *const () as usize;
        action.sa_flags = SA_SIGINFO | SA_NODEFER | SA_ONSTACK;
        sigemptyset(&mut action.sa_mask);
        if sigaction(SIGSYS, &action, std::ptr::null_mut()) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
extern "C" fn on_sigsys(_signal: c_int, info: *mut siginfo_t, ctx: *mut c_void) {
    // SAFETY: the kernel passes a valid siginfo and ucontext. For SIGSYS, `si_syscall` is the
    // int at offset 24 on 64-bit Linux (after si_signo, si_errno, si_code, padding, and the
    // `si_call_addr` pointer). The libc crate has no accessor for it.
    let nr = unsafe { *info.cast::<u8>().add(24).cast::<c_int>() };
    let ret = -(EPERM as i64);
    // SAFETY: writing the syscall return register of the interrupted context.
    unsafe {
        let ctx = &mut *ctx.cast::<ucontext_t>();
        #[cfg(target_arch = "x86_64")]
        {
            ctx.uc_mcontext.gregs[REG_RAX as usize] = ret;
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
    let mut buf = [0u8; 96];
    let mut len = 0;
    let tag_len = TAG_LEN.load(Ordering::Relaxed).min(40);
    let tag = TAG_PTR.load(Ordering::Relaxed);
    if !tag.is_null() {
        // SAFETY: a &'static str's bytes, stored by install_sigsys_handler.
        let tag = unsafe { std::slice::from_raw_parts(tag, tag_len) };
        buf[..tag_len].copy_from_slice(tag);
        len += tag_len;
    }
    let msg = b": seccomp blocked syscall ";
    buf[len..len + msg.len()].copy_from_slice(msg);
    len += msg.len();

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
        write(STDERR_FILENO, buf.as_ptr().cast(), len);
    }
}
