//! Process-level services (`getcwd`, ids, `umask`, `uname`, entropy, terminal size, ...) with
//! errors carried as [`FsError`].

use crate::errno::FsError;

pub type R<T> = Result<T, FsError>;

pub fn getcwd() -> R<String> {
    Ok(std::env::current_dir()?.to_string_lossy().into_owned())
}

pub fn chdir(path: &str) -> R<()> {
    Ok(std::env::set_current_dir(path)?)
}

pub fn getpid() -> u32 {
    std::process::id()
}

macro_rules! id_fn {
    ($name:ident, $call:ident) => {
        pub fn $name() -> u32 {
            #[cfg(unix)]
            {
                // SAFETY: no arguments, cannot fail.
                unsafe { libc::$call() as u32 }
            }
            #[cfg(not(unix))]
            {
                0
            }
        }
    };
}

id_fn!(getppid, getppid);
id_fn!(getuid, getuid);
id_fn!(geteuid, geteuid);
id_fn!(getgid, getgid);
id_fn!(getegid, getegid);

/// Sets the file-mode creation mask and returns the previous one.
pub fn umask(mask: u32) -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: umask has no failure mode.
        unsafe { libc::umask(mask as libc::mode_t) as u32 }
    }
    #[cfg(not(unix))]
    {
        let _ = mask;
        0o022
    }
}

/// `(sysname, nodename, release, version, machine)`.
pub fn uname() -> R<[String; 5]> {
    #[cfg(unix)]
    {
        // SAFETY: a zeroed utsname is a valid out-parameter for uname.
        let mut u: libc::utsname = unsafe { std::mem::zeroed() };
        if unsafe { libc::uname(&mut u) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let s = |f: &[libc::c_char]| {
            let bytes: Vec<u8> = f.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
            String::from_utf8_lossy(&bytes).into_owned()
        };
        Ok([s(&u.sysname), s(&u.nodename), s(&u.release), s(&u.version), s(&u.machine)])
    }
    #[cfg(not(unix))]
    {
        let sys = match std::env::consts::OS {
            "windows" => "Windows",
            other => other,
        };
        Ok([sys.to_string(), String::new(), String::new(), String::new(), std::env::consts::ARCH.to_string()])
    }
}

/// `(user, system, children_user, children_system, elapsed)` in seconds.
pub fn times() -> R<[f64; 5]> {
    #[cfg(unix)]
    {
        // SAFETY: a zeroed tms is a valid out-parameter for times.
        let mut t: libc::tms = unsafe { std::mem::zeroed() };
        let elapsed = unsafe { libc::times(&mut t) };
        if elapsed == (-1i64) as libc::clock_t {
            return Err(std::io::Error::last_os_error().into());
        }
        let tick = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
        Ok([
            t.tms_utime as f64 / tick,
            t.tms_stime as f64 / tick,
            t.tms_cutime as f64 / tick,
            t.tms_cstime as f64 / tick,
            elapsed as f64 / tick,
        ])
    }
    #[cfg(not(unix))]
    Err(FsError("ENOSYS"))
}

/// The password-database entry of the real user id (`getpwuid(getuid())`). Without an entry (or
/// off Unix) the ids are still set (-1 off Unix) and the strings are absent.
pub struct PasswdEntry {
    pub uid: i64,
    pub gid: i64,
    pub name: Option<String>,
    pub dir: Option<String>,
    pub shell: Option<String>,
}

pub fn current_user() -> PasswdEntry {
    #[cfg(unix)]
    {
        use std::ffi::CStr;
        let text = |p: *const libc::c_char| {
            // SAFETY: a non-null passwd string field is a NUL-terminated C string.
            (!p.is_null()).then(|| unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
        };
        // SAFETY: no arguments, cannot fail.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        // SAFETY: getpwuid returns null or a pointer to static storage valid until the next call.
        let entry = unsafe { libc::getpwuid(uid) };
        let (name, dir, shell) = if entry.is_null() {
            (None, None, None)
        } else {
            unsafe { (text((*entry).pw_name), text((*entry).pw_dir), text((*entry).pw_shell)) }
        };
        PasswdEntry { uid: uid as i64, gid: gid as i64, name, dir, shell }
    }
    #[cfg(not(unix))]
    PasswdEntry { uid: -1, gid: -1, name: None, dir: None, shell: None }
}

/// The 1, 5 and 15 minute load averages (zeros where the OS has none).
pub fn loadavg() -> [f64; 3] {
    #[cfg(unix)]
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
            .and_then(|t| t.split_whitespace().next().and_then(|n| n.parse::<f64>().ok()))
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
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|t| {
                t.lines().find(|l| l.starts_with("MemAvailable:")).and_then(|l| {
                    l.split_whitespace().nth(1).and_then(|n| n.parse::<f64>().ok())
                })
            })
            .map(|kb| kb * 1024.0)
            .unwrap_or(0.0)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    0.0
}

#[cfg(target_os = "macos")]
pub(crate) fn sysctl<T: Default>(name: &str) -> Option<T> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut value = T::default();
    let mut len = std::mem::size_of::<T>();
    // SAFETY: `value` is a writable `T` of `len` bytes.
    let rc = unsafe {
        libc::sysctlbyname(cname.as_ptr(), (&mut value as *mut T).cast(), &mut len, std::ptr::null_mut(), 0)
    };
    (rc == 0).then_some(value)
}

/// A `sysctlbyname` value of up to `buf.len()` bytes; the length written.
#[cfg(target_os = "macos")]
pub(crate) fn sysctl_bytes(name: &str, buf: &mut [u8]) -> Option<usize> {
    let cname = std::ffi::CString::new(name).ok()?;
    let mut len = buf.len();
    // SAFETY: `buf` is writable for `len` bytes and `len` is updated to the bytes written.
    let rc = unsafe { libc::sysctlbyname(cname.as_ptr(), buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    (rc == 0).then_some(len)
}

pub fn cpu_count() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// `(columns, lines)` of the terminal behind `fd`.
pub fn terminal_size(fd: i32) -> R<(u32, u32)> {
    #[cfg(unix)]
    {
        // SAFETY: a zeroed winsize is a valid out-parameter for TIOCGWINSZ.
        let mut w: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut w) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok((w.ws_col as u32, w.ws_row as u32))
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        Err(FsError("ENOSYS"))
    }
}

/// Fills `buf` from the operating system's CSPRNG: `/dev/urandom` on Unix (opened once),
/// `ProcessPrng` on Windows (what std itself uses), `crypto.getRandomValues` on wasm32.
pub fn entropy(buf: &mut [u8]) -> R<()> {
    #[cfg(unix)]
    {
        use std::io::Read;
        static URANDOM: std::sync::OnceLock<std::fs::File> = std::sync::OnceLock::new();
        let file = match URANDOM.get() {
            Some(file) => file,
            None => {
                let opened = std::fs::File::open("/dev/urandom")?;
                URANDOM.get_or_init(|| opened)
            }
        };
        (&*file).read_exact(buf)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        #[link(name = "bcryptprimitives", kind = "raw-dylib")]
        extern "system" {
            fn ProcessPrng(data: *mut u8, len: usize) -> i32;
        }
        // SAFETY: `buf` is a live, writable slice of `len` bytes. Documented to always succeed.
        if unsafe { ProcessPrng(buf.as_mut_ptr(), buf.len()) } == 0 {
            return Err(FsError("EIO"));
        }
        Ok(())
    }
    #[cfg(target_arch = "wasm32")]
    {
        getrandom::getrandom(buf).map_err(|_| FsError("EIO"))
    }
    #[cfg(not(any(unix, windows, target_arch = "wasm32")))]
    {
        let _ = buf;
        Err(FsError("ENOSYS"))
    }
}

/// The process environment as raw byte pairs.
pub fn environ() -> Vec<(Vec<u8>, Vec<u8>)> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        std::env::vars_os().map(|(k, v)| (k.into_vec(), v.into_vec())).collect()
    }
    #[cfg(not(unix))]
    {
        std::env::vars_os().map(|(k, v)| (k.to_string_lossy().into_owned().into_bytes(), v.to_string_lossy().into_owned().into_bytes())).collect()
    }
}

pub fn setenv(key: &str, value: &str) -> R<()> {
    if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
        return Err(FsError("EINVAL"));
    }
    std::env::set_var(key, value);
    Ok(())
}

pub fn unsetenv(key: &str) -> R<()> {
    if key.is_empty() || key.contains('=') || key.contains('\0') {
        return Err(FsError("EINVAL"));
    }
    std::env::remove_var(key);
    Ok(())
}

/// `kill(2)`.
pub fn kill(pid: i32, sig: i32) -> R<()> {
    #[cfg(unix)]
    {
        // SAFETY: kill takes plain integers; the kernel validates them.
        if unsafe { libc::kill(pid, sig) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, sig);
        Err(FsError("ENOSYS"))
    }
}

/// `sysconf(3)`; `Ok(-1)` for a limit that is indeterminate.
pub fn sysconf(name: i32) -> R<i64> {
    #[cfg(unix)]
    {
        // SAFETY: sysconf takes a plain integer.
        let v = unsafe { libc::sysconf(name) };
        if v == -1 {
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINVAL) {
                return Err(e.into());
            }
        }
        Ok(v as i64)
    }
    #[cfg(not(unix))]
    {
        let _ = name;
        Err(FsError("EINVAL"))
    }
}

/// Decoding of a `wait(2)` status word (the `W*` macros).
pub mod wait {
    pub fn if_exited(s: i32) -> bool {
        s & 0x7f == 0
    }
    pub fn exit_status(s: i32) -> i32 {
        (s >> 8) & 0xff
    }
    pub fn if_signaled(s: i32) -> bool {
        let sig = s & 0x7f;
        sig != 0 && sig != 0x7f
    }
    pub fn term_sig(s: i32) -> i32 {
        s & 0x7f
    }
    pub fn if_stopped(s: i32) -> bool {
        s & 0xff == 0x7f && !if_continued(s)
    }
    pub fn stop_sig(s: i32) -> i32 {
        (s >> 8) & 0xff
    }
    pub fn core_dump(s: i32) -> bool {
        s & 0x80 != 0
    }
    pub fn if_continued(s: i32) -> bool {
        if cfg!(any(target_os = "macos", target_os = "ios")) {
            s & 0x7f == 0x7f && (s >> 8) & 0xff == 0x13
        } else {
            s == 0xffff
        }
    }
}

#[cfg(unix)]
fn check(rc: libc::c_int) -> R<libc::c_int> {
    if rc < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(rc)
    }
}

/// `waitpid(2)`: `(pid, status)`; `pid` is 0 when `WNOHANG` found no exited child. Retries on
/// `EINTR`.
pub fn waitpid(pid: i32, options: i32) -> R<(i32, i32)> {
    #[cfg(unix)]
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `status` is a valid out-parameter.
        let r = unsafe { libc::waitpid(pid, &mut status, options) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        return Ok((r, status));
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, options);
        Err(FsError("ENOSYS"))
    }
}

/// `system(3)`: the raw wait status of `/bin/sh -c command`.
pub fn system(command: &str) -> R<i32> {
    #[cfg(unix)]
    {
        let c = std::ffi::CString::new(command).map_err(|_| FsError("EINVAL"))?;
        // SAFETY: `c` is a valid NUL-terminated string.
        Ok(unsafe { libc::system(c.as_ptr()) })
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        Err(FsError("ENOSYS"))
    }
}

/// Process-group and session calls.
pub mod group {
    use super::*;

    pub fn getpgrp() -> i32 {
        #[cfg(unix)]
        {
            // SAFETY: no arguments, cannot fail.
            unsafe { libc::getpgrp() }
        }
        #[cfg(not(unix))]
        0
    }

    pub fn getpgid(pid: i32) -> R<i32> {
        #[cfg(unix)]
        {
            // SAFETY: plain integer argument.
            check(unsafe { libc::getpgid(pid) })
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            Err(FsError("ENOSYS"))
        }
    }

    pub fn getsid(pid: i32) -> R<i32> {
        #[cfg(unix)]
        {
            // SAFETY: plain integer argument.
            check(unsafe { libc::getsid(pid) })
        }
        #[cfg(not(unix))]
        {
            let _ = pid;
            Err(FsError("ENOSYS"))
        }
    }

    pub fn setpgid(pid: i32, pgrp: i32) -> R<()> {
        #[cfg(unix)]
        {
            // SAFETY: plain integer arguments.
            check(unsafe { libc::setpgid(pid, pgrp) })?;
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = (pid, pgrp);
            Err(FsError("ENOSYS"))
        }
    }

    pub fn setsid() -> R<i32> {
        #[cfg(unix)]
        {
            // SAFETY: no arguments.
            check(unsafe { libc::setsid() })
        }
        #[cfg(not(unix))]
        Err(FsError("ENOSYS"))
    }
}

/// The login name of the session's user (`getlogin(3)`).
pub fn getlogin() -> R<String> {
    #[cfg(unix)]
    {
        // SAFETY: getlogin returns null or a pointer to static storage.
        let p = unsafe { libc::getlogin() };
        if p.is_null() {
            let e = std::io::Error::last_os_error();
            return Err(if e.raw_os_error() == Some(0) { FsError("ENOENT") } else { e.into() });
        }
        // SAFETY: a non-null result is a NUL-terminated string.
        Ok(unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned())
    }
    #[cfg(not(unix))]
    Err(FsError("ENOSYS"))
}

/// The supplementary group ids of the process (`getgroups(2)`).
pub fn getgroups() -> R<Vec<u32>> {
    #[cfg(unix)]
    {
        // SAFETY: a zero-length query returns the count.
        let n = check(unsafe { libc::getgroups(0, std::ptr::null_mut()) })?;
        let mut v: Vec<libc::gid_t> = vec![0; n as usize];
        // SAFETY: `v` has room for `n` ids.
        let n = check(unsafe { libc::getgroups(n, v.as_mut_ptr()) })?;
        v.truncate(n as usize);
        Ok(v)
    }
    #[cfg(not(unix))]
    Ok(Vec::new())
}
