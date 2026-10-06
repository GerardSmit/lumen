//! Process and machine facts a runtime reports: resource usage (`getrusage` and its Windows
//! equivalents, as libuv gathers them), resident, free and available memory, CPU count, model and
//! memory size, load averages and uptime, scheduling priority and the temporary directory.

use crate::errno::FsError;

pub type R<T> = Result<T, FsError>;

/// The counters of `getrusage(RUSAGE_SELF)`; CPU times in microseconds, `max_rss` in KiB.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceUsage {
    pub user_us: u64,
    pub system_us: u64,
    pub max_rss_kib: u64,
    pub minor_faults: u64,
    pub major_faults: u64,
    pub swaps: u64,
    pub block_in: u64,
    pub block_out: u64,
    pub msgs_sent: u64,
    pub msgs_received: u64,
    pub signals: u64,
    pub voluntary_switches: u64,
    pub involuntary_switches: u64,
}

#[cfg(unix)]
pub fn resource_usage() -> R<ResourceUsage> {
    let u = crate::rlimit::getrusage(crate::rlimit::RUSAGE_SELF)?;
    let [maxrss, _, _, _, minflt, majflt, nswap, inblock, oublock, msgsnd, msgrcv, nsignals, nvcsw, nivcsw] = u.counters;
    // macOS reports ru_maxrss in bytes, everyone else in KiB.
    let max_rss_kib = if cfg!(any(target_os = "macos", target_os = "ios")) { maxrss as u64 / 1024 } else { maxrss as u64 };
    Ok(ResourceUsage {
        user_us: (u.utime * 1_000_000.0).round() as u64,
        system_us: (u.stime * 1_000_000.0).round() as u64,
        max_rss_kib,
        minor_faults: minflt as u64,
        major_faults: majflt as u64,
        swaps: nswap as u64,
        block_in: inblock as u64,
        block_out: oublock as u64,
        msgs_sent: msgsnd as u64,
        msgs_received: msgrcv as u64,
        signals: nsignals as u64,
        voluntary_switches: nvcsw as u64,
        involuntary_switches: nivcsw as u64,
    })
}

/// The same counters from the sources libuv's `uv_getrusage` uses: process times, memory
/// counters (page faults count as major faults) and I/O operation counts.
#[cfg(windows)]
pub fn resource_usage() -> R<ResourceUsage> {
    let (mut creation, mut exit, mut kernel, mut user) = Default::default();
    let memory = win::memory_counters();
    let mut io = win::IoCounters::default();
    // SAFETY: the pseudo-handle needs no closing; every out pointer is a live struct.
    unsafe {
        let process = win::GetCurrentProcess();
        win::GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user);
        win::GetProcessIoCounters(process, &mut io);
    }
    Ok(ResourceUsage {
        user_us: win::micros(&user),
        system_us: win::micros(&kernel),
        max_rss_kib: memory.peak_working_set as u64 / 1024,
        major_faults: u64::from(memory.page_fault_count),
        block_in: io.read_ops,
        block_out: io.write_ops,
        ..Default::default()
    })
}

#[cfg(not(any(unix, windows)))]
pub fn resource_usage() -> R<ResourceUsage> {
    Ok(ResourceUsage::default())
}

/// Bytes of physical memory the process occupies now, where the OS reports it.
pub fn resident_set_bytes() -> Option<u64> {
    resident_set_bytes_of(std::process::id())
}

/// Bytes of physical memory process `pid` occupies now (on Windows only the current process).
pub fn resident_set_bytes_of(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let pages = std::fs::read_to_string(format!("/proc/{pid}/statm"))
            .ok()?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()?;
        // SAFETY: sysconf has no failure mode beyond returning -1.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        Some(pages * if page > 0 { page as u64 } else { 4096 })
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: a zeroed rusage_info_v2 is a valid out-buffer for proc_pid_rusage.
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::proc_pid_rusage(
                pid as libc::c_int,
                libc::RUSAGE_INFO_V2,
                (&mut info as *mut libc::rusage_info_v2).cast(),
            )
        };
        (rc == 0).then_some(info.ri_resident_size)
    }
    #[cfg(windows)]
    {
        (pid == std::process::id()).then(|| win::memory_counters().working_set as u64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = pid;
        None
    }
}

/// `(available, constrained)` memory in bytes, as Node's `process.availableMemory()` /
/// `process.constrainedMemory()` see them: a cgroup limit (0 if none) caps what is available to
/// a process already using `rss` bytes.
pub fn available_memory(rss: u64) -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        let [total, host_available] = meminfo(&["MemTotal:", "MemAvailable:"]);
        let limit = std::fs::read_to_string("/sys/fs/cgroup/memory.max")
            .ok()
            .or_else(|| std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes").ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|limit| *limit > 0 && *limit < total);
        let available = limit.map_or(host_available, |limit| {
            host_available.min(limit.saturating_sub(rss))
        });
        (available, limit.unwrap_or(0))
    }
    #[cfg(target_os = "macos")]
    {
        let n = |name| sysctl::<u64>(name).unwrap_or(0);
        let total = n("hw.memsize");
        let pages =
            n("vm.page_free_count") + n("vm.page_inactive_count") + n("vm.page_purgeable_count");
        let page_size = sysctl::<u64>("hw.pagesize").unwrap_or(4096);
        (
            pages
                .saturating_mul(page_size)
                .min(total.saturating_sub(rss)),
            0,
        )
    }
    #[cfg(windows)]
    {
        let _ = rss;
        (win::memory_status().avail_phys, 0)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = rss;
        (0, 0)
    }
}

/// `(model, speed in MHz, total memory in bytes)` of the machine (empty / 0 where unknown).
pub fn cpu_and_memory() -> (String, u64, u64) {
    #[cfg(target_os = "macos")]
    {
        let mut model = [0u8; 256];
        let model = sysctl_bytes("machdep.cpu.brand_string", &mut model)
            .map(|n| {
                String::from_utf8_lossy(&model[..n])
                    .trim_end_matches('\0')
                    .to_string()
            })
            .unwrap_or_default();
        let hz = sysctl::<u64>("hw.cpufrequency").unwrap_or(0);
        (
            model,
            hz / 1_000_000,
            sysctl::<u64>("hw.memsize").unwrap_or(0),
        )
    }
    #[cfg(target_os = "linux")]
    {
        let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let field = |key: &str| {
            cpuinfo
                .lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_once(':'))
                .map(|(_, v)| v.trim().to_string())
        };
        let model = field("model name").unwrap_or_default();
        let mhz = field("cpu MHz")
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.0) as u64;
        (model, mhz, meminfo(&["MemTotal:"])[0])
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    (String::new(), 0, 0)
}

/// The 1, 5 and 15 minute load averages (zeros where the OS has none).
pub fn loadavg() -> [f64; 3] {
    #[cfg(all(unix, not(target_os = "android")))]
    {
        let mut out = [0f64; 3];
        // SAFETY: `out` has room for the three samples requested.
        if unsafe { libc::getloadavg(out.as_mut_ptr(), 3) } == 3 {
            return out;
        }
    }
    [0.0; 3]
}

/// Seconds since boot (0 where unknown).
pub fn uptime() -> f64 {
    #[cfg(target_os = "macos")]
    {
        sysctl::<libc::timeval>("kern.boottime")
            .map(|boot| {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                (now - boot.tv_sec as f64 - boot.tv_usec as f64 / 1e6).max(0.0)
            })
            .unwrap_or(0.0)
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|t| {
                t.split_whitespace()
                    .next()
                    .and_then(|n| n.parse::<f64>().ok())
            })
            .unwrap_or(0.0)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    0.0
}

/// Bytes of memory available to new allocations (0 where unknown).
pub fn free_memory() -> f64 {
    #[cfg(target_os = "macos")]
    {
        let free_pages = sysctl::<u32>("vm.page_free_count").unwrap_or(0) as f64;
        // SAFETY: sysconf has no failure mode beyond returning -1.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(0) as f64;
        free_pages * page
    }
    #[cfg(target_os = "linux")]
    {
        meminfo(&["MemAvailable:"])[0] as f64
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    0.0
}

#[cfg(target_os = "macos")]
fn sysctl<T: Default>(name: &str) -> Option<T> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut value = T::default();
    let mut len = std::mem::size_of::<T>();
    // SAFETY: `value` is a writable `T` of `len` bytes.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            (&mut value as *mut T).cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(value)
}

/// A `sysctlbyname` value of up to `buf.len()` bytes; the length written.
#[cfg(target_os = "macos")]
fn sysctl_bytes(name: &str, buf: &mut [u8]) -> Option<usize> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut len = buf.len();
    // SAFETY: `buf` is writable for `len` bytes and `len` is updated to the bytes written.
    let rc = unsafe {
        libc::sysctlbyname(
            cname.as_ptr(),
            buf.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    (rc == 0).then_some(len)
}

pub fn cpu_count() -> usize {
    crate::sched::current().available_parallelism().get()
}

/// Byte values of `/proc/meminfo` fields (`"MemTotal:"`), 0 for a missing one.
#[cfg(target_os = "linux")]
fn meminfo<const N: usize>(fields: &[&str; N]) -> [u64; N] {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    fields.map(|name| {
        text.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|kb| kb.parse::<u64>().ok())
            .map_or(0, |kb| kb * 1024)
    })
}

/// `getpriority(PRIO_PROCESS, pid)`.
pub fn get_priority(pid: i32) -> R<i32> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        crate::posix::getpriority(libc::PRIO_PROCESS as i32, pid as u32)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = pid;
        Ok(0)
    }
}

/// `setpriority(PRIO_PROCESS, pid, priority)`.
pub fn set_priority(pid: i32, priority: i32) -> R<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        // SAFETY: setpriority takes plain integers.
        if unsafe { libc::setpriority(libc::PRIO_PROCESS as _, pid as libc::id_t, priority) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (pid, priority);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
unsafe fn errno_location() -> *mut libc::c_int {
    libc::__error()
}

#[cfg(target_os = "linux")]
unsafe fn errno_location() -> *mut libc::c_int {
    libc::__errno_location()
}

/// The temporary directory as Node's `os.tmpdir()` picks it from the environment `env`: Unix
/// `TMPDIR`, `TMP`, `TEMP`, else `/tmp`, without a trailing `/`; Windows `TEMP`, `TMP`, else
/// `%SystemRoot%\temp`, without a trailing `\` (but keeping `C:\`).
pub fn tmpdir_from(env: impl Fn(&str) -> Option<String>) -> String {
    let get = |name: &str| env(name).filter(|v| !v.is_empty());
    if cfg!(windows) {
        let mut path = get("TEMP").or_else(|| get("TMP")).unwrap_or_else(|| {
            format!(
                "{}\\temp",
                get("SystemRoot")
                    .or_else(|| get("windir"))
                    .unwrap_or_default()
            )
        });
        if path.len() > 1 && path.ends_with('\\') && !path.ends_with(":\\") {
            path.pop();
        }
        path
    } else {
        let mut path = get("TMPDIR")
            .or_else(|| get("TMP"))
            .or_else(|| get("TEMP"))
            .unwrap_or_else(|| "/tmp".into());
        if path.len() > 1 && path.ends_with('/') {
            path.pop();
        }
        path
    }
}

/// [`tmpdir_from`] the process environment.
pub fn tmpdir() -> String {
    tmpdir_from(|name| std::env::var(name).ok())
}

#[cfg(windows)]
mod win {
    #[repr(C)]
    #[derive(Default)]
    pub struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct MemoryCounters {
        pub cb: u32,
        pub page_fault_count: u32,
        pub peak_working_set: usize,
        pub working_set: usize,
        quota_peak_paged_pool: usize,
        quota_paged_pool: usize,
        quota_peak_non_paged_pool: usize,
        quota_non_paged_pool: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct IoCounters {
        pub read_ops: u64,
        pub write_ops: u64,
        other_ops: u64,
        read_bytes: u64,
        write_bytes: u64,
        other_bytes: u64,
    }

    #[repr(C)]
    #[derive(Default)]
    pub struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        pub avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }

    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetCurrentProcess() -> isize;
        pub fn GetProcessTimes(
            process: isize,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn K32GetProcessMemoryInfo(process: isize, counters: *mut MemoryCounters, cb: u32) -> i32;
        pub fn GetProcessIoCounters(process: isize, counters: *mut IoCounters) -> i32;
        fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
    }

    /// FILETIME counts 100 ns units.
    pub fn micros(t: &FileTime) -> u64 {
        ((u64::from(t.high) << 32) | u64::from(t.low)) / 10
    }

    pub fn memory_counters() -> MemoryCounters {
        let mut memory = MemoryCounters {
            cb: std::mem::size_of::<MemoryCounters>() as u32,
            ..Default::default()
        };
        // SAFETY: the pseudo-handle needs no closing; `memory` carries its size.
        unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut memory, memory.cb) };
        memory
    }

    pub fn memory_status() -> MemoryStatusEx {
        let mut status = MemoryStatusEx {
            length: std::mem::size_of::<MemoryStatusEx>() as u32,
            ..Default::default()
        };
        // SAFETY: `status` carries its size.
        unsafe { GlobalMemoryStatusEx(&mut status) };
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tmpdir_rules() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |k: &str| vars.iter().find(|v| v.0 == k).map(|v| v.1.to_string())
        };
        if cfg!(windows) {
            assert_eq!(tmpdir_from(env(&[("TEMP", "C:\\")])), "C:\\");
            assert_eq!(tmpdir_from(env(&[("TEMP", "\\temp\\")])), "\\temp");
        } else {
            assert_eq!(tmpdir_from(env(&[])), "/tmp");
            assert_eq!(
                tmpdir_from(env(&[("TMPDIR", ""), ("TMP", "/tmp2/")])),
                "/tmp2"
            );
            assert_eq!(tmpdir_from(env(&[("TMPDIR", "/tmpdir\\")])), "/tmpdir\\");
            assert_eq!(tmpdir_from(env(&[("TMPDIR", "/")])), "/");
        }
    }

    #[test]
    fn usage() {
        let u = resource_usage().unwrap();
        assert!(u.max_rss_kib > 0 || cfg!(not(any(unix, windows))));
        let _ = (
            resident_set_bytes(),
            available_memory(0),
            cpu_and_memory(),
            get_priority(0),
        );
    }
}
