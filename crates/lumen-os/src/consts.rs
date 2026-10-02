//! Named OS constants of this platform (`O_*`, `SEEK_*`, `W*`, `EX_*`, `_SC_*`, ...) as
//! `(name, value)` tables, so a runtime can expose them without depending on `libc` itself.

#[cfg(unix)]
macro_rules! libc_table {
    ($vis:vis $table:ident: $($(#[$m:meta])* $name:ident),* $(,)?) => {
        $vis static $table: &[(&str, i64)] = &[$($(#[$m])* (stringify!($name), libc::$name as i64),)*];
    };
}

/// `open(2)` flags.
pub fn open_flags() -> impl Iterator<Item = (&'static str, i64)> {
    unix::OPEN.iter().chain(system::OPEN.iter()).copied()
}

/// Everything else a POSIX module exposes: `SEEK_DATA`/`SEEK_HOLE`, `wait` options, `P_*`,
/// `PRIO_*`, `RTLD_*`, sysexits `EX_*`.
pub fn misc() -> impl Iterator<Item = (&'static str, i64)> {
    unix::MISC.iter().chain(SYSEXITS.iter()).chain(system::MISC.iter()).copied()
}

/// `sysconf` names (`SC_*`, as Python spells them) and their numbers.
pub fn sysconf_names() -> impl Iterator<Item = (&'static str, i64)> {
    system::SYSCONF.iter().map(|(n, v)| (n.strip_prefix('_').unwrap_or(n), *v))
}

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
        O_NDELAY, O_SYNC, O_DSYNC, O_NOCTTY, O_CLOEXEC, O_DIRECTORY, O_NOFOLLOW, O_ACCMODE, O_ASYNC,
        #[cfg(not(target_os = "android"))] O_FSYNC);
    libc_table!(pub(super) MISC: SEEK_DATA, SEEK_HOLE, WNOHANG, WUNTRACED, WCONTINUED, WEXITED, WSTOPPED, WNOWAIT,
        P_ALL, P_PID, P_PGID, PRIO_PROCESS, PRIO_PGRP, PRIO_USER, RTLD_LAZY, RTLD_NOW, RTLD_GLOBAL, RTLD_LOCAL,
        RTLD_NODELETE, RTLD_NOLOAD, F_LOCK, F_TLOCK, F_ULOCK, F_TEST);
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
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod system {
    libc_table!(pub(super) SYSCONF:
    _SC_2_CHAR_TERM,
    _SC_2_C_BIND,
    _SC_2_C_DEV,
    _SC_2_C_VERSION,
    _SC_2_FORT_DEV,
    _SC_2_FORT_RUN,
    _SC_2_LOCALEDEF,
    _SC_2_SW_DEV,
    _SC_2_UPE,
    _SC_2_VERSION,
    _SC_AIO_LISTIO_MAX,
    _SC_AIO_MAX,
    _SC_AIO_PRIO_DELTA_MAX,
    _SC_ARG_MAX,
    _SC_ASYNCHRONOUS_IO,
    _SC_ATEXIT_MAX,
    _SC_AVPHYS_PAGES,
    _SC_BC_BASE_MAX,
    _SC_BC_DIM_MAX,
    _SC_BC_SCALE_MAX,
    _SC_BC_STRING_MAX,
    _SC_CHARCLASS_NAME_MAX,
    _SC_CHAR_BIT,
    _SC_CHAR_MAX,
    _SC_CHAR_MIN,
    _SC_CHILD_MAX,
    _SC_CLK_TCK,
    _SC_COLL_WEIGHTS_MAX,
    _SC_DELAYTIMER_MAX,
    _SC_EQUIV_CLASS_MAX,
    _SC_EXPR_NEST_MAX,
    _SC_FSYNC,
    _SC_GETGR_R_SIZE_MAX,
    _SC_GETPW_R_SIZE_MAX,
    _SC_INT_MAX,
    _SC_INT_MIN,
    _SC_IOV_MAX,
    _SC_JOB_CONTROL,
    _SC_LINE_MAX,
    _SC_LOGIN_NAME_MAX,
    _SC_LONG_BIT,
    _SC_MAPPED_FILES,
    _SC_MB_LEN_MAX,
    _SC_MEMLOCK,
    _SC_MEMLOCK_RANGE,
    _SC_MEMORY_PROTECTION,
    _SC_MESSAGE_PASSING,
    _SC_MQ_OPEN_MAX,
    _SC_MQ_PRIO_MAX,
    _SC_NGROUPS_MAX,
    _SC_NL_ARGMAX,
    _SC_NL_LANGMAX,
    _SC_NL_MSGMAX,
    _SC_NL_NMAX,
    _SC_NL_SETMAX,
    _SC_NL_TEXTMAX,
    _SC_NPROCESSORS_CONF,
    _SC_NPROCESSORS_ONLN,
    _SC_NZERO,
    _SC_OPEN_MAX,
    _SC_PAGESIZE,
    _SC_PAGE_SIZE,
    _SC_PASS_MAX,
    _SC_PHYS_PAGES,
    _SC_PII,
    _SC_PII_INTERNET,
    _SC_PII_INTERNET_DGRAM,
    _SC_PII_INTERNET_STREAM,
    _SC_PII_OSI,
    _SC_PII_OSI_CLTS,
    _SC_PII_OSI_COTS,
    _SC_PII_OSI_M,
    _SC_PII_SOCKET,
    _SC_PII_XTI,
    _SC_POLL,
    _SC_PRIORITIZED_IO,
    _SC_PRIORITY_SCHEDULING,
    _SC_REALTIME_SIGNALS,
    _SC_RE_DUP_MAX,
    _SC_RTSIG_MAX,
    _SC_SAVED_IDS,
    _SC_SCHAR_MAX,
    _SC_SCHAR_MIN,
    _SC_SELECT,
    _SC_SEMAPHORES,
    _SC_SEM_NSEMS_MAX,
    _SC_SEM_VALUE_MAX,
    _SC_SHARED_MEMORY_OBJECTS,
    _SC_SHRT_MAX,
    _SC_SHRT_MIN,
    _SC_SIGQUEUE_MAX,
    _SC_SSIZE_MAX,
    _SC_STREAM_MAX,
    _SC_SYNCHRONIZED_IO,
    _SC_THREADS,
    _SC_THREAD_ATTR_STACKADDR,
    _SC_THREAD_ATTR_STACKSIZE,
    _SC_THREAD_DESTRUCTOR_ITERATIONS,
    _SC_THREAD_KEYS_MAX,
    _SC_THREAD_PRIORITY_SCHEDULING,
    _SC_THREAD_PRIO_INHERIT,
    _SC_THREAD_PRIO_PROTECT,
    _SC_THREAD_PROCESS_SHARED,
    _SC_THREAD_SAFE_FUNCTIONS,
    _SC_THREAD_STACK_MIN,
    _SC_THREAD_THREADS_MAX,
    _SC_TIMERS,
    _SC_TIMER_MAX,
    _SC_TTY_NAME_MAX,
    _SC_TZNAME_MAX,
    _SC_T_IOV_MAX,
    _SC_UCHAR_MAX,
    _SC_UINT_MAX,
    _SC_UIO_MAXIOV,
    _SC_ULONG_MAX,
    _SC_USHRT_MAX,
    _SC_VERSION,
    _SC_WORD_BIT,
    _SC_XBS5_ILP32_OFF32,
    _SC_XBS5_ILP32_OFFBIG,
    _SC_XBS5_LP64_OFF64,
    _SC_XBS5_LPBIG_OFFBIG,
    _SC_XOPEN_CRYPT,
    _SC_XOPEN_ENH_I18N,
    _SC_XOPEN_LEGACY,
    _SC_XOPEN_REALTIME,
    _SC_XOPEN_REALTIME_THREADS,
    _SC_XOPEN_SHM,
    _SC_XOPEN_UNIX,
    _SC_XOPEN_VERSION,
    _SC_XOPEN_XCU_VERSION,
    _SC_XOPEN_XPG2,
    _SC_XOPEN_XPG3,
    _SC_XOPEN_XPG4,
);

    libc_table!(pub(super) OPEN: O_DIRECT, O_LARGEFILE, O_NOATIME, O_PATH, O_TMPFILE, O_RSYNC);
    pub(super) static MISC: &[(&str, i64)] = &[];
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
mod system {
    pub(super) static SYSCONF: &[(&str, i64)] = &[
    ("SC_2_CHAR_TERM", 20),
    ("SC_2_C_BIND", 18),
    ("SC_2_C_DEV", 19),
    ("SC_2_FORT_DEV", 21),
    ("SC_2_FORT_RUN", 22),
    ("SC_2_LOCALEDEF", 23),
    ("SC_2_SW_DEV", 24),
    ("SC_2_UPE", 25),
    ("SC_2_VERSION", 17),
    ("SC_AIO_LISTIO_MAX", 42),
    ("SC_AIO_MAX", 43),
    ("SC_AIO_PRIO_DELTA_MAX", 44),
    ("SC_ARG_MAX", 1),
    ("SC_ASYNCHRONOUS_IO", 28),
    ("SC_ATEXIT_MAX", 107),
    ("SC_BC_BASE_MAX", 9),
    ("SC_BC_DIM_MAX", 10),
    ("SC_BC_SCALE_MAX", 11),
    ("SC_BC_STRING_MAX", 12),
    ("SC_CHILD_MAX", 2),
    ("SC_CLK_TCK", 3),
    ("SC_COLL_WEIGHTS_MAX", 13),
    ("SC_DELAYTIMER_MAX", 45),
    ("SC_EXPR_NEST_MAX", 14),
    ("SC_FSYNC", 38),
    ("SC_GETGR_R_SIZE_MAX", 70),
    ("SC_GETPW_R_SIZE_MAX", 71),
    ("SC_IOV_MAX", 56),
    ("SC_JOB_CONTROL", 6),
    ("SC_LINE_MAX", 15),
    ("SC_LOGIN_NAME_MAX", 73),
    ("SC_MAPPED_FILES", 47),
    ("SC_MEMLOCK", 30),
    ("SC_MEMLOCK_RANGE", 31),
    ("SC_MEMORY_PROTECTION", 32),
    ("SC_MESSAGE_PASSING", 33),
    ("SC_MQ_OPEN_MAX", 46),
    ("SC_MQ_PRIO_MAX", 75),
    ("SC_NGROUPS_MAX", 4),
    ("SC_NPROCESSORS_CONF", 57),
    ("SC_NPROCESSORS_ONLN", 58),
    ("SC_OPEN_MAX", 5),
    ("SC_PAGESIZE", 29),
    ("SC_PAGE_SIZE", 29),
    ("SC_PASS_MAX", 131),
    ("SC_PHYS_PAGES", 200),
    ("SC_PRIORITIZED_IO", 34),
    ("SC_PRIORITY_SCHEDULING", 35),
    ("SC_REALTIME_SIGNALS", 36),
    ("SC_RE_DUP_MAX", 16),
    ("SC_RTSIG_MAX", 48),
    ("SC_SAVED_IDS", 7),
    ("SC_SEMAPHORES", 37),
    ("SC_SEM_NSEMS_MAX", 49),
    ("SC_SEM_VALUE_MAX", 50),
    ("SC_SHARED_MEMORY_OBJECTS", 39),
    ("SC_SIGQUEUE_MAX", 51),
    ("SC_STREAM_MAX", 26),
    ("SC_SYNCHRONIZED_IO", 40),
    ("SC_THREADS", 96),
    ("SC_THREAD_ATTR_STACKADDR", 82),
    ("SC_THREAD_ATTR_STACKSIZE", 83),
    ("SC_THREAD_DESTRUCTOR_ITERATIONS", 85),
    ("SC_THREAD_KEYS_MAX", 86),
    ("SC_THREAD_PRIORITY_SCHEDULING", 89),
    ("SC_THREAD_PRIO_INHERIT", 87),
    ("SC_THREAD_PRIO_PROTECT", 88),
    ("SC_THREAD_PROCESS_SHARED", 90),
    ("SC_THREAD_SAFE_FUNCTIONS", 91),
    ("SC_THREAD_STACK_MIN", 93),
    ("SC_THREAD_THREADS_MAX", 94),
    ("SC_TIMERS", 41),
    ("SC_TIMER_MAX", 52),
    ("SC_TTY_NAME_MAX", 101),
    ("SC_TZNAME_MAX", 27),
    ("SC_VERSION", 8),
    ("SC_XBS5_ILP32_OFF32", 122),
    ("SC_XBS5_ILP32_OFFBIG", 123),
    ("SC_XBS5_LP64_OFF64", 124),
    ("SC_XBS5_LPBIG_OFFBIG", 125),
    ("SC_XOPEN_CRYPT", 108),
    ("SC_XOPEN_ENH_I18N", 109),
    ("SC_XOPEN_LEGACY", 110),
    ("SC_XOPEN_REALTIME", 111),
    ("SC_XOPEN_REALTIME_THREADS", 112),
    ("SC_XOPEN_SHM", 113),
    ("SC_XOPEN_UNIX", 115),
    ("SC_XOPEN_VERSION", 116),
    ("SC_XOPEN_XCU_VERSION", 121),
    ];

    libc_table!(pub(super) OPEN: O_SHLOCK, O_EXLOCK, O_EVTONLY, O_SYMLINK, O_EXEC, O_SEARCH, O_NOFOLLOW_ANY);
    libc_table!(pub(super) MISC: PRIO_DARWIN_THREAD, PRIO_DARWIN_PROCESS, PRIO_DARWIN_BG, PRIO_DARWIN_NONUI);
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios")))]
mod system {
    pub(super) static OPEN: &[(&str, i64)] = &[];
    pub(super) static MISC: &[(&str, i64)] = &[];
    pub(super) static SYSCONF: &[(&str, i64)] = &[];
}
