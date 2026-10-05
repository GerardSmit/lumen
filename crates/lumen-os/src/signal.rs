//! Signals: dispositions and process-wide delivery for every runtime. A caught signal sets its
//! pending bit and writes its number to every registered wake-up descriptor; each runtime then drains the pending set on its own thread (the Python
//! interpreter between bytecodes, the Node runtime from its watcher thread).

use crate::errno::FsError;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

pub type R<T> = Result<T, FsError>;

/// One more than the highest signal number (the C `NSIG`).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub const NSIG: i32 = 65;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub const NSIG: i32 = 32;

/// What a signal does when it arrives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Disposition {
    Default,
    Ignore,
    /// Delivered through this module.
    Catch,
    /// A handler installed by someone else.
    Foreign,
}

/// The signals of this platform by name (Windows: the C runtime's numbers libuv emulates).
pub fn names() -> &'static [(&'static str, i32)] {
    SIGNALS
}

/// The number of signal `name` (`"SIGTERM"`) on this platform.
pub fn number(name: &str) -> Option<i32> {
    SIGNALS.iter().find(|(n, _)| *n == name).map(|e| e.1)
}

#[cfg(unix)]
macro_rules! signal_table {
    ($($(#[$m:meta])* $name:ident),* $(,)?) => {
        static SIGNALS: &[(&str, i32)] = &[$($(#[$m])* (stringify!($name), libc::$name),)*];
    };
}

#[cfg(unix)]
signal_table!(
    SIGHUP,
    SIGINT,
    SIGQUIT,
    SIGILL,
    SIGTRAP,
    SIGABRT,
    SIGIOT,
    SIGBUS,
    SIGFPE,
    SIGKILL,
    SIGUSR1,
    SIGSEGV,
    SIGUSR2,
    SIGPIPE,
    SIGALRM,
    SIGTERM,
    SIGCHLD,
    SIGCONT,
    SIGSTOP,
    SIGTSTP,
    SIGTTIN,
    SIGTTOU,
    SIGURG,
    SIGXCPU,
    SIGXFSZ,
    SIGVTALRM,
    SIGPROF,
    SIGWINCH,
    SIGIO,
    SIGSYS,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    SIGSTKFLT,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    SIGPOLL,
    #[cfg(any(target_os = "linux", target_os = "android"))]
    SIGPWR,
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    SIGEMT,
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    SIGINFO,
);

#[cfg(not(unix))]
static SIGNALS: &[(&str, i32)] = &[
    ("SIGHUP", 1),
    ("SIGINT", 2),
    ("SIGILL", 4),
    ("SIGABRT", 22),
    ("SIGFPE", 8),
    ("SIGKILL", 9),
    ("SIGSEGV", 11),
    ("SIGTERM", 15),
    ("SIGBREAK", 21),
    ("SIGWINCH", 28),
];

/// The wake-up descriptor slots: one per runtime, so each keeps its own.
pub const WAKE_NODE: usize = 0;
pub const WAKE_PYTHON: usize = 1;

static PENDING: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static ANY_PENDING: AtomicBool = AtomicBool::new(false);
static WAKE: [AtomicI32; 2] = [AtomicI32::new(-1), AtomicI32::new(-1)];

/// Whether a caught signal is waiting for [`take_pending`]. One relaxed load, for hot paths.
#[inline(always)]
pub fn any_pending() -> bool {
    ANY_PENDING.load(Ordering::Relaxed)
}

/// The caught signals not yet handled, in increasing order; clears them.
pub fn take_pending() -> Vec<i32> {
    ANY_PENDING.store(false, Ordering::SeqCst);
    let mut out = Vec::new();
    for (word, bits) in PENDING.iter().enumerate() {
        let mut b = bits.swap(0, Ordering::SeqCst);
        while b != 0 {
            let i = b.trailing_zeros();
            out.push(word as i32 * 64 + i as i32);
            b &= b - 1;
        }
    }
    out
}

/// Marks `sig` pending again (a handler that could not run yet).
pub fn repend(sig: i32) {
    if (0..128).contains(&sig) {
        PENDING[sig as usize / 64].fetch_or(1 << (sig % 64), Ordering::SeqCst);
        ANY_PENDING.store(true, Ordering::SeqCst);
    }
}

/// Sets the wake-up descriptor of `slot` (`-1`: none); returns the previous one.
pub fn set_wakeup_fd(slot: usize, fd: i32) -> i32 {
    WAKE[slot].swap(fd, Ordering::SeqCst)
}

pub fn wakeup_fd(slot: usize) -> i32 {
    WAKE[slot].load(Ordering::SeqCst)
}

/// The `sigmask` `how` values and the interval timers, by C name.
pub fn constants() -> &'static [(&'static str, i32)] {
    #[cfg(unix)]
    {
        &[
            ("SIG_BLOCK", libc::SIG_BLOCK),
            ("SIG_UNBLOCK", libc::SIG_UNBLOCK),
            ("SIG_SETMASK", libc::SIG_SETMASK),
            ("ITIMER_REAL", 0),
            ("ITIMER_VIRTUAL", 1),
            ("ITIMER_PROF", 2),
        ]
    }
    #[cfg(not(unix))]
    {
        &[]
    }
}

/// Blocks every asynchronous signal on the calling thread, so process-directed signals reach
/// the runtime's own thread (a launcher thread that only waits for it calls this).
pub fn block_on_this_thread() {
    #[cfg(unix)]
    {
        let sync = [
            libc::SIGSEGV,
            libc::SIGBUS,
            libc::SIGFPE,
            libc::SIGILL,
            libc::SIGTRAP,
            libc::SIGABRT,
        ];
        let sigs: Vec<i32> = valid_signals()
            .into_iter()
            .filter(|s| !sync.contains(s))
            .collect();
        let _ = sigmask(libc::SIG_BLOCK, &sigs);
    }
}

/// Ignores `SIGXFSZ`, so a write past `RLIMIT_FSIZE` fails with `EFBIG` instead of killing the
/// process (`SIGPIPE` is already ignored by Rust's runtime).
pub fn ignore_file_size_limit_signal() {
    #[cfg(unix)]
    // SAFETY: signal(3) with SIG_IGN.
    unsafe {
        libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
    }
}

/// Whether `sig` can be caught or ignored at all.
pub fn catchable(sig: i32) -> bool {
    #[cfg(unix)]
    {
        sig != libc::SIGKILL && sig != libc::SIGSTOP
    }
    #[cfg(not(unix))]
    {
        let _ = sig;
        false
    }
}

#[cfg(unix)]
mod imp {
    use super::*;

    extern "C" fn on_signal(sig: libc::c_int) {
        // SAFETY (whole handler): only atomics, write(2) and errno, all async-signal-safe; errno
        // is restored so the interrupted code does not observe the handler's.
        unsafe {
            let errno = crate::errno::errno_location();
            let saved = *errno;
            if (0..128).contains(&sig) {
                PENDING[sig as usize / 64].fetch_or(1 << (sig % 64), Ordering::SeqCst);
                ANY_PENDING.store(true, Ordering::SeqCst);
            }
            for w in &WAKE {
                let fd = w.load(Ordering::SeqCst);
                if fd >= 0 {
                    let byte = sig as u8;
                    libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
                }
            }
            *errno = saved;
        }
    }

    fn os_err() -> FsError {
        std::io::Error::last_os_error().into()
    }

    fn of_handler(h: libc::sighandler_t) -> Disposition {
        let ours = on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t;
        match h {
            libc::SIG_DFL => Disposition::Default,
            libc::SIG_IGN => Disposition::Ignore,
            h if h == ours => Disposition::Catch,
            _ => Disposition::Foreign,
        }
    }

    pub fn disposition(sig: i32) -> R<Disposition> {
        // SAFETY: a null new action only queries the current one into `old`.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, std::ptr::null(), &mut old) != 0 {
                return Err(os_err());
            }
            Ok(of_handler(old.sa_sigaction))
        }
    }

    pub fn set_disposition(sig: i32, d: Disposition, restart: bool) -> R<Disposition> {
        let handler = match d {
            Disposition::Default => libc::SIG_DFL,
            Disposition::Ignore => libc::SIG_IGN,
            Disposition::Catch => on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t,
            Disposition::Foreign => return disposition(sig),
        };
        // SAFETY: a zeroed sigaction with a valid handler and an empty mask.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler;
            action.sa_flags = libc::SA_ONSTACK | if restart { libc::SA_RESTART } else { 0 };
            libc::sigemptyset(&mut action.sa_mask);
            let mut old: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, &action, &mut old) != 0 {
                return Err(os_err());
            }
            Ok(of_handler(old.sa_sigaction))
        }
    }

    pub fn set_restart(sig: i32, restart: bool) -> R<()> {
        // SAFETY: reads the current action and writes it back with only SA_RESTART changed.
        unsafe {
            let mut act: libc::sigaction = std::mem::zeroed();
            if libc::sigaction(sig, std::ptr::null(), &mut act) != 0 {
                return Err(os_err());
            }
            if restart {
                act.sa_flags |= libc::SA_RESTART;
            } else {
                act.sa_flags &= !libc::SA_RESTART;
            }
            if libc::sigaction(sig, &act, std::ptr::null_mut()) != 0 {
                return Err(os_err());
            }
        }
        Ok(())
    }

    fn to_set(sigs: &[i32]) -> R<libc::sigset_t> {
        // SAFETY: sigemptyset/sigaddset initialise and fill a local set.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            for &s in sigs {
                if libc::sigaddset(&mut set, s) != 0 {
                    return Err(os_err());
                }
            }
            Ok(set)
        }
    }

    fn of_set(set: &libc::sigset_t) -> Vec<i32> {
        // SAFETY: sigismember reads an initialised set.
        (1..NSIG)
            .filter(|&s| unsafe { libc::sigismember(set, s) } == 1)
            .collect()
    }

    pub fn valid_signals() -> Vec<i32> {
        // SAFETY: sigfillset initialises a local set.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigfillset(&mut set);
            of_set(&set)
        }
    }

    pub fn sigmask(how: i32, sigs: &[i32]) -> R<Vec<i32>> {
        let set = to_set(sigs)?;
        // SAFETY: both sets are valid locals.
        unsafe {
            let mut old: libc::sigset_t = std::mem::zeroed();
            let rc = libc::pthread_sigmask(how, &set, &mut old);
            if rc != 0 {
                return Err(std::io::Error::from_raw_os_error(rc).into());
            }
            Ok(of_set(&old))
        }
    }

    pub fn sigpending() -> R<Vec<i32>> {
        // SAFETY: sigpending fills a local set.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            if libc::sigpending(&mut set) != 0 {
                return Err(os_err());
            }
            Ok(of_set(&set))
        }
    }

    pub fn sigwait(sigs: &[i32]) -> R<i32> {
        let set = to_set(sigs)?;
        let mut sig: libc::c_int = 0;
        // SAFETY: valid set and output.
        let rc = unsafe { libc::sigwait(&set, &mut sig) };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc).into());
        }
        Ok(sig)
    }

    pub fn raise(sig: i32) -> R<()> {
        // SAFETY: raise(3) takes no pointers.
        if unsafe { libc::raise(sig) } != 0 {
            return Err(os_err());
        }
        Ok(())
    }

    pub fn pthread_kill(thread: u64, sig: i32) -> R<()> {
        // SAFETY: the kernel validates the thread handle.
        let rc = unsafe { libc::pthread_kill(thread as libc::pthread_t, sig) };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc).into());
        }
        Ok(())
    }

    pub fn alarm(secs: u32) -> u32 {
        // SAFETY: alarm(3) takes no pointers.
        unsafe { libc::alarm(secs) }
    }

    fn to_tv(secs: f64) -> libc::timeval {
        let whole = secs.trunc();
        let mut usec = ((secs - whole) * 1e6).round() as i64;
        let mut s = whole as i64;
        if usec >= 1_000_000 {
            s += 1;
            usec -= 1_000_000;
        }
        // CPython rounds a non-zero interval below one microsecond up to one.
        if s == 0 && usec == 0 && secs > 0.0 {
            usec = 1;
        }
        libc::timeval {
            tv_sec: s as libc::time_t,
            tv_usec: usec as libc::suseconds_t,
        }
    }

    fn of_tv(tv: &libc::timeval) -> f64 {
        tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6
    }

    pub fn setitimer(which: i32, value: f64, interval: f64) -> R<(f64, f64)> {
        let new = libc::itimerval {
            it_value: to_tv(value),
            it_interval: to_tv(interval),
        };
        // SAFETY: valid locals.
        unsafe {
            let mut old: libc::itimerval = std::mem::zeroed();
            if libc::setitimer(which as _, &new, &mut old) != 0 {
                return Err(os_err());
            }
            Ok((of_tv(&old.it_value), of_tv(&old.it_interval)))
        }
    }

    pub fn getitimer(which: i32) -> R<(f64, f64)> {
        // SAFETY: valid local.
        unsafe {
            let mut cur: libc::itimerval = std::mem::zeroed();
            if libc::getitimer(which as _, &mut cur) != 0 {
                return Err(os_err());
            }
            Ok((of_tv(&cur.it_value), of_tv(&cur.it_interval)))
        }
    }

    pub fn strsignal(sig: i32) -> Option<String> {
        if !(1..NSIG).contains(&sig) {
            return None;
        }
        // SAFETY: strsignal returns a static or thread-local string, copied at once.
        unsafe {
            let p = libc::strsignal(sig);
            if p.is_null() {
                return None;
            }
            Some(std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned())
        }
    }

    /// Restores the dispositions a runtime changes for itself (`SIGPIPE`, `SIGXFSZ` ignored) in
    /// a child about to exec. Async-signal-safe.
    pub fn restore_child_defaults() {
        // SAFETY: signal(3) with SIG_DFL.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::signal(libc::SIGXFSZ, libc::SIG_DFL);
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    fn unsupported<T>() -> R<T> {
        Err(FsError("ENOSYS"))
    }
    pub fn disposition(_sig: i32) -> R<Disposition> {
        Ok(Disposition::Default)
    }
    pub fn set_disposition(_sig: i32, _d: Disposition, _restart: bool) -> R<Disposition> {
        unsupported()
    }
    pub fn set_restart(_sig: i32, _restart: bool) -> R<()> {
        unsupported()
    }
    pub fn valid_signals() -> Vec<i32> {
        Vec::new()
    }
    pub fn sigmask(_how: i32, _sigs: &[i32]) -> R<Vec<i32>> {
        unsupported()
    }
    pub fn sigpending() -> R<Vec<i32>> {
        unsupported()
    }
    pub fn sigwait(_sigs: &[i32]) -> R<i32> {
        unsupported()
    }
    pub fn raise(_sig: i32) -> R<()> {
        unsupported()
    }
    pub fn pthread_kill(_thread: u64, _sig: i32) -> R<()> {
        unsupported()
    }
    pub fn alarm(_secs: u32) -> u32 {
        0
    }
    pub fn setitimer(_which: i32, _value: f64, _interval: f64) -> R<(f64, f64)> {
        unsupported()
    }
    pub fn getitimer(_which: i32) -> R<(f64, f64)> {
        unsupported()
    }
    pub fn strsignal(_sig: i32) -> Option<String> {
        None
    }
    pub fn restore_child_defaults() {}
}

pub use imp::*;
