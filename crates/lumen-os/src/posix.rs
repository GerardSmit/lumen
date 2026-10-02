//! POSIX calls behind Python's `posix` module that the other modules do not cover: credentials,
//! priorities, `*conf`, `statvfs`, device nodes, vectored and zero-copy I/O, `exec*`, scheduling
//! and the Linux-only descriptors (`eventfd`, `timerfd`, `pidfd_open`, ...). Plain arguments and
//! results; errors are carried as [`FsError`]. Calls a platform lacks fail with `ENOSYS`.

use crate::errno::FsError;

pub type R<T> = Result<T, FsError>;

macro_rules! sys {
    ($(#[$m:meta])* pub fn $name:ident($($a:ident: $t:ty),* $(,)?) -> R<$r:ty> $body:block) => {
        $(#[$m])*
        pub fn $name($($a: $t),*) -> R<$r> {
            #[cfg(unix)]
            {
                $body
            }
            #[cfg(not(unix))]
            {
                $(let _ = $a;)*
                Err(FsError("ENOSYS"))
            }
        }
    };
}

macro_rules! linux {
    ($(#[$m:meta])* pub fn $name:ident($($a:ident: $t:ty),* $(,)?) -> R<$r:ty> $body:block) => {
        $(#[$m])*
        pub fn $name($($a: $t),*) -> R<$r> {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            {
                $body
            }
            #[cfg(not(any(target_os = "linux", target_os = "android")))]
            {
                $(let _ = $a;)*
                Err(FsError("ENOSYS"))
            }
        }
    };
}

macro_rules! apple {
    ($(#[$m:meta])* pub fn $name:ident($($a:ident: $t:ty),* $(,)?) -> R<$r:ty> $body:block) => {
        $(#[$m])*
        pub fn $name($($a: $t),*) -> R<$r> {
            #[cfg(target_vendor = "apple")]
            {
                $body
            }
            #[cfg(not(target_vendor = "apple"))]
            {
                $(let _ = $a;)*
                Err(FsError("ENOSYS"))
            }
        }
    };
}

#[cfg(unix)]
fn last() -> FsError {
    std::io::Error::last_os_error().into()
}

#[cfg(unix)]
fn ck(rc: libc::c_int) -> R<libc::c_int> {
    if rc < 0 {
        Err(last())
    } else {
        Ok(rc)
    }
}

#[cfg(unix)]
fn ck_unit(rc: libc::c_int) -> R<()> {
    ck(rc).map(|_| ())
}

#[cfg(unix)]
fn ck_size(rc: isize) -> R<usize> {
    if rc < 0 {
        Err(last())
    } else {
        Ok(rc as usize)
    }
}

#[cfg(unix)]
fn cstr(s: &str) -> R<std::ffi::CString> {
    std::ffi::CString::new(s).map_err(|_| FsError("EINVAL"))
}

#[cfg(unix)]
fn cstr_bytes(s: &[u8]) -> R<std::ffi::CString> {
    std::ffi::CString::new(s).map_err(|_| FsError("EINVAL"))
}

/// Clears `errno`, for the calls whose `-1` result is ambiguous.
#[cfg(unix)]
fn clear_errno() {
    // SAFETY: the errno location is thread-local storage owned by libc.
    unsafe {
        #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
        {
            *libc::__error() = 0;
        }
        #[cfg(any(target_os = "linux", target_os = "emscripten"))]
        {
            *libc::__errno_location() = 0;
        }
        #[cfg(target_os = "android")]
        {
            *libc::__errno() = 0;
        }
    }
}

#[cfg(unix)]
fn errno_is_set() -> bool {
    std::io::Error::last_os_error().raw_os_error().is_some_and(|e| e != 0)
}

#[cfg(unix)]
fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

// ---- descriptors and paths ----------------------------------------------------------------------

sys! {
    /// `fchdir(2)`.
    pub fn fchdir(fd: i32) -> R<()> {
        // SAFETY: plain integer argument.
        ck_unit(unsafe { libc::fchdir(fd) })
    }
}

sys! {
    /// `fchown(2)`.
    pub fn fchown(fd: i32, uid: u32, gid: u32) -> R<()> {
        // SAFETY: plain integer arguments.
        ck_unit(unsafe { libc::fchown(fd, uid as _, gid as _) })
    }
}

sys! {
    /// `chroot(2)`.
    pub fn chroot(path: &str) -> R<()> {
        let p = cstr(path)?;
        // SAFETY: `p` is a valid NUL-terminated string.
        ck_unit(unsafe { libc::chroot(p.as_ptr()) })
    }
}

apple! {
    /// `chflags(2)`, or `lchflags(2)` when `follow` is false.
    pub fn chflags(path: &str, flags: u32, follow: bool) -> R<()> {
        extern "C" {
            fn lchflags(path: *const libc::c_char, flags: libc::c_uint) -> libc::c_int;
        }
        let p = cstr(path)?;
        // SAFETY: `p` is a valid NUL-terminated string.
        ck_unit(unsafe { if follow { libc::chflags(p.as_ptr(), flags as _) } else { lchflags(p.as_ptr(), flags as _) } })
    }
}

apple! {
    /// `lchmod(2)`.
    pub fn lchmod(path: &str, mode: u32) -> R<()> {
        extern "C" {
            fn lchmod(path: *const libc::c_char, mode: libc::mode_t) -> libc::c_int;
        }
        let p = cstr(path)?;
        // SAFETY: `p` is a valid NUL-terminated string.
        ck_unit(unsafe { lchmod(p.as_ptr(), mode as _) })
    }
}

sys! {
    /// `mkfifo(3)`.
    pub fn mkfifo(path: &str, mode: u32) -> R<()> {
        let p = cstr(path)?;
        // SAFETY: `p` is a valid NUL-terminated string.
        ck_unit(unsafe { libc::mkfifo(p.as_ptr(), mode as _) })
    }
}

sys! {
    /// `mknod(2)`.
    pub fn mknod(path: &str, mode: u32, device: u64) -> R<()> {
        let p = cstr(path)?;
        // SAFETY: `p` is a valid NUL-terminated string.
        ck_unit(unsafe { libc::mknod(p.as_ptr(), mode as _, device as _) })
    }
}

/// The major number of a device number.
pub fn major(device: u64) -> u32 {
    #[cfg(unix)]
    {
        libc::major(device as _) as u32
    }
    #[cfg(not(unix))]
    {
        (device >> 8) as u32 & 0xfff
    }
}

/// The minor number of a device number.
pub fn minor(device: u64) -> u32 {
    #[cfg(unix)]
    {
        libc::minor(device as _) as u32
    }
    #[cfg(not(unix))]
    {
        device as u32 & 0xff
    }
}

/// A device number from its major and minor parts.
pub fn makedev(major: u32, minor: u32) -> u64 {
    #[cfg(unix)]
    {
        libc::makedev(major as _, minor as _) as u64
    }
    #[cfg(not(unix))]
    {
        ((major as u64) << 8) | minor as u64
    }
}

sys! {
    /// `sync(2)`.
    pub fn sync() -> R<()> {
        // SAFETY: no arguments.
        unsafe { libc::sync() };
        Ok(())
    }
}

sys! {
    /// `lockf(3)`.
    pub fn lockf(fd: i32, command: i32, length: i64) -> R<()> {
        // SAFETY: plain integer arguments.
        ck_unit(unsafe { libc::lockf(fd, command, length as _) })
    }
}

// ---- statvfs ------------------------------------------------------------------------------------

/// A `struct statvfs`.
#[derive(Clone, Copy, Debug, Default)]
pub struct StatVfs {
    pub bsize: u64,
    pub frsize: u64,
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub favail: u64,
    pub flag: u64,
    pub namemax: u64,
    pub fsid: u64,
}

#[cfg(unix)]
fn statvfs_of(s: &libc::statvfs) -> StatVfs {
    StatVfs {
        bsize: s.f_bsize as u64,
        frsize: s.f_frsize as u64,
        blocks: s.f_blocks as u64,
        bfree: s.f_bfree as u64,
        bavail: s.f_bavail as u64,
        files: s.f_files as u64,
        ffree: s.f_ffree as u64,
        favail: s.f_favail as u64,
        flag: s.f_flag as u64,
        namemax: s.f_namemax as u64,
        fsid: s.f_fsid as u64,
    }
}

sys! {
    /// `statvfs(3)`.
    pub fn statvfs(path: &str) -> R<StatVfs> {
        let p = cstr(path)?;
        // SAFETY: a zeroed statvfs is a valid out-parameter; `p` is NUL-terminated.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        ck_unit(unsafe { libc::statvfs(p.as_ptr(), &mut s) })?;
        Ok(statvfs_of(&s))
    }
}

sys! {
    /// `fstatvfs(3)`.
    pub fn fstatvfs(fd: i32) -> R<StatVfs> {
        // SAFETY: a zeroed statvfs is a valid out-parameter.
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        ck_unit(unsafe { libc::fstatvfs(fd, &mut s) })?;
        Ok(statvfs_of(&s))
    }
}

/// The `ST_*` flag bits of `statvfs` on this platform.
pub fn statvfs_flags() -> Vec<(&'static str, i64)> {
    #[allow(unused_mut)]
    let mut v = vec![("ST_RDONLY", 1), ("ST_NOSUID", 2)];
    #[cfg(any(target_os = "linux", target_os = "android"))]
    v.extend([
        ("ST_NODEV", 4),
        ("ST_NOEXEC", 8),
        ("ST_SYNCHRONOUS", 16),
        ("ST_MANDLOCK", 64),
        ("ST_WRITE", 128),
        ("ST_APPEND", 256),
        ("ST_NOATIME", 1024),
        ("ST_NODIRATIME", 2048),
        ("ST_RELATIME", 4096),
    ]);
    v
}

// ---- credentials --------------------------------------------------------------------------------

macro_rules! id_setter {
    ($(#[$m:meta])* $name:ident, $call:ident) => {
        sys! {
            $(#[$m])*
            pub fn $name(id: u32) -> R<()> {
                // SAFETY: plain integer argument.
                ck_unit(unsafe { libc::$call(id as _) })
            }
        }
    };
}

id_setter!(
    /// `setuid(2)`.
    setuid, setuid
);
id_setter!(
    /// `seteuid(2)`.
    seteuid, seteuid
);
id_setter!(
    /// `setgid(2)`.
    setgid, setgid
);
id_setter!(
    /// `setegid(2)`.
    setegid, setegid
);

sys! {
    /// `setreuid(2)`; `u32::MAX` leaves an id unchanged.
    pub fn setreuid(ruid: u32, euid: u32) -> R<()> {
        // SAFETY: plain integer arguments.
        ck_unit(unsafe { libc::setreuid(ruid as _, euid as _) })
    }
}

sys! {
    /// `setregid(2)`; `u32::MAX` leaves an id unchanged.
    pub fn setregid(rgid: u32, egid: u32) -> R<()> {
        // SAFETY: plain integer arguments.
        ck_unit(unsafe { libc::setregid(rgid as _, egid as _) })
    }
}

linux! {
    /// `setresuid(2)`.
    pub fn setresuid(r: u32, e: u32, s: u32) -> R<()> {
        // SAFETY: plain integer arguments.
        ck_unit(unsafe { libc::setresuid(r, e, s) })
    }
}

linux! {
    /// `setresgid(2)`.
    pub fn setresgid(r: u32, e: u32, s: u32) -> R<()> {
        // SAFETY: plain integer arguments.
        ck_unit(unsafe { libc::setresgid(r, e, s) })
    }
}

linux! {
    /// `getresuid(2)`: `(real, effective, saved)`.
    pub fn getresuid() -> R<[u32; 3]> {
        let (mut r, mut e, mut s) = (0, 0, 0);
        // SAFETY: three valid out-parameters.
        ck_unit(unsafe { libc::getresuid(&mut r, &mut e, &mut s) })?;
        Ok([r, e, s])
    }
}

linux! {
    /// `getresgid(2)`: `(real, effective, saved)`.
    pub fn getresgid() -> R<[u32; 3]> {
        let (mut r, mut e, mut s) = (0, 0, 0);
        // SAFETY: three valid out-parameters.
        ck_unit(unsafe { libc::getresgid(&mut r, &mut e, &mut s) })?;
        Ok([r, e, s])
    }
}

sys! {
    /// `setgroups(2)`.
    pub fn setgroups(groups: &[u32]) -> R<()> {
        let list: Vec<libc::gid_t> = groups.iter().map(|&g| g as libc::gid_t).collect();
        // SAFETY: `list` is a live array of `list.len()` group ids.
        ck_unit(unsafe { libc::setgroups(list.len() as _, list.as_ptr()) })
    }
}

sys! {
    /// `initgroups(3)`.
    pub fn initgroups(user: &str, gid: u32) -> R<()> {
        let u = cstr(user)?;
        // SAFETY: `u` is a valid NUL-terminated string.
        ck_unit(unsafe { libc::initgroups(u.as_ptr(), gid as _) })
    }
}

sys! {
    /// `getgrouplist(3)`: the groups `user` belongs to, including `group`.
    pub fn getgrouplist(user: &str, group: u32) -> R<Vec<u32>> {
        #[cfg(target_vendor = "apple")]
        type G = libc::c_int;
        #[cfg(not(target_vendor = "apple"))]
        type G = libc::gid_t;
        let u = cstr(user)?;
        let mut n: libc::c_int = 16;
        loop {
            let mut buf: Vec<G> = vec![0; n as usize];
            let mut count = n;
            // SAFETY: `buf` holds `count` entries; `u` is NUL-terminated.
            let r = unsafe { libc::getgrouplist(u.as_ptr(), group as _, buf.as_mut_ptr(), &mut count) };
            if r >= 0 {
                buf.truncate(count as usize);
                return Ok(buf.into_iter().map(|g| g as u32).collect());
            }
            n = if count > n { count } else { n.saturating_mul(2) };
            if n > 1 << 20 {
                return Err(FsError("EINVAL"));
            }
        }
    }
}

sys! {
    /// `setpgid(0, 0)`, as `os.setpgrp`.
    pub fn setpgrp() -> R<()> {
        // SAFETY: plain integer arguments.
        ck_unit(unsafe { libc::setpgid(0, 0) })
    }
}

sys! {
    /// `getpriority(2)`.
    pub fn getpriority(which: i32, who: u32) -> R<i32> {
        clear_errno();
        // SAFETY: plain integer arguments.
        let r = unsafe { libc::getpriority(which as _, who as _) };
        if r == -1 && errno_is_set() {
            return Err(last());
        }
        Ok(r)
    }
}

sys! {
    /// `setpriority(2)`.
    pub fn setpriority(which: i32, who: u32, priority: i32) -> R<()> {
        // SAFETY: plain integer arguments.
        ck_unit(unsafe { libc::setpriority(which as _, who as _, priority) })
    }
}

sys! {
    /// `nice(3)`: the new niceness.
    pub fn nice(increment: i32) -> R<i32> {
        clear_errno();
        // SAFETY: plain integer argument.
        let r = unsafe { libc::nice(increment) };
        if r == -1 && errno_is_set() {
            return Err(last());
        }
        Ok(r)
    }
}

/// `getloadavg(3)`; `None` when the load averages are unobtainable.
pub fn getloadavg() -> Option<[f64; 3]> {
    #[cfg(unix)]
    {
        let mut a = [0f64; 3];
        // SAFETY: `a` has room for three samples.
        if unsafe { libc::getloadavg(a.as_mut_ptr(), 3) } != 3 {
            return None;
        }
        Some(a)
    }
    #[cfg(not(unix))]
    None
}

// ---- configuration values -----------------------------------------------------------------------

sys! {
    /// `confstr(3)`; `None` when the variable has no value.
    pub fn confstr(name: i32) -> R<Option<String>> {
        clear_errno();
        let mut buf = vec![0u8; 256];
        loop {
            // SAFETY: `buf` is a live buffer of the length passed.
            let n = unsafe { libc::confstr(name, buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
            if n == 0 {
                return if errno_is_set() { Err(last()) } else { Ok(None) };
            }
            if n > buf.len() {
                buf = vec![0u8; n];
                continue;
            }
            return Ok(Some(text(&buf[..n - 1])));
        }
    }
}

sys! {
    /// `pathconf(3)`; `-1` for no limit.
    pub fn pathconf(path: &str, name: i32) -> R<i64> {
        let p = cstr(path)?;
        clear_errno();
        // SAFETY: `p` is a valid NUL-terminated string.
        let r = unsafe { libc::pathconf(p.as_ptr(), name) };
        if r == -1 && errno_is_set() {
            return Err(last());
        }
        Ok(r as i64)
    }
}

sys! {
    /// `fpathconf(3)`; `-1` for no limit.
    pub fn fpathconf(fd: i32, name: i32) -> R<i64> {
        clear_errno();
        // SAFETY: plain integer arguments.
        let r = unsafe { libc::fpathconf(fd, name) };
        if r == -1 && errno_is_set() {
            return Err(last());
        }
        Ok(r as i64)
    }
}

/// `pathconf` names (without the leading underscore, as Python spells them) and their numbers.
pub fn pathconf_names() -> Vec<(&'static str, i64)> {
    #[cfg(target_vendor = "apple")]
    {
        vec![
            ("PC_ASYNC_IO", 17),
            ("PC_CHOWN_RESTRICTED", 7),
            ("PC_FILESIZEBITS", 18),
            ("PC_LINK_MAX", 1),
            ("PC_MAX_CANON", 2),
            ("PC_MAX_INPUT", 3),
            ("PC_NAME_MAX", 4),
            ("PC_NO_TRUNC", 8),
            ("PC_PATH_MAX", 5),
            ("PC_PIPE_BUF", 6),
            ("PC_PRIO_IO", 19),
            ("PC_SYNC_IO", 25),
            ("PC_VDISABLE", 9),
            ("PC_MIN_HOLE_SIZE", 27),
            ("PC_ALLOC_SIZE_MIN", 16),
            ("PC_REC_INCR_XFER_SIZE", 20),
            ("PC_REC_MAX_XFER_SIZE", 21),
            ("PC_REC_MIN_XFER_SIZE", 22),
            ("PC_REC_XFER_ALIGN", 23),
            ("PC_SYMLINK_MAX", 24),
        ]
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        vec![
            ("PC_LINK_MAX", 0),
            ("PC_MAX_CANON", 1),
            ("PC_MAX_INPUT", 2),
            ("PC_NAME_MAX", 3),
            ("PC_PATH_MAX", 4),
            ("PC_PIPE_BUF", 5),
            ("PC_CHOWN_RESTRICTED", 6),
            ("PC_NO_TRUNC", 7),
            ("PC_VDISABLE", 8),
            ("PC_SYNC_IO", 9),
            ("PC_ASYNC_IO", 10),
            ("PC_PRIO_IO", 11),
            ("PC_SOCK_MAXBUF", 12),
            ("PC_FILESIZEBITS", 13),
            ("PC_REC_INCR_XFER_SIZE", 14),
            ("PC_REC_MAX_XFER_SIZE", 15),
            ("PC_REC_MIN_XFER_SIZE", 16),
            ("PC_REC_XFER_ALIGN", 17),
            ("PC_ALLOC_SIZE_MIN", 18),
            ("PC_SYMLINK_MAX", 19),
            ("PC_2_SYMLINKS", 20),
        ]
    }
}

/// `confstr` names (`CS_*`) and their numbers.
pub fn confstr_names() -> Vec<(&'static str, i64)> {
    #[cfg(target_vendor = "apple")]
    {
        vec![
            ("CS_PATH", 1),
            ("CS_XBS5_ILP32_OFF32_CFLAGS", 20),
            ("CS_XBS5_ILP32_OFF32_LDFLAGS", 21),
            ("CS_XBS5_ILP32_OFF32_LIBS", 22),
            ("CS_XBS5_ILP32_OFF32_LINTFLAGS", 23),
            ("CS_XBS5_ILP32_OFFBIG_CFLAGS", 24),
            ("CS_XBS5_ILP32_OFFBIG_LDFLAGS", 25),
            ("CS_XBS5_ILP32_OFFBIG_LIBS", 26),
            ("CS_XBS5_ILP32_OFFBIG_LINTFLAGS", 27),
            ("CS_XBS5_LP64_OFF64_CFLAGS", 28),
            ("CS_XBS5_LP64_OFF64_LDFLAGS", 29),
            ("CS_XBS5_LP64_OFF64_LIBS", 30),
            ("CS_XBS5_LP64_OFF64_LINTFLAGS", 31),
            ("CS_XBS5_LPBIG_OFFBIG_CFLAGS", 32),
            ("CS_XBS5_LPBIG_OFFBIG_LDFLAGS", 33),
            ("CS_XBS5_LPBIG_OFFBIG_LIBS", 34),
            ("CS_XBS5_LPBIG_OFFBIG_LINTFLAGS", 35),
        ]
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        vec![
            ("CS_PATH", 0),
            ("CS_V6_WIDTH_RESTRICTED_ENVS", 1),
            ("CS_GNU_LIBC_VERSION", 2),
            ("CS_GNU_LIBPTHREAD_VERSION", 3),
            ("CS_V5_WIDTH_RESTRICTED_ENVS", 4),
            ("CS_V7_WIDTH_RESTRICTED_ENVS", 5),
        ]
    }
}

/// `NGROUPS_MAX` and `TMP_MAX`.
pub fn limits() -> Vec<(&'static str, i64)> {
    #[cfg(target_vendor = "apple")]
    {
        vec![("NGROUPS_MAX", 16), ("TMP_MAX", 308_915_776)]
    }
    #[cfg(all(unix, not(target_vendor = "apple")))]
    {
        vec![("NGROUPS_MAX", 65536), ("TMP_MAX", 238_328)]
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

// ---- scheduling ---------------------------------------------------------------------------------

sys! {
    /// `sched_yield(2)`.
    pub fn sched_yield() -> R<()> {
        // SAFETY: no arguments.
        ck_unit(unsafe { libc::sched_yield() })
    }
}

sys! {
    /// `sched_get_priority_max(2)`.
    pub fn sched_get_priority_max(policy: i32) -> R<i32> {
        // SAFETY: plain integer argument.
        ck(unsafe { libc::sched_get_priority_max(policy) })
    }
}

sys! {
    /// `sched_get_priority_min(2)`.
    pub fn sched_get_priority_min(policy: i32) -> R<i32> {
        // SAFETY: plain integer argument.
        ck(unsafe { libc::sched_get_priority_min(policy) })
    }
}

/// `SCHED_*` policies.
pub fn sched_policies() -> Vec<(&'static str, i64)> {
    #[cfg(unix)]
    {
        #[allow(unused_mut)]
        let mut v = vec![
            ("SCHED_OTHER", libc::SCHED_OTHER as i64),
            ("SCHED_FIFO", libc::SCHED_FIFO as i64),
            ("SCHED_RR", libc::SCHED_RR as i64),
        ];
        #[cfg(any(target_os = "linux", target_os = "android"))]
        v.extend([
            ("SCHED_BATCH", libc::SCHED_BATCH as i64),
            ("SCHED_IDLE", libc::SCHED_IDLE as i64),
            ("SCHED_RESET_ON_FORK", libc::SCHED_RESET_ON_FORK as i64),
        ]);
        v
    }
    #[cfg(not(unix))]
    {
        Vec::new()
    }
}

linux! {
    /// `sched_getaffinity(2)`: the CPUs `pid` may run on.
    pub fn sched_getaffinity(pid: i32) -> R<Vec<usize>> {
        // SAFETY: a zeroed cpu_set_t is a valid out-parameter.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        ck_unit(unsafe { libc::sched_getaffinity(pid, std::mem::size_of::<libc::cpu_set_t>(), &mut set) })?;
        Ok((0..libc::CPU_SETSIZE as usize).filter(|&c| unsafe { libc::CPU_ISSET(c, &set) }).collect())
    }
}

linux! {
    /// `sched_setaffinity(2)`.
    pub fn sched_setaffinity(pid: i32, cpus: &[usize]) -> R<()> {
        // SAFETY: a zeroed cpu_set_t is a valid value; each CPU is range-checked.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        for &c in cpus {
            if c >= libc::CPU_SETSIZE as usize {
                return Err(FsError("EINVAL"));
            }
            unsafe { libc::CPU_SET(c, &mut set) };
        }
        ck_unit(unsafe { libc::sched_setaffinity(pid, std::mem::size_of::<libc::cpu_set_t>(), &set) })
    }
}

linux! {
    /// `sched_getscheduler(2)`.
    pub fn sched_getscheduler(pid: i32) -> R<i32> {
        // SAFETY: plain integer argument.
        ck(unsafe { libc::sched_getscheduler(pid) })
    }
}

linux! {
    /// `sched_setscheduler(2)`.
    pub fn sched_setscheduler(pid: i32, policy: i32, priority: i32) -> R<()> {
        let p = libc::sched_param { sched_priority: priority };
        // SAFETY: `p` is a live sched_param.
        ck_unit(unsafe { libc::sched_setscheduler(pid, policy, &p) })
    }
}

linux! {
    /// `sched_getparam(2)`: the priority.
    pub fn sched_getparam(pid: i32) -> R<i32> {
        let mut p = libc::sched_param { sched_priority: 0 };
        // SAFETY: `p` is a live out-parameter.
        ck_unit(unsafe { libc::sched_getparam(pid, &mut p) })?;
        Ok(p.sched_priority)
    }
}

linux! {
    /// `sched_setparam(2)`.
    pub fn sched_setparam(pid: i32, priority: i32) -> R<()> {
        let p = libc::sched_param { sched_priority: priority };
        // SAFETY: `p` is a live sched_param.
        ck_unit(unsafe { libc::sched_setparam(pid, &p) })
    }
}

linux! {
    /// `sched_rr_get_interval(2)` in seconds.
    pub fn sched_rr_get_interval(pid: i32) -> R<f64> {
        let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: `t` is a live out-parameter.
        ck_unit(unsafe { libc::sched_rr_get_interval(pid, &mut t) })?;
        Ok(t.tv_sec as f64 + t.tv_nsec as f64 * 1e-9)
    }
}

// ---- vectored and zero-copy I/O -----------------------------------------------------------------

#[cfg(unix)]
fn iovecs_mut(bufs: &mut [Vec<u8>]) -> Vec<libc::iovec> {
    bufs.iter_mut().map(|b| libc::iovec { iov_base: b.as_mut_ptr() as *mut libc::c_void, iov_len: b.len() }).collect()
}

#[cfg(unix)]
fn iovecs(bufs: &[&[u8]]) -> Vec<libc::iovec> {
    bufs.iter().map(|b| libc::iovec { iov_base: b.as_ptr() as *mut libc::c_void, iov_len: b.len() }).collect()
}

sys! {
    /// `readv(2)` into `bufs`; the bytes read.
    pub fn readv(fd: i32, bufs: &mut [Vec<u8>]) -> R<usize> {
        let v = iovecs_mut(bufs);
        // SAFETY: each iovec points into a live buffer of its length.
        ck_size(unsafe { libc::readv(fd, v.as_ptr(), v.len() as _) })
    }
}

sys! {
    /// `writev(2)`; the bytes written.
    pub fn writev(fd: i32, bufs: &[&[u8]]) -> R<usize> {
        let v = iovecs(bufs);
        // SAFETY: each iovec points into a live buffer of its length.
        ck_size(unsafe { libc::writev(fd, v.as_ptr(), v.len() as _) })
    }
}

sys! {
    /// `preadv(2)` into `bufs` at `offset`; the bytes read.
    pub fn preadv(fd: i32, bufs: &mut [Vec<u8>], offset: i64, flags: i32) -> R<usize> {
        let v = iovecs_mut(bufs);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: each iovec points into a live buffer of its length.
        let r = unsafe { if flags == 0 { libc::preadv(fd, v.as_ptr(), v.len() as _, offset as _) } else { libc::preadv2(fd, v.as_ptr(), v.len() as _, offset as _, flags) } };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let r = {
            let _ = flags;
            // SAFETY: each iovec points into a live buffer of its length.
            unsafe { libc::preadv(fd, v.as_ptr(), v.len() as _, offset as _) }
        };
        ck_size(r as isize)
    }
}

sys! {
    /// `pwritev(2)` at `offset`; the bytes written.
    pub fn pwritev(fd: i32, bufs: &[&[u8]], offset: i64, flags: i32) -> R<usize> {
        let v = iovecs(bufs);
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: each iovec points into a live buffer of its length.
        let r = unsafe { if flags == 0 { libc::pwritev(fd, v.as_ptr(), v.len() as _, offset as _) } else { libc::pwritev2(fd, v.as_ptr(), v.len() as _, offset as _, flags) } };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let r = {
            let _ = flags;
            // SAFETY: each iovec points into a live buffer of its length.
            unsafe { libc::pwritev(fd, v.as_ptr(), v.len() as _, offset as _) }
        };
        ck_size(r as isize)
    }
}

/// The `RWF_*` flags of `preadv` / `pwritev`.
pub fn rw_flags() -> Vec<(&'static str, i64)> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        vec![("RWF_HIPRI", 1), ("RWF_NOWAIT", 8), ("RWF_DSYNC", 2), ("RWF_SYNC", 4), ("RWF_APPEND", 16)]
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        Vec::new()
    }
}

linux! {
    /// `sendfile(2)`: the bytes sent; with `offset` the file position is left alone.
    pub fn sendfile(out_fd: i32, in_fd: i32, offset: Option<i64>, count: usize) -> R<usize> {
        let mut off = offset.unwrap_or(0) as libc::off_t;
        let p = if offset.is_some() { &mut off as *mut libc::off_t } else { std::ptr::null_mut() };
        // SAFETY: `p` is null or points at a live offset.
        ck_size(unsafe { libc::sendfile(out_fd, in_fd, p, count) })
    }
}

apple! {
    /// `sendfile(2)` with headers and trailers: the bytes sent. A would-block after some data
    /// reports that data.
    pub fn sendfile_bsd(out_fd: i32, in_fd: i32, offset: i64, count: i64, headers: &[&[u8]], trailers: &[&[u8]], flags: i32) -> R<u64> {
        let (mut h, mut t) = (iovecs(headers), iovecs(trailers));
        let mut hdtr = libc::sf_hdtr {
            headers: if h.is_empty() { std::ptr::null_mut() } else { h.as_mut_ptr() },
            hdr_cnt: h.len() as _,
            trailers: if t.is_empty() { std::ptr::null_mut() } else { t.as_mut_ptr() },
            trl_cnt: t.len() as _,
        };
        let mut sent = (count + headers.iter().map(|h| h.len() as i64).sum::<i64>()) as libc::off_t;
        let with_hdtr = !h.is_empty() || !t.is_empty();
        // SAFETY: every pointer refers to live data for the duration of the call.
        let r = unsafe {
            libc::sendfile(in_fd, out_fd, offset as _, &mut sent, if with_hdtr { &mut hdtr } else { std::ptr::null_mut() }, flags)
        };
        if r < 0 {
            let e = last();
            if matches!(e.code(), "EAGAIN" | "EBUSY") && sent != 0 {
                return Ok(sent as u64);
            }
            return Err(e);
        }
        Ok(sent as u64)
    }
}

apple! {
    /// `fcopyfile(3)`.
    pub fn fcopyfile(in_fd: i32, out_fd: i32, flags: u32) -> R<()> {
        // SAFETY: plain descriptors and flags; no copy state.
        ck_unit(unsafe { libc::fcopyfile(in_fd, out_fd, std::ptr::null_mut(), flags) })
    }
}

/// The `COPYFILE_*` flags `fcopyfile` takes, as `_COPYFILE_*`.
pub fn copyfile_flags() -> Vec<(&'static str, i64)> {
    #[cfg(target_vendor = "apple")]
    {
        vec![("_COPYFILE_ACL", 1), ("_COPYFILE_STAT", 2), ("_COPYFILE_XATTR", 4), ("_COPYFILE_DATA", 8)]
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        Vec::new()
    }
}

linux! {
    /// `copy_file_range(2)`: the bytes copied.
    pub fn copy_file_range(src: i32, dst: i32, count: usize, offset_src: Option<i64>, offset_dst: Option<i64>) -> R<usize> {
        let (mut os, mut od) = (offset_src.unwrap_or(0) as libc::off64_t, offset_dst.unwrap_or(0) as libc::off64_t);
        let ps = if offset_src.is_some() { &mut os as *mut libc::off64_t } else { std::ptr::null_mut() };
        let pd = if offset_dst.is_some() { &mut od as *mut libc::off64_t } else { std::ptr::null_mut() };
        // SAFETY: the offset pointers are null or point at live offsets.
        ck_size(unsafe { libc::copy_file_range(src, ps, dst, pd, count, 0) as isize })
    }
}

linux! {
    /// `splice(2)`: the bytes moved.
    pub fn splice(src: i32, dst: i32, count: usize, offset_src: Option<i64>, offset_dst: Option<i64>, flags: u32) -> R<usize> {
        let (mut os, mut od) = (offset_src.unwrap_or(0) as libc::loff_t, offset_dst.unwrap_or(0) as libc::loff_t);
        let ps = if offset_src.is_some() { &mut os as *mut libc::loff_t } else { std::ptr::null_mut() };
        let pd = if offset_dst.is_some() { &mut od as *mut libc::loff_t } else { std::ptr::null_mut() };
        // SAFETY: the offset pointers are null or point at live offsets.
        ck_size(unsafe { libc::splice(src, ps, dst, pd, count, flags) as isize })
    }
}

linux! {
    /// `posix_fallocate(3)`.
    pub fn posix_fallocate(fd: i32, offset: i64, len: i64) -> R<()> {
        // SAFETY: plain integer arguments.
        let r = unsafe { libc::posix_fallocate(fd, offset as _, len as _) };
        if r != 0 {
            return Err(std::io::Error::from_raw_os_error(r).into());
        }
        Ok(())
    }
}

linux! {
    /// `posix_fadvise(3)`.
    pub fn posix_fadvise(fd: i32, offset: i64, len: i64, advice: i32) -> R<()> {
        // SAFETY: plain integer arguments.
        let r = unsafe { libc::posix_fadvise(fd, offset as _, len as _, advice) };
        if r != 0 {
            return Err(std::io::Error::from_raw_os_error(r).into());
        }
        Ok(())
    }
}

linux! {
    /// `fdatasync(2)`.
    pub fn fdatasync(fd: i32) -> R<()> {
        // SAFETY: plain integer argument.
        ck_unit(unsafe { libc::fdatasync(fd) })
    }
}

linux! {
    /// `pipe2(2)`.
    pub fn pipe2(flags: i32) -> R<(i32, i32)> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` has room for the two descriptors.
        ck_unit(unsafe { libc::pipe2(fds.as_mut_ptr(), flags) })?;
        Ok((fds[0], fds[1]))
    }
}

linux! {
    /// `getrandom(2)`.
    pub fn getrandom(size: usize, flags: u32) -> R<Vec<u8>> {
        let mut buf = vec![0u8; size];
        // SAFETY: `buf` is a live buffer of `size` bytes.
        let n = ck_size(unsafe { libc::getrandom(buf.as_mut_ptr() as *mut libc::c_void, size, flags) as isize })?;
        buf.truncate(n);
        Ok(buf)
    }
}

linux! {
    /// `memfd_create(2)`.
    pub fn memfd_create(name: &str, flags: u32) -> R<i32> {
        let n = cstr(name)?;
        // SAFETY: `n` is a valid NUL-terminated string.
        ck(unsafe { libc::memfd_create(n.as_ptr(), flags) })
    }
}

linux! {
    /// `eventfd(2)`.
    pub fn eventfd(initval: u32, flags: i32) -> R<i32> {
        // SAFETY: plain integer arguments.
        ck(unsafe { libc::eventfd(initval, flags) })
    }
}

linux! {
    /// Reads the 64-bit counter of an `eventfd`.
    pub fn eventfd_read(fd: i32) -> R<u64> {
        let mut v = 0u64;
        // SAFETY: `v` is a live 8-byte buffer.
        let n = unsafe { libc::read(fd, &mut v as *mut u64 as *mut libc::c_void, 8) };
        if n != 8 {
            return Err(if n < 0 { last() } else { FsError("EIO") });
        }
        Ok(v)
    }
}

linux! {
    /// Adds to the 64-bit counter of an `eventfd`.
    pub fn eventfd_write(fd: i32, value: u64) -> R<()> {
        // SAFETY: `value` is a live 8-byte buffer.
        let n = unsafe { libc::write(fd, &value as *const u64 as *const libc::c_void, 8) };
        if n != 8 {
            return Err(if n < 0 { last() } else { FsError("EIO") });
        }
        Ok(())
    }
}

linux! {
    /// `pidfd_open(2)`.
    pub fn pidfd_open(pid: i32, flags: u32) -> R<i32> {
        // SAFETY: plain integer arguments to a raw syscall.
        ck(unsafe { libc::syscall(libc::SYS_pidfd_open, pid, flags) as libc::c_int })
    }
}

linux! {
    /// `unshare(2)`.
    pub fn unshare(flags: i32) -> R<()> {
        // SAFETY: plain integer argument.
        ck_unit(unsafe { libc::unshare(flags) })
    }
}

linux! {
    /// `setns(2)`.
    pub fn setns(fd: i32, nstype: i32) -> R<()> {
        // SAFETY: plain integer arguments.
        ck_unit(unsafe { libc::setns(fd, nstype) })
    }
}

linux! {
    /// `timerfd_create(2)`.
    pub fn timerfd_create(clockid: i32, flags: i32) -> R<i32> {
        // SAFETY: plain integer arguments.
        ck(unsafe { libc::timerfd_create(clockid, flags) })
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn ns_timespec(ns: i64) -> libc::timespec {
    libc::timespec { tv_sec: ns.div_euclid(1_000_000_000) as _, tv_nsec: ns.rem_euclid(1_000_000_000) as _ }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn timespec_ns(t: &libc::timespec) -> i64 {
    t.tv_sec as i64 * 1_000_000_000 + t.tv_nsec as i64
}

linux! {
    /// `timerfd_settime(2)` in nanoseconds: the previous `(delay, interval)`.
    pub fn timerfd_settime(fd: i32, flags: i32, initial_ns: i64, interval_ns: i64) -> R<(i64, i64)> {
        let new = libc::itimerspec { it_interval: ns_timespec(interval_ns), it_value: ns_timespec(initial_ns) };
        // SAFETY: a zeroed itimerspec is a valid out-parameter.
        let mut old: libc::itimerspec = unsafe { std::mem::zeroed() };
        ck_unit(unsafe { libc::timerfd_settime(fd, flags, &new, &mut old) })?;
        Ok((timespec_ns(&old.it_value), timespec_ns(&old.it_interval)))
    }
}

linux! {
    /// `timerfd_gettime(2)` in nanoseconds: `(delay, interval)`.
    pub fn timerfd_gettime(fd: i32) -> R<(i64, i64)> {
        // SAFETY: a zeroed itimerspec is a valid out-parameter.
        let mut cur: libc::itimerspec = unsafe { std::mem::zeroed() };
        ck_unit(unsafe { libc::timerfd_gettime(fd, &mut cur) })?;
        Ok((timespec_ns(&cur.it_value), timespec_ns(&cur.it_interval)))
    }
}

linux! {
    /// `getxattr(2)`.
    pub fn getxattr(path: &str, name: &str, follow: bool) -> R<Vec<u8>> {
        let (p, n) = (cstr(path)?, cstr(name)?);
        let mut buf = vec![0u8; 256];
        loop {
            // SAFETY: `buf` is a live buffer of the length passed.
            let r = unsafe {
                let b = buf.as_mut_ptr() as *mut libc::c_void;
                if follow { libc::getxattr(p.as_ptr(), n.as_ptr(), b, buf.len()) } else { libc::lgetxattr(p.as_ptr(), n.as_ptr(), b, buf.len()) }
            };
            if r < 0 {
                let e = last();
                if e.code() == "ERANGE" && buf.len() < 1 << 24 {
                    buf = vec![0u8; buf.len() * 4];
                    continue;
                }
                return Err(e);
            }
            buf.truncate(r as usize);
            return Ok(buf);
        }
    }
}

linux! {
    /// `setxattr(2)`.
    pub fn setxattr(path: &str, name: &str, value: &[u8], flags: i32, follow: bool) -> R<()> {
        let (p, n) = (cstr(path)?, cstr(name)?);
        let v = value.as_ptr() as *const libc::c_void;
        // SAFETY: `value` is a live buffer of its length.
        ck_unit(unsafe {
            if follow { libc::setxattr(p.as_ptr(), n.as_ptr(), v, value.len(), flags) } else { libc::lsetxattr(p.as_ptr(), n.as_ptr(), v, value.len(), flags) }
        })
    }
}

linux! {
    /// `removexattr(2)`.
    pub fn removexattr(path: &str, name: &str, follow: bool) -> R<()> {
        let (p, n) = (cstr(path)?, cstr(name)?);
        // SAFETY: both strings are NUL-terminated.
        ck_unit(unsafe { if follow { libc::removexattr(p.as_ptr(), n.as_ptr()) } else { libc::lremovexattr(p.as_ptr(), n.as_ptr()) } })
    }
}

linux! {
    /// `listxattr(2)`: the attribute names.
    pub fn listxattr(path: &str, follow: bool) -> R<Vec<String>> {
        let p = cstr(path)?;
        let mut buf = vec![0u8; 256];
        loop {
            // SAFETY: `buf` is a live buffer of the length passed.
            let r = unsafe {
                let b = buf.as_mut_ptr() as *mut libc::c_char;
                if follow { libc::listxattr(p.as_ptr(), b, buf.len()) } else { libc::llistxattr(p.as_ptr(), b, buf.len()) }
            };
            if r < 0 {
                let e = last();
                if e.code() == "ERANGE" && buf.len() < 1 << 24 {
                    buf = vec![0u8; buf.len() * 4];
                    continue;
                }
                return Err(e);
            }
            buf.truncate(r as usize);
            return Ok(buf.split(|&c| c == 0).filter(|s| !s.is_empty()).map(text).collect());
        }
    }
}

// ---- exec ---------------------------------------------------------------------------------------

#[cfg(unix)]
fn nul_terminated(list: &[std::ffi::CString]) -> Vec<*const libc::c_char> {
    list.iter().map(|c| c.as_ptr()).chain(std::iter::once(std::ptr::null())).collect()
}

/// `execv(3)` / `execve(3)` / `fexecve(3)`: returns only on failure. `fd` selects `fexecve`.
pub fn exec(path: &[u8], fd: Option<i32>, argv: &[Vec<u8>], env: Option<&[Vec<u8>]>) -> FsError {
    #[cfg(unix)]
    {
        let args: Result<Vec<_>, _> = argv.iter().map(|a| cstr_bytes(a)).collect();
        let envs: Option<Result<Vec<_>, _>> = env.map(|e| e.iter().map(|a| cstr_bytes(a)).collect());
        let (Ok(args), p) = (args, cstr_bytes(path)) else { return FsError("EINVAL") };
        let envs = match envs {
            Some(Ok(e)) => Some(e),
            Some(Err(e)) => return e,
            None => None,
        };
        let a = nul_terminated(&args);
        let e = envs.as_deref().map(nul_terminated);
        // SAFETY: every pointer array is NUL-terminated and backed by live CStrings.
        unsafe {
            match (fd, e) {
                #[cfg(any(target_os = "linux", target_os = "android"))]
                (Some(fd), Some(e)) => {
                    libc::fexecve(fd, a.as_ptr(), e.as_ptr());
                }
                (Some(_), _) => return FsError("ENOSYS"),
                (None, Some(e)) => {
                    let Ok(p) = p else { return FsError("EINVAL") };
                    libc::execve(p.as_ptr(), a.as_ptr(), e.as_ptr());
                }
                (None, None) => {
                    let Ok(p) = p else { return FsError("EINVAL") };
                    libc::execv(p.as_ptr(), a.as_ptr());
                }
            }
        }
        last()
    }
    #[cfg(not(unix))]
    {
        let _ = (path, fd, argv, env);
        FsError("ENOSYS")
    }
}

// ---- posix_spawn --------------------------------------------------------------------------------

/// One entry of `posix_spawn`'s `file_actions`.
#[derive(Clone, Debug)]
pub enum FileAction {
    Open { fd: i32, path: Vec<u8>, flags: i32, mode: u32 },
    Close(i32),
    Dup2(i32, i32),
    CloseFrom(i32),
}

/// The attributes of a `posix_spawn` call.
#[derive(Clone, Debug, Default)]
pub struct SpawnAttrs {
    pub setpgroup: Option<i32>,
    pub resetids: bool,
    pub setsid: bool,
    pub setsigmask: Option<Vec<i32>>,
    pub setsigdef: Option<Vec<i32>>,
    /// `(policy, priority)`; the policy is optional.
    pub scheduler: Option<(Option<i32>, i32)>,
}

/// Whether `posix_spawn` can start a new session on this platform.
pub fn spawn_has_setsid() -> bool {
    cfg!(any(target_vendor = "apple", target_os = "linux"))
}

/// Whether `posix_spawn` supports a scheduler attribute on this platform.
pub fn spawn_has_scheduler() -> bool {
    cfg!(any(target_os = "linux", target_os = "freebsd"))
}

/// Whether `posix_spawn` has a `closefrom` file action on this platform.
pub fn spawn_has_closefrom() -> bool {
    cfg!(all(target_os = "linux", target_env = "gnu"))
}

/// `posix_spawn(3)` / `posix_spawnp(3)` with `env` (`None`: the current environment): the new pid.
pub fn posix_spawn(
    path: &[u8],
    search_path: bool,
    argv: &[Vec<u8>],
    env: Option<&[Vec<u8>]>,
    actions: &[FileAction],
    attrs: &SpawnAttrs,
) -> R<i32> {
    #[cfg(unix)]
    {
        spawn_imp::run(path, search_path, argv, env, actions, attrs)
    }
    #[cfg(not(unix))]
    {
        let _ = (path, search_path, argv, env, actions, attrs);
        Err(FsError("ENOSYS"))
    }
}

#[cfg(unix)]
mod spawn_imp {
    use super::*;
    use std::mem::MaybeUninit;

    #[cfg(target_vendor = "apple")]
    extern "C" {
        #[link_name = "_NSGetEnviron"]
        fn ns_get_environ() -> *mut *const *const libc::c_char;
    }

    fn code(rc: libc::c_int) -> R<()> {
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::from_raw_os_error(rc).into())
        }
    }

    fn sigset(sigs: &[i32]) -> libc::sigset_t {
        let mut set = MaybeUninit::<libc::sigset_t>::uninit();
        // SAFETY: sigemptyset initialises the set; sigaddset only reads the signal number.
        unsafe {
            libc::sigemptyset(set.as_mut_ptr());
            for &s in sigs {
                libc::sigaddset(set.as_mut_ptr(), s);
            }
            set.assume_init()
        }
    }

    struct Actions(libc::posix_spawn_file_actions_t);

    impl Drop for Actions {
        fn drop(&mut self) {
            // SAFETY: initialised by `Actions::new`.
            unsafe { libc::posix_spawn_file_actions_destroy(&mut self.0) };
        }
    }

    struct Attr(libc::posix_spawnattr_t);

    impl Drop for Attr {
        fn drop(&mut self) {
            // SAFETY: initialised by `Attr::new`.
            unsafe { libc::posix_spawnattr_destroy(&mut self.0) };
        }
    }

    #[cfg(target_vendor = "apple")]
    const SETSID: i32 = 0x400;
    #[cfg(not(target_vendor = "apple"))]
    const SETSID: i32 = libc::POSIX_SPAWN_SETSID as i32;

    pub fn run(
        path: &[u8],
        search_path: bool,
        argv: &[Vec<u8>],
        env: Option<&[Vec<u8>]>,
        actions: &[FileAction],
        attrs: &SpawnAttrs,
    ) -> R<i32> {
        let path = cstr_bytes(path)?;
        let args = argv.iter().map(|a| cstr_bytes(a)).collect::<R<Vec<_>>>()?;
        let envs = env.map(|e| e.iter().map(|a| cstr_bytes(a)).collect::<R<Vec<_>>>()).transpose()?;
        let (a, e) = (nul_terminated(&args), envs.as_deref().map(nul_terminated));

        let mut fa = {
            let mut fa = MaybeUninit::<libc::posix_spawn_file_actions_t>::uninit();
            // SAFETY: init fills the struct on success.
            code(unsafe { libc::posix_spawn_file_actions_init(fa.as_mut_ptr()) })?;
            Actions(unsafe { fa.assume_init() })
        };
        let paths = actions
            .iter()
            .map(|a| match a {
                FileAction::Open { path, .. } => cstr_bytes(path).map(Some),
                _ => Ok(None),
            })
            .collect::<R<Vec<_>>>()?;
        for (action, p) in actions.iter().zip(&paths) {
            // SAFETY: the action list is initialised; open paths stay alive in `paths`.
            code(unsafe {
                match (action, p) {
                    (FileAction::Open { fd, flags, mode, .. }, Some(p)) => {
                        libc::posix_spawn_file_actions_addopen(&mut fa.0, *fd, p.as_ptr(), *flags, *mode as _)
                    }
                    (FileAction::Close(fd), _) => libc::posix_spawn_file_actions_addclose(&mut fa.0, *fd),
                    (FileAction::Dup2(a, b), _) => libc::posix_spawn_file_actions_adddup2(&mut fa.0, *a, *b),
                    #[cfg(all(target_os = "linux", target_env = "gnu"))]
                    (FileAction::CloseFrom(fd), _) => libc::posix_spawn_file_actions_addclosefrom_np(&mut fa.0, *fd),
                    _ => return Err(FsError("ENOSYS")),
                }
            })?;
        }

        let mut at = {
            let mut at = MaybeUninit::<libc::posix_spawnattr_t>::uninit();
            // SAFETY: init fills the struct on success.
            code(unsafe { libc::posix_spawnattr_init(at.as_mut_ptr()) })?;
            Attr(unsafe { at.assume_init() })
        };
        let mut all: i32 = 0;
        // SAFETY: the attribute object is initialised and the values are plain data.
        unsafe {
            if let Some(pg) = attrs.setpgroup {
                code(libc::posix_spawnattr_setpgroup(&mut at.0, pg))?;
                all |= libc::POSIX_SPAWN_SETPGROUP;
            }
            if attrs.resetids {
                all |= libc::POSIX_SPAWN_RESETIDS;
            }
            if attrs.setsid {
                all |= SETSID;
            }
            if let Some(s) = &attrs.setsigmask {
                code(libc::posix_spawnattr_setsigmask(&mut at.0, &sigset(s)))?;
                all |= libc::POSIX_SPAWN_SETSIGMASK;
            }
            if let Some(s) = &attrs.setsigdef {
                code(libc::posix_spawnattr_setsigdefault(&mut at.0, &sigset(s)))?;
                all |= libc::POSIX_SPAWN_SETSIGDEF;
            }
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            if let Some((policy, priority)) = attrs.scheduler {
                if let Some(p) = policy {
                    code(libc::posix_spawnattr_setschedpolicy(&mut at.0, p))?;
                    all |= libc::POSIX_SPAWN_SETSCHEDULER;
                }
                let param = libc::sched_param { sched_priority: priority };
                code(libc::posix_spawnattr_setschedparam(&mut at.0, &param))?;
                all |= libc::POSIX_SPAWN_SETSCHEDPARAM;
            }
            code(libc::posix_spawnattr_setflags(&mut at.0, all as _))?;
        }

        #[cfg(target_vendor = "apple")]
        // SAFETY: _NSGetEnviron returns the address of the process environment pointer.
        let current: *const *const libc::c_char = unsafe { *ns_get_environ() };
        #[cfg(not(target_vendor = "apple"))]
        // SAFETY: reads the process environment pointer.
        let current: *const *const libc::c_char = unsafe {
            extern "C" {
                static environ: *const *const libc::c_char;
            }
            environ
        };
        let envp = e.as_ref().map_or(current, |e| e.as_ptr());
        let mut pid: libc::pid_t = 0;
        // SAFETY: all strings and arrays are live and NUL-terminated for the call.
        let rc = unsafe {
            if search_path {
                libc::posix_spawnp(&mut pid, path.as_ptr(), &fa.0, &at.0, a.as_ptr() as *const *mut _, envp as *const *mut _)
            } else {
                libc::posix_spawn(&mut pid, path.as_ptr(), &fa.0, &at.0, a.as_ptr() as *const *mut _, envp as *const *mut _)
            }
        };
        code(rc)?;
        Ok(pid)
    }
}

/// Constants of the Linux-only calls (`EFD_*`, `TFD_*`, `GRND_*`, `MFD_*`, `CLONE_*`, ...).
pub fn linux_constants() -> Vec<(&'static str, i64)> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        vec![
            ("EFD_CLOEXEC", libc::EFD_CLOEXEC as i64),
            ("EFD_NONBLOCK", libc::EFD_NONBLOCK as i64),
            ("EFD_SEMAPHORE", libc::EFD_SEMAPHORE as i64),
            ("TFD_CLOEXEC", libc::TFD_CLOEXEC as i64),
            ("TFD_NONBLOCK", libc::TFD_NONBLOCK as i64),
            ("TFD_TIMER_ABSTIME", libc::TFD_TIMER_ABSTIME as i64),
            ("TFD_TIMER_CANCEL_ON_SET", 2),
            ("GRND_NONBLOCK", libc::GRND_NONBLOCK as i64),
            ("GRND_RANDOM", libc::GRND_RANDOM as i64),
            ("MFD_CLOEXEC", libc::MFD_CLOEXEC as i64),
            ("MFD_ALLOW_SEALING", libc::MFD_ALLOW_SEALING as i64),
            ("MFD_HUGETLB", libc::MFD_HUGETLB as i64),
            ("POSIX_FADV_NORMAL", libc::POSIX_FADV_NORMAL as i64),
            ("POSIX_FADV_SEQUENTIAL", libc::POSIX_FADV_SEQUENTIAL as i64),
            ("POSIX_FADV_RANDOM", libc::POSIX_FADV_RANDOM as i64),
            ("POSIX_FADV_NOREUSE", libc::POSIX_FADV_NOREUSE as i64),
            ("POSIX_FADV_WILLNEED", libc::POSIX_FADV_WILLNEED as i64),
            ("POSIX_FADV_DONTNEED", libc::POSIX_FADV_DONTNEED as i64),
            ("SPLICE_F_MOVE", 1),
            ("SPLICE_F_NONBLOCK", 2),
            ("SPLICE_F_MORE", 4),
            ("XATTR_CREATE", libc::XATTR_CREATE as i64),
            ("XATTR_REPLACE", libc::XATTR_REPLACE as i64),
            ("XATTR_SIZE_MAX", 65536),
            ("CLONE_FILES", libc::CLONE_FILES as i64),
            ("CLONE_FS", libc::CLONE_FS as i64),
            ("CLONE_NEWCGROUP", 0x0200_0000),
            ("CLONE_NEWIPC", libc::CLONE_NEWIPC as i64),
            ("CLONE_NEWNET", libc::CLONE_NEWNET as i64),
            ("CLONE_NEWNS", libc::CLONE_NEWNS as i64),
            ("CLONE_NEWPID", libc::CLONE_NEWPID as i64),
            ("CLONE_NEWTIME", 0x80),
            ("CLONE_NEWUSER", libc::CLONE_NEWUSER as i64),
            ("CLONE_NEWUTS", libc::CLONE_NEWUTS as i64),
            ("CLONE_SIGHAND", libc::CLONE_SIGHAND as i64),
            ("CLONE_SYSVSEM", libc::CLONE_SYSVSEM as i64),
            ("CLONE_THREAD", libc::CLONE_THREAD as i64),
            ("CLONE_VM", libc::CLONE_VM as i64),
            ("P_PIDFD", 3),
            ("PIDFD_NONBLOCK", libc::O_NONBLOCK as i64),
        ]
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        Vec::new()
    }
}

/// The names of the Linux-only `posix` functions, removed from the module elsewhere.
pub const LINUX_ONLY: &[&str] = &[
    "setresuid", "setresgid", "getresuid", "getresgid", "sched_getaffinity", "sched_setaffinity", "sched_getscheduler",
    "sched_setscheduler", "sched_getparam", "sched_setparam", "sched_rr_get_interval", "copy_file_range", "splice",
    "posix_fallocate", "posix_fadvise", "fdatasync", "pipe2", "getrandom", "memfd_create", "eventfd", "eventfd_read",
    "eventfd_write", "pidfd_open", "unshare", "setns", "timerfd_create", "timerfd_settime", "timerfd_settime_ns",
    "timerfd_gettime", "timerfd_gettime_ns", "getxattr", "setxattr", "removexattr", "listxattr", "sched_param",
];

/// `EFD_CLOEXEC`, `TFD_CLOEXEC` and `MFD_CLOEXEC` (zero off Linux).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub const EFD_CLOEXEC: i32 = libc::EFD_CLOEXEC;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub const TFD_CLOEXEC: i32 = libc::TFD_CLOEXEC;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub const MFD_CLOEXEC: u32 = libc::MFD_CLOEXEC;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub const EFD_CLOEXEC: i32 = 0;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub const TFD_CLOEXEC: i32 = 0;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub const MFD_CLOEXEC: u32 = 0;

/// The `POSIX_SPAWN_*` file-action tags.
pub const SPAWN_OPEN: i64 = 0;
pub const SPAWN_CLOSE: i64 = 1;
pub const SPAWN_DUP2: i64 = 2;
pub const SPAWN_CLOSEFROM: i64 = 3;

// ---- terminals and sessions ---------------------------------------------------------------------

sys! {
    /// `tcgetpgrp(3)`.
    pub fn tcgetpgrp(fd: i32) -> R<i32> {
        // SAFETY: plain integer argument.
        ck(unsafe { libc::tcgetpgrp(fd) })
    }
}

sys! {
    /// `tcsetpgrp(3)`.
    pub fn tcsetpgrp(fd: i32, pgid: i32) -> R<()> {
        // SAFETY: plain integer arguments.
        ck_unit(unsafe { libc::tcsetpgrp(fd, pgid) })
    }
}

sys! {
    /// `ctermid(3)`: the path of the controlling terminal.
    pub fn ctermid() -> R<String> {
        extern "C" {
            fn ctermid(s: *mut libc::c_char) -> *mut libc::c_char;
        }
        let mut buf = [0 as libc::c_char; 256];
        // SAFETY: `buf` is larger than L_ctermid.
        let p = unsafe { ctermid(buf.as_mut_ptr()) };
        if p.is_null() {
            return Err(last());
        }
        // SAFETY: ctermid wrote a NUL-terminated string into `buf`.
        Ok(unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
    }
}

sys! {
    /// `ttyname_r(3)`.
    pub fn ttyname(fd: i32) -> R<String> {
        let mut buf = vec![0 as libc::c_char; 1024];
        // SAFETY: `buf` is a live buffer of the length passed.
        let rc = unsafe { libc::ttyname_r(fd, buf.as_mut_ptr(), buf.len()) };
        if rc != 0 {
            return Err(std::io::Error::from_raw_os_error(rc).into());
        }
        // SAFETY: ttyname_r wrote a NUL-terminated string into `buf`.
        Ok(unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned())
    }
}

sys! {
    /// `grantpt(3)`.
    pub fn grantpt(fd: i32) -> R<()> {
        // SAFETY: plain integer argument.
        ck_unit(unsafe { libc::grantpt(fd) })
    }
}

sys! {
    /// `unlockpt(3)`.
    pub fn unlockpt(fd: i32) -> R<()> {
        // SAFETY: plain integer argument.
        ck_unit(unsafe { libc::unlockpt(fd) })
    }
}

sys! {
    /// `posix_openpt(3)`; the new descriptor is not inheritable.
    pub fn posix_openpt(oflag: i32) -> R<i32> {
        // SAFETY: plain integer argument.
        let fd = ck(unsafe { libc::posix_openpt(oflag) })?;
        if let Err(e) = crate::fdctl::set_inheritable(fd, false) {
            // SAFETY: `fd` was just opened and is owned here.
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(fd)
    }
}

sys! {
    /// `ptsname(3)`: the slave device of a pseudo-terminal master.
    pub fn ptsname(fd: i32) -> R<String> {
        // SAFETY: ptsname returns null or a pointer to static storage.
        let p = unsafe { libc::ptsname(fd) };
        if p.is_null() {
            return Err(last());
        }
        // SAFETY: a non-null result is a NUL-terminated string.
        Ok(unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn device_numbers_round_trip() {
        let dev = makedev(3, 7);
        assert_eq!((major(dev), minor(dev)), (3, 7));
    }

    #[test]
    fn statvfs_of_root() {
        let s = statvfs("/").unwrap();
        assert!(s.bsize > 0 && s.namemax > 0);
    }

    #[test]
    fn confstr_path() {
        let name = confstr_names().into_iter().find(|(n, _)| *n == "CS_PATH").unwrap().1;
        assert!(confstr(name as i32).unwrap().is_some());
    }

    #[test]
    fn pathconf_name_max() {
        let name = pathconf_names().into_iter().find(|(n, _)| *n == "PC_NAME_MAX").unwrap().1;
        assert!(pathconf("/", name as i32).unwrap() >= 255);
    }
}
