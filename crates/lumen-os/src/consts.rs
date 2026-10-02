//! Named OS constants of this platform (`O_*`, `SEEK_*`, `W*`, `EX_*`, `_SC_*`, ...) as
//! `(name, value)` tables, so a runtime can expose them without depending on `libc` itself.

#[cfg(unix)]
macro_rules! libc_table {
    ($vis:vis $table:ident: $($name:ident),* $(,)?) => {
        $vis static $table: &[(&str, i64)] = &[$((stringify!($name), libc::$name as i64),)*];
    };
}

/// `open(2)` flags.
pub fn open_flags() -> impl Iterator<Item = (&'static str, i64)> {
    unix::OPEN.iter().chain(system::OPEN.iter()).copied()
}

/// Everything else a POSIX module exposes: `SEEK_DATA`/`SEEK_HOLE`, `wait` options, `P_*`,
/// `PRIO_*`, `RTLD_*`, sysexits `EX_*`.
pub fn misc() -> impl Iterator<Item = (&'static str, i64)> {
    unix::MISC.iter().chain(SYSEXITS.iter()).copied()
}

/// `sysconf` names (without the leading underscore, as Python spells them) and their numbers.
pub fn sysconf_names() -> impl Iterator<Item = (&'static str, i64)> {
    unix::SYSCONF.iter().map(|(n, v)| (&n[1..], *v))
}

/// The signals of this platform by name (Windows: the C runtime's numbers libuv emulates).
pub fn signals() -> &'static [(&'static str, i32)] {
    SIGNALS
}

/// The number of signal `name` (`"SIGTERM"`) on this platform.
pub fn signal_number(name: &str) -> Option<i32> {
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
    SIGHUP, SIGINT, SIGQUIT, SIGILL, SIGTRAP, SIGABRT, SIGIOT, SIGBUS, SIGFPE, SIGKILL, SIGUSR1,
    SIGSEGV, SIGUSR2, SIGPIPE, SIGALRM, SIGTERM, SIGCHLD, SIGCONT, SIGSTOP, SIGTSTP, SIGTTIN,
    SIGTTOU, SIGURG, SIGXCPU, SIGXFSZ, SIGVTALRM, SIGPROF, SIGWINCH, SIGIO, SIGSYS,
    #[cfg(any(target_os = "linux", target_os = "android"))] SIGSTKFLT,
    #[cfg(any(target_os = "linux", target_os = "android"))] SIGPOLL,
    #[cfg(any(target_os = "linux", target_os = "android"))] SIGPWR,
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))] SIGEMT,
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))] SIGINFO,
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

static SYSEXITS: &[(&str, i64)] = &[
    ("EX_OK", 0),
    ("EX_USAGE", 64),
    ("EX_DATAERR", 65),
    ("EX_NOINPUT", 66),
    ("EX_NOUSER", 67),
    ("EX_NOHOST", 68),
    ("EX_UNAVAILABLE", 69),
    ("EX_SOFTWARE", 70),
    ("EX_OSERR", 71),
    ("EX_OSFILE", 72),
    ("EX_CANTCREAT", 73),
    ("EX_IOERR", 74),
    ("EX_TEMPFAIL", 75),
    ("EX_PROTOCOL", 76),
    ("EX_NOPERM", 77),
    ("EX_CONFIG", 78),
];

#[cfg(unix)]
mod unix {
    libc_table!(pub(super) OPEN: O_RDONLY, O_WRONLY, O_RDWR, O_APPEND, O_CREAT, O_EXCL, O_TRUNC, O_NONBLOCK,
        O_NDELAY, O_SYNC, O_DSYNC, O_NOCTTY, O_CLOEXEC, O_DIRECTORY, O_NOFOLLOW, O_ACCMODE, O_ASYNC, O_FSYNC);
    libc_table!(pub(super) MISC: SEEK_DATA, SEEK_HOLE, WNOHANG, WUNTRACED, WCONTINUED, WEXITED, WSTOPPED, WNOWAIT,
        P_ALL, P_PID, P_PGID, PRIO_PROCESS, PRIO_PGRP, PRIO_USER, RTLD_LAZY, RTLD_NOW, RTLD_GLOBAL, RTLD_LOCAL,
        RTLD_NODELETE, RTLD_NOLOAD, F_LOCK, F_TLOCK, F_ULOCK, F_TEST);
    libc_table!(pub(super) SYSCONF: _SC_ARG_MAX, _SC_CHILD_MAX, _SC_CLK_TCK, _SC_NGROUPS_MAX, _SC_OPEN_MAX,
        _SC_PAGESIZE, _SC_PAGE_SIZE, _SC_NPROCESSORS_CONF, _SC_NPROCESSORS_ONLN, _SC_PHYS_PAGES);
}

#[cfg(not(unix))]
mod unix {
    pub(super) static OPEN: &[(&str, i64)] = &[
        ("O_RDONLY", 0),
        ("O_WRONLY", crate::fs::flags::O_WRONLY as i64),
        ("O_RDWR", crate::fs::flags::O_RDWR as i64),
        ("O_APPEND", crate::fs::flags::O_APPEND as i64),
        ("O_CREAT", crate::fs::flags::O_CREAT as i64),
        ("O_EXCL", crate::fs::flags::O_EXCL as i64),
        ("O_TRUNC", crate::fs::flags::O_TRUNC as i64),
    ];
    pub(super) static MISC: &[(&str, i64)] = &[];
    pub(super) static SYSCONF: &[(&str, i64)] = &[];
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod system {
    libc_table!(pub(super) OPEN: O_DIRECT, O_LARGEFILE, O_NOATIME, O_PATH, O_TMPFILE, O_RSYNC);
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod system {
    libc_table!(pub(super) OPEN: O_SHLOCK, O_EXLOCK, O_EVTONLY, O_SYMLINK);
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios")))]
mod system {
    pub(super) static OPEN: &[(&str, i64)] = &[];
}
