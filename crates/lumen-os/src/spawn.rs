//! `fork` + `exec` with CPython `_posixsubprocess` semantics: descriptor plumbing, session and
//! credential changes in the child, and exec failures reported to the parent through an error
//! pipe as `"<ExceptionName>:<hex errno>:<message>"`.

use crate::errno::FsError;

pub type R<T> = Result<T, FsError>;

/// Everything the child does between `fork` and `exec`. Descriptors are `-1` when unused.
#[derive(Default)]
pub struct ForkExec {
    /// The `argv` of the new program; `None` passes an empty one.
    pub args: Option<Vec<Vec<u8>>>,
    /// Paths tried in turn (the `PATH` search done by the caller).
    pub executables: Vec<Vec<u8>>,
    /// `KEY=value` entries; `None` inherits the environment.
    pub env: Option<Vec<Vec<u8>>>,
    pub cwd: Option<Vec<u8>>,
    pub close_fds: bool,
    /// Sorted; kept open (and made inheritable) when `close_fds`.
    pub fds_to_keep: Vec<i32>,
    pub p2cread: i32,
    pub p2cwrite: i32,
    pub c2pread: i32,
    pub c2pwrite: i32,
    pub errread: i32,
    pub errwrite: i32,
    pub errpipe_read: i32,
    pub errpipe_write: i32,
    pub restore_signals: bool,
    pub call_setsid: bool,
    /// `setpgid(0, pgid)` when non-negative.
    pub pgid: i32,
    pub gid: Option<u32>,
    /// `setgroups` when `Some` (an empty list clears the supplementary groups).
    pub extra_groups: Option<Vec<u32>>,
    pub uid: Option<u32>,
    /// `umask` when non-negative.
    pub umask: i32,
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::ffi::CString;

    fn cstring(b: &[u8]) -> R<CString> {
        CString::new(b).map_err(|_| FsError("EINVAL"))
    }

    fn cstrings(list: &[Vec<u8>]) -> R<Vec<CString>> {
        list.iter().map(|b| cstring(b)).collect()
    }

    fn null_terminated(list: &[CString]) -> Vec<*const libc::c_char> {
        list.iter()
            .map(|c| c.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect()
    }

    /// Forks and execs per `cfg`; `preexec` runs in the child just before the descriptors are
    /// closed and returns `false` on failure. Returns the child's pid. Strings with an interior
    /// NUL fail with `EINVAL` before forking.
    pub fn fork_exec(cfg: &ForkExec, preexec: Option<&mut dyn FnMut() -> bool>) -> R<i32> {
        let args = match &cfg.args {
            Some(a) => cstrings(a)?,
            None => Vec::new(),
        };
        let exes = cstrings(&cfg.executables)?;
        let env = cfg.env.as_deref().map(cstrings).transpose()?;
        let cwd = cfg.cwd.as_deref().map(cstring).transpose()?;
        let argv = null_terminated(&args);
        let exec_array = null_terminated(&exes);
        let envp = env.as_deref().map(null_terminated);
        let groups: Option<Vec<libc::gid_t>> = cfg
            .extra_groups
            .as_ref()
            .map(|g| g.iter().map(|&g| g as libc::gid_t).collect());
        let child = Child {
            cfg,
            argv: &argv,
            exec_array: &exec_array,
            envp: envp.as_deref(),
            cwd: cwd.as_deref(),
            groups: groups.as_deref(),
        };
        // SAFETY: the child only makes async-signal-safe calls on memory prepared above (and
        // whatever `preexec` does, which is the caller's contract, as in CPython).
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if pid == 0 {
            // SAFETY: in the child; never returns.
            unsafe { child.run(preexec) }
        }
        Ok(pid)
    }

    struct Child<'a> {
        cfg: &'a ForkExec,
        argv: &'a [*const libc::c_char],
        exec_array: &'a [*const libc::c_char],
        envp: Option<&'a [*const libc::c_char]>,
        cwd: Option<&'a std::ffi::CStr>,
        groups: Option<&'a [libc::gid_t]>,
    }

    unsafe fn errno() -> i32 {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    unsafe fn set_errno(v: i32) {
        *crate::errno::errno_location() = v;
    }

    unsafe fn set_inheritable(fd: i32, inheritable: bool) -> bool {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 {
            return false;
        }
        let new = if inheritable {
            flags & !libc::FD_CLOEXEC
        } else {
            flags | libc::FD_CLOEXEC
        };
        new == flags || libc::fcntl(fd, libc::F_SETFD, new) >= 0
    }

    unsafe fn write_all(fd: i32, mut b: &[u8]) {
        while !b.is_empty() {
            let n = libc::write(fd, b.as_ptr().cast(), b.len());
            if n < 0 && errno() == libc::EINTR {
                continue;
            }
            if n <= 0 {
                return;
            }
            b = &b[n as usize..];
        }
    }

    /// Closes every descriptor from 3 up except `keep` (sorted).
    unsafe fn close_open_fds(keep: &[i32]) {
        let max = match libc::sysconf(libc::_SC_OPEN_MAX) {
            n if n > 0 => n.min(1 << 20) as i32,
            _ => 256,
        };
        let mut start = 3;
        for &k in keep.iter().chain(std::iter::once(&max)) {
            if k < start {
                continue;
            }
            close_range(start, k);
            start = k + 1;
        }
    }

    /// Closes `[from, to)`.
    unsafe fn close_range(from: i32, to: i32) {
        #[cfg(target_os = "linux")]
        if libc::syscall(
            libc::SYS_close_range,
            from as libc::c_uint,
            (to - 1) as libc::c_uint,
            0,
        ) == 0
        {
            return;
        }
        for fd in from..to {
            libc::close(fd);
        }
    }

    impl Child<'_> {
        unsafe fn run(&self, preexec: Option<&mut dyn FnMut() -> bool>) -> ! {
            let err_msg = self.exec(preexec);
            let saved = errno();
            let w = self.cfg.errpipe_write;
            if saved != 0 {
                write_all(w, b"OSError:");
                let mut hex = [0u8; 8];
                let mut at = hex.len();
                let mut e = saved as u32;
                while e != 0 {
                    at -= 1;
                    hex[at] = b"0123456789ABCDEF"[(e % 16) as usize];
                    e /= 16;
                }
                write_all(w, &hex[at..]);
                write_all(w, b":");
            } else {
                write_all(w, b"SubprocessError:0:");
            }
            write_all(w, err_msg.as_bytes());
            libc::_exit(255)
        }

        /// The child's work up to `exec`; returns the message for the error pipe, with `errno`
        /// set (0 for a non-OS failure).
        unsafe fn exec(&self, preexec: Option<&mut dyn FnMut() -> bool>) -> &'static str {
            let c = self.cfg;
            macro_rules! posix {
                ($e:expr) => {
                    if $e == -1 {
                        return "noexec";
                    }
                };
            }
            for &fd in &c.fds_to_keep {
                if fd != c.errpipe_write && !set_inheritable(fd, true) {
                    return "noexec";
                }
            }
            for fd in [c.p2cwrite, c.c2pread, c.errread] {
                if fd != -1 {
                    posix!(libc::close(fd));
                }
            }
            posix!(libc::close(c.errpipe_read));
            let mut c2pwrite = c.c2pwrite;
            let mut errwrite = c.errwrite;
            if c2pwrite == 0 {
                c2pwrite = libc::dup(c2pwrite);
                posix!(c2pwrite);
                if !set_inheritable(c2pwrite, false) {
                    return "noexec";
                }
            }
            while errwrite == 0 || errwrite == 1 {
                errwrite = libc::dup(errwrite);
                posix!(errwrite);
                if !set_inheritable(errwrite, false) {
                    return "noexec";
                }
            }
            for (fd, target) in [(c.p2cread, 0), (c2pwrite, 1), (errwrite, 2)] {
                if fd == target {
                    if !set_inheritable(fd, true) {
                        return "noexec";
                    }
                } else if fd != -1 {
                    posix!(libc::dup2(fd, target));
                }
            }
            if let Some(cwd) = self.cwd {
                if libc::chdir(cwd.as_ptr()) == -1 {
                    return "noexec:chdir";
                }
            }
            if c.umask >= 0 {
                libc::umask(c.umask as libc::mode_t);
            }
            if c.restore_signals {
                crate::signal::restore_child_defaults();
            }
            if c.call_setsid {
                posix!(libc::setsid());
            }
            if c.pgid >= 0 {
                posix!(libc::setpgid(0, c.pgid));
            }
            if let Some(groups) = self.groups {
                posix!(libc::setgroups(groups.len() as _, groups.as_ptr()));
            }
            if let Some(gid) = c.gid {
                posix!(libc::setregid(gid, gid));
            }
            if let Some(uid) = c.uid {
                posix!(libc::setreuid(uid, uid));
            }
            if let Some(f) = preexec {
                if !f() {
                    set_errno(0);
                    return "Exception occurred in preexec_fn.";
                }
            }
            if c.close_fds {
                close_open_fds(&c.fds_to_keep);
            }
            let mut saved = 0;
            for &exe in &self.exec_array[..self.exec_array.len() - 1] {
                match self.envp {
                    Some(envp) => libc::execve(exe, self.argv.as_ptr(), envp.as_ptr()),
                    None => libc::execv(exe, self.argv.as_ptr()),
                };
                let e = errno();
                if e != libc::ENOENT && e != libc::ENOTDIR && saved == 0 {
                    saved = e;
                }
            }
            if saved != 0 {
                set_errno(saved);
            }
            ""
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    pub fn fork_exec(_cfg: &ForkExec, _preexec: Option<&mut dyn FnMut() -> bool>) -> R<i32> {
        Err(FsError("ENOSYS"))
    }
}

pub use imp::fork_exec;
