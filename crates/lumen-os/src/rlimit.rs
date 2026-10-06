//! Resource limits and usage: `getrlimit` / `setrlimit` / `prlimit`, `getrusage` and the page
//! size. The Python `resource` module and the process-resource report of both runtimes run on
//! these. Off Unix everything fails with `ENOSYS`.

use crate::errno::FsError;

pub type R<T> = Result<T, FsError>;

/// `RLIM_INFINITY` as a signed value, which is how Python reports it.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub const RLIM_INFINITY: u64 = 0x7fff_ffff_ffff_ffff;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub const RLIM_INFINITY: u64 = u64::MAX;

/// The number of resource kinds (`RLIM_NLIMITS`); a valid resource is below it.
#[cfg(any(target_os = "macos", target_os = "ios"))]
pub const RLIM_NLIMITS: i32 = 9;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
pub const RLIM_NLIMITS: i32 = 16;

/// A soft and a hard limit (`struct rlimit`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub cur: u64,
    pub max: u64,
}

#[cfg(unix)]
fn os_err() -> FsError {
    std::io::Error::last_os_error().into()
}

/// `getrlimit(2)`.
pub fn getrlimit(resource: i32) -> R<Limits> {
    #[cfg(unix)]
    {
        // SAFETY: a zeroed rlimit is a valid out-parameter.
        let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrlimit(resource as _, &mut rl) } != 0 {
            return Err(os_err());
        }
        Ok(Limits { cur: rl.rlim_cur as u64, max: rl.rlim_max as u64 })
    }
    #[cfg(not(unix))]
    {
        let _ = resource;
        Err(FsError("ENOSYS"))
    }
}

/// `setrlimit(2)`.
pub fn setrlimit(resource: i32, limits: Limits) -> R<()> {
    #[cfg(unix)]
    {
        let rl = libc::rlimit { rlim_cur: limits.cur as _, rlim_max: limits.max as _ };
        // SAFETY: `rl` is a live struct rlimit.
        if unsafe { libc::setrlimit(resource as _, &rl) } != 0 {
            return Err(os_err());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (resource, limits);
        Err(FsError("ENOSYS"))
    }
}

/// Whether [`prlimit`] exists here.
pub const HAVE_PRLIMIT: bool = cfg!(target_os = "linux");

/// `prlimit(2)`: the previous limits of `pid`'s `resource`, setting `new` when given.
pub fn prlimit(pid: i32, resource: i32, new: Option<Limits>) -> R<Limits> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: zeroed rlimits are valid; both pointers are live structs (or null).
        let mut old: libc::rlimit = unsafe { std::mem::zeroed() };
        let new = new.map(|l| libc::rlimit { rlim_cur: l.cur as _, rlim_max: l.max as _ });
        let new_ptr = new.as_ref().map_or(std::ptr::null(), |l| l as *const libc::rlimit);
        if unsafe { libc::prlimit(pid, resource as _, new_ptr, &mut old) } != 0 {
            return Err(os_err());
        }
        Ok(Limits { cur: old.rlim_cur as u64, max: old.rlim_max as u64 })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, resource, new);
        Err(FsError("ENOSYS"))
    }
}

/// `RUSAGE_SELF`, `RUSAGE_CHILDREN`.
pub const RUSAGE_SELF: i32 = 0;
pub const RUSAGE_CHILDREN: i32 = -1;

/// The counters of `getrusage(2)`: CPU times in seconds, the rest as the OS reports them
/// (`max_rss` in the OS's own unit: bytes on macOS, KiB elsewhere).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rusage {
    pub utime: f64,
    pub stime: f64,
    pub counters: [i64; 14],
}

#[cfg(unix)]
pub(crate) fn rusage_of(u: &libc::rusage) -> Rusage {
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 * 0.000001;
    Rusage {
        utime: secs(u.ru_utime),
        stime: secs(u.ru_stime),
        counters: [
            u.ru_maxrss as i64,
            u.ru_ixrss as i64,
            u.ru_idrss as i64,
            u.ru_isrss as i64,
            u.ru_minflt as i64,
            u.ru_majflt as i64,
            u.ru_nswap as i64,
            u.ru_inblock as i64,
            u.ru_oublock as i64,
            u.ru_msgsnd as i64,
            u.ru_msgrcv as i64,
            u.ru_nsignals as i64,
            u.ru_nvcsw as i64,
            u.ru_nivcsw as i64,
        ],
    }
}

/// `getrusage(2)`: `maxrss, ixrss, idrss, isrss, minflt, majflt, nswap, inblock, oublock,
/// msgsnd, msgrcv, nsignals, nvcsw, nivcsw` are `counters` in order.
pub fn getrusage(who: i32) -> R<Rusage> {
    #[cfg(unix)]
    {
        // SAFETY: a zeroed rusage is a valid out-parameter for getrusage.
        let mut u: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(who as _, &mut u) } != 0 {
            return Err(os_err());
        }
        Ok(rusage_of(&u))
    }
    #[cfg(not(unix))]
    {
        let _ = who;
        Err(FsError("ENOSYS"))
    }
}

/// `getpagesize()`.
pub fn pagesize() -> i64 {
    #[cfg(unix)]
    {
        // SAFETY: sysconf takes a plain integer.
        let n = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if n > 0 {
            n as i64
        } else {
            4096
        }
    }
    #[cfg(not(unix))]
    4096
}

/// The `resource` module's `RLIMIT_*` and `RUSAGE_*` constants on this platform.
pub fn constants() -> Vec<(&'static str, i64)> {
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios"))]
    let mut v: Vec<(&'static str, i64)> = Vec::new();
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos", target_os = "ios")))]
    let v: Vec<(&'static str, i64)> = Vec::new();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    v.extend([
        ("RLIMIT_CPU", 0),
        ("RLIMIT_FSIZE", 1),
        ("RLIMIT_DATA", 2),
        ("RLIMIT_STACK", 3),
        ("RLIMIT_CORE", 4),
        ("RLIMIT_RSS", 5),
        ("RLIMIT_NPROC", 6),
        ("RLIMIT_NOFILE", 7),
        ("RLIMIT_MEMLOCK", 8),
        ("RLIMIT_AS", 9),
        ("RLIMIT_SIGPENDING", 11),
        ("RLIMIT_MSGQUEUE", 12),
        ("RLIMIT_NICE", 13),
        ("RLIMIT_RTPRIO", 14),
        ("RLIMIT_RTTIME", 15),
        ("RUSAGE_SELF", 0),
        ("RUSAGE_CHILDREN", -1),
        ("RUSAGE_THREAD", 1),
    ]);
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    v.extend([
        ("RLIMIT_CPU", 0),
        ("RLIMIT_FSIZE", 1),
        ("RLIMIT_DATA", 2),
        ("RLIMIT_STACK", 3),
        ("RLIMIT_CORE", 4),
        ("RLIMIT_AS", 5),
        ("RLIMIT_RSS", 5),
        ("RLIMIT_MEMLOCK", 6),
        ("RLIMIT_NPROC", 7),
        ("RLIMIT_NOFILE", 8),
        ("RUSAGE_SELF", 0),
        ("RUSAGE_CHILDREN", -1),
    ]);
    v
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn limits_and_usage() {
        let nofile = constants().iter().find(|c| c.0 == "RLIMIT_NOFILE").unwrap().1 as i32;
        let l = getrlimit(nofile).unwrap();
        assert!(l.cur <= l.max);
        setrlimit(nofile, l).unwrap();
        assert!(getrusage(RUSAGE_SELF).unwrap().utime >= 0.0);
        assert!(pagesize() >= 4096);
    }
}
