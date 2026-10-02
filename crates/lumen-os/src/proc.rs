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

/// `fork(2)`: the child's pid in the parent, 0 in the child.
pub fn fork() -> R<i32> {
    #[cfg(unix)]
    {
        // SAFETY: the caller runs the fork hooks and keeps the child single-threaded.
        check(unsafe { libc::fork() })
    }
    #[cfg(not(unix))]
    Err(FsError("ENOSYS"))
}

/// `wait4(2)` (`pid` -1 for any child; `wait3` is the same call): `(pid, status, usage)`; `pid`
/// is 0 when `WNOHANG` found no child. Retries on `EINTR`.
pub fn wait4(pid: i32, options: i32) -> R<(i32, i32, crate::rlimit::Rusage)> {
    #[cfg(unix)]
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: a zeroed rusage is a valid out-parameter; `status` is live.
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::wait4(pid, &mut status, options, &mut usage) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        return Ok((r, status, crate::rlimit::rusage_of(&usage)));
    }
    #[cfg(not(unix))]
    {
        let _ = (pid, options);
        Err(FsError("ENOSYS"))
    }
}

/// The `siginfo_t` fields `waitid(2)` fills in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitInfo {
    pub pid: i32,
    pub uid: u32,
    pub signo: i32,
    pub status: i32,
    pub code: i32,
}

/// `waitid(2)`; `None` when `WNOHANG` found no child. Retries on `EINTR`.
pub fn waitid(idtype: i32, id: u32, options: i32) -> R<Option<WaitInfo>> {
    #[cfg(unix)]
    loop {
        // SAFETY: a zeroed siginfo_t is a valid out-parameter for waitid.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::waitid(idtype as _, id as _, &mut info, options) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: waitid filled the union members read here.
        let (pid, uid, status) = unsafe { (info.si_pid(), info.si_uid(), info.si_status()) };
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let (pid, uid, status) = (info.si_pid, info.si_uid, info.si_status);
        if pid == 0 {
            return Ok(None);
        }
        return Ok(Some(WaitInfo { pid, uid: uid as u32, signo: info.si_signo, status, code: info.si_code }));
    }
    #[cfg(not(unix))]
    {
        let _ = (idtype, id, options);
        Err(FsError("ENOSYS"))
    }
}

/// The `CLD_*` codes of a `waitid` result.
pub const CLD_CONSTANTS: [(&str, i64); 6] = [
    ("CLD_EXITED", 1),
    ("CLD_KILLED", 2),
    ("CLD_DUMPED", 3),
    ("CLD_TRAPPED", 4),
    ("CLD_STOPPED", 5),
    ("CLD_CONTINUED", 6),
];

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
