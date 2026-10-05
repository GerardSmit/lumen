//! Local time through the C library (`localtime_r`, `mktime`, `tzset`), so the zone and its
//! abbreviations are the ones the process sees: `TZ`, else the system zone; the system zone's
//! IANA name ([`system_zone`]) for runtimes that keep their own zone in `lumen_common::local_tz`;
//! the clocks.

use crate::errno::FsError;
use lumen_common::civil::Tm;

pub type R<T> = Result<T, FsError>;

/// The local broken-down time of `sec` seconds since the epoch.
#[cfg(unix)]
pub fn localtime(sec: i64) -> R<Tm> {
    #[allow(
        clippy::useless_conversion,
        reason = "time_t is 32 bits on some targets"
    )]
    let t: libc::time_t = sec.try_into().map_err(|_| FsError("EOVERFLOW"))?;
    // SAFETY: a zeroed tm is a valid out-parameter for localtime_r.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        let e = std::io::Error::last_os_error();
        return Err(if e.raw_os_error() == Some(0) {
            FsError("EINVAL")
        } else {
            e.into()
        });
    }
    let zone = if tm.tm_zone.is_null() {
        None
    } else {
        // SAFETY: localtime_r points tm_zone at a NUL-terminated abbreviation it keeps alive.
        Some(
            unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }
                .to_string_lossy()
                .into_owned(),
        )
    };
    Ok(Tm {
        year: tm.tm_year as i64 + 1900,
        mon: tm.tm_mon + 1,
        mday: tm.tm_mday,
        hour: tm.tm_hour,
        min: tm.tm_min,
        sec: tm.tm_sec,
        wday: (tm.tm_wday + 6) % 7,
        yday: tm.tm_yday + 1,
        isdst: tm.tm_isdst,
        gmtoff: tm.tm_gmtoff as i64,
        zone,
    })
}

#[cfg(not(unix))]
pub fn localtime(sec: i64) -> R<Tm> {
    let mut tm = Tm::from_epoch(sec, 0);
    tm.zone = Some("UTC".to_string());
    Ok(tm)
}

/// The instant of local time `tm` (`year` through `sec` and `isdst`; out-of-range fields carry
/// over), or `None` when the C library cannot represent it.
#[cfg(unix)]
pub fn mktime(tm: &Tm) -> Option<i64> {
    // SAFETY: a zeroed tm is a valid argument once its fields are filled in.
    let mut c: libc::tm = unsafe { std::mem::zeroed() };
    c.tm_year = i32::try_from(tm.year - 1900).ok()?;
    c.tm_mon = tm.mon - 1;
    c.tm_mday = tm.mday;
    c.tm_hour = tm.hour;
    c.tm_min = tm.min;
    c.tm_sec = tm.sec;
    c.tm_isdst = tm.isdst;
    // mktime leaves tm_wday alone on failure, which tells a failure from the instant -1.
    c.tm_wday = -1;
    let t = unsafe { libc::mktime(&mut c) };
    if t == -1 && c.tm_wday == -1 {
        return None;
    }
    Some(t as i64)
}

#[cfg(not(unix))]
pub fn mktime(tm: &Tm) -> Option<i64> {
    Some(tm.to_epoch_utc())
}

/// The system's IANA zone: the `/etc/localtime` link's `.../zoneinfo/<name>` target, else
/// `/etc/timezone` (Debian).
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub fn system_zone() -> Option<String> {
    if let Ok(target) = std::fs::read_link("/etc/localtime") {
        let target = target.to_string_lossy();
        if let Some(pos) = target.rfind("zoneinfo/") {
            return Some(target[pos + "zoneinfo/".len()..].to_string());
        }
    }
    let name = std::fs::read_to_string("/etc/timezone").ok()?;
    Some(name.trim().to_string()).filter(|n| !n.is_empty())
}

#[cfg(not(all(unix, not(target_arch = "wasm32"))))]
pub fn system_zone() -> Option<String> {
    None
}

/// Re-reads `TZ` for later local-time conversions.
pub fn tzset() {
    #[cfg(unix)]
    {
        extern "C" {
            fn tzset();
        }
        // SAFETY: tzset takes no arguments and only updates the C library's zone state.
        unsafe { tzset() }
    }
}

/// The `CLOCK_*` ids `clock_gettime` accepts on this platform.
pub fn clock_ids() -> &'static [(&'static str, i64)] {
    #[cfg(target_os = "macos")]
    {
        static T: &[(&str, i64)] = &[
            ("CLOCK_MONOTONIC", libc::CLOCK_MONOTONIC as i64),
            ("CLOCK_MONOTONIC_RAW", libc::CLOCK_MONOTONIC_RAW as i64),
            (
                "CLOCK_MONOTONIC_RAW_APPROX",
                libc::CLOCK_MONOTONIC_RAW_APPROX as i64,
            ),
            (
                "CLOCK_PROCESS_CPUTIME_ID",
                libc::CLOCK_PROCESS_CPUTIME_ID as i64,
            ),
            ("CLOCK_REALTIME", libc::CLOCK_REALTIME as i64),
            (
                "CLOCK_THREAD_CPUTIME_ID",
                libc::CLOCK_THREAD_CPUTIME_ID as i64,
            ),
            ("CLOCK_UPTIME_RAW", libc::CLOCK_UPTIME_RAW as i64),
            (
                "CLOCK_UPTIME_RAW_APPROX",
                libc::CLOCK_UPTIME_RAW_APPROX as i64,
            ),
        ];
        T
    }
    #[cfg(target_os = "linux")]
    {
        static T: &[(&str, i64)] = &[
            ("CLOCK_BOOTTIME", libc::CLOCK_BOOTTIME as i64),
            ("CLOCK_MONOTONIC", libc::CLOCK_MONOTONIC as i64),
            ("CLOCK_MONOTONIC_RAW", libc::CLOCK_MONOTONIC_RAW as i64),
            (
                "CLOCK_PROCESS_CPUTIME_ID",
                libc::CLOCK_PROCESS_CPUTIME_ID as i64,
            ),
            ("CLOCK_REALTIME", libc::CLOCK_REALTIME as i64),
            ("CLOCK_TAI", libc::CLOCK_TAI as i64),
            (
                "CLOCK_THREAD_CPUTIME_ID",
                libc::CLOCK_THREAD_CPUTIME_ID as i64,
            ),
        ];
        T
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        &[]
    }
}

/// `clock_gettime` (`res` false) or `clock_getres` (`res` true) of clock `id`, in nanoseconds.
#[cfg(unix)]
pub fn clock_ns(id: i64, res: bool) -> R<i128> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let id = id as libc::clockid_t;
    // SAFETY: ts is a valid out-parameter.
    let rc = unsafe {
        if res {
            libc::clock_getres(id, &mut ts)
        } else {
            libc::clock_gettime(id, &mut ts)
        }
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(ts.tv_sec as i128 * 1_000_000_000 + ts.tv_nsec as i128)
}

#[cfg(not(unix))]
pub fn clock_ns(_id: i64, _res: bool) -> R<i128> {
    Err(FsError("ENOSYS"))
}

/// `clock_settime` of clock `id` to `ns` nanoseconds.
#[cfg(unix)]
pub fn clock_set_ns(id: i64, ns: i128) -> R<()> {
    let ts = libc::timespec {
        tv_sec: ns.div_euclid(1_000_000_000) as libc::time_t,
        tv_nsec: ns.rem_euclid(1_000_000_000) as _,
    };
    // SAFETY: ts is a valid timespec.
    if unsafe { libc::clock_settime(id as libc::clockid_t, &ts) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn clock_set_ns(_id: i64, _ns: i128) -> R<()> {
    Err(FsError("ENOSYS"))
}

/// CPU time used by the process (or by the calling thread), in nanoseconds.
pub fn cpu_time_ns(thread: bool) -> R<i128> {
    #[cfg(unix)]
    {
        let id = if thread {
            libc::CLOCK_THREAD_CPUTIME_ID
        } else {
            libc::CLOCK_PROCESS_CPUTIME_ID
        };
        clock_ns(id as i64, false)
    }
    #[cfg(not(unix))]
    {
        let _ = thread;
        Err(FsError("ENOSYS"))
    }
}
