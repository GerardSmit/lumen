//! Descriptor control: `dup2`, the close-on-exec ("inheritable") flag and non-blocking mode. On
//! Unix these act on the OS descriptor directly, so they work for descriptors from
//! [`crate::fs::open`] and for ones opened elsewhere alike.

use crate::errno::FsError;

pub type R<T> = Result<T, FsError>;

#[cfg(unix)]
fn check(rc: libc::c_int) -> R<libc::c_int> {
    if rc < 0 {
        Err(std::io::Error::last_os_error().into())
    } else {
        Ok(rc)
    }
}

/// A raw OS `pipe(2)` outside the descriptor table of [`crate::fs`]: `(read, write)`, both
/// close-on-exec. For a runtime's own wake-up and self-pipes.
#[cfg(unix)]
pub fn os_pipe() -> R<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)> {
    use std::os::fd::FromRawFd;
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors pipe writes.
    check(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
    // SAFETY: both descriptors are fresh and owned by nobody else.
    let (r, w) = unsafe {
        (std::os::fd::OwnedFd::from_raw_fd(fds[0]), std::os::fd::OwnedFd::from_raw_fd(fds[1]))
    };
    set_inheritable(fds[0], false)?;
    set_inheritable(fds[1], false)?;
    Ok((r, w))
}

/// `dup2(fd, fd2)`; the new descriptor is close-on-exec unless `inheritable`.
pub fn dup2(fd: i32, fd2: i32, inheritable: bool) -> R<i32> {
    #[cfg(unix)]
    {
        // SAFETY: plain descriptor syscalls; the kernel validates both numbers.
        let r = check(unsafe { libc::dup2(fd, fd2) })?;
        if !inheritable && fd != fd2 {
            set_inheritable(r, false)?;
        }
        Ok(r)
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, fd2, inheritable);
        Err(FsError("ENOSYS"))
    }
}

pub fn get_inheritable(fd: i32) -> R<bool> {
    #[cfg(unix)]
    {
        // SAFETY: F_GETFD only reads the descriptor flags.
        let flags = check(unsafe { libc::fcntl(fd, libc::F_GETFD) })?;
        Ok(flags & libc::FD_CLOEXEC == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        Err(FsError("ENOSYS"))
    }
}

pub fn set_inheritable(fd: i32, inheritable: bool) -> R<()> {
    #[cfg(unix)]
    {
        // SAFETY: F_GETFD/F_SETFD only touch the descriptor flags.
        let flags = check(unsafe { libc::fcntl(fd, libc::F_GETFD) })?;
        let new = if inheritable { flags & !libc::FD_CLOEXEC } else { flags | libc::FD_CLOEXEC };
        if new != flags {
            check(unsafe { libc::fcntl(fd, libc::F_SETFD, new) })?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, inheritable);
        Err(FsError("ENOSYS"))
    }
}

pub fn get_blocking(fd: i32) -> R<bool> {
    #[cfg(unix)]
    {
        // SAFETY: F_GETFL only reads the file status flags.
        let flags = check(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
        Ok(flags & libc::O_NONBLOCK == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = fd;
        Err(FsError("ENOSYS"))
    }
}

pub fn set_blocking(fd: i32, blocking: bool) -> R<()> {
    #[cfg(unix)]
    {
        // SAFETY: F_GETFL/F_SETFL only touch the file status flags.
        let flags = check(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
        let new = if blocking { flags & !libc::O_NONBLOCK } else { flags | libc::O_NONBLOCK };
        if new != flags {
            check(unsafe { libc::fcntl(fd, libc::F_SETFL, new) })?;
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, blocking);
        Err(FsError("ENOSYS"))
    }
}

/// `fcntl(2)` with an integer argument.
pub fn fcntl_int(fd: i32, cmd: i32, arg: i32) -> R<i32> {
    #[cfg(unix)]
    {
        // SAFETY: an integer-argument fcntl command; the kernel validates every value.
        check(unsafe { libc::fcntl(fd, cmd, arg) })
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, cmd, arg);
        Err(FsError("ENOSYS"))
    }
}

/// `fcntl(2)` with a pointer argument: `buf` is passed to the kernel and holds what it wrote.
pub fn fcntl_buf(fd: i32, cmd: i32, buf: &mut [u8]) -> R<i32> {
    #[cfg(unix)]
    {
        // SAFETY: `buf` is a live buffer the caller sized for the command.
        check(unsafe { libc::fcntl(fd, cmd, buf.as_mut_ptr()) })
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, cmd, buf);
        Err(FsError("ENOSYS"))
    }
}

/// `ioctl(2)` with an integer argument.
pub fn ioctl_int(fd: i32, request: u32, arg: i32) -> R<i32> {
    #[cfg(unix)]
    {
        // SAFETY: an integer-argument ioctl request; the kernel validates every value.
        check(unsafe { libc::ioctl(fd, request as _, arg) })
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, request, arg);
        Err(FsError("ENOSYS"))
    }
}

/// `ioctl(2)` with a pointer argument: `buf` is passed to the kernel and holds what it wrote.
pub fn ioctl_buf(fd: i32, request: u32, buf: &mut [u8]) -> R<i32> {
    #[cfg(unix)]
    {
        // SAFETY: `buf` is a live buffer the caller sized for the request.
        check(unsafe { libc::ioctl(fd, request as _, buf.as_mut_ptr()) })
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, request, buf);
        Err(FsError("ENOSYS"))
    }
}

/// `flock(2)`.
pub fn flock(fd: i32, operation: i32) -> R<()> {
    #[cfg(unix)]
    {
        // SAFETY: plain integer arguments.
        check(unsafe { libc::flock(fd, operation) }).map(|_| ())
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, operation);
        Err(FsError("ENOSYS"))
    }
}

/// A POSIX record lock for [`lockf`]: `kind` is `F_RDLCK`, `F_WRLCK` or `F_UNLCK`.
#[derive(Clone, Copy, Debug)]
pub struct RecordLock {
    pub kind: i32,
    pub whence: i32,
    pub start: i64,
    pub len: i64,
    pub wait: bool,
}

/// The `l_type` values of a [`RecordLock`] (they differ between Linux and macOS).
#[cfg(unix)]
pub const F_RDLCK: i32 = libc::F_RDLCK as i32;
#[cfg(unix)]
pub const F_WRLCK: i32 = libc::F_WRLCK as i32;
#[cfg(unix)]
pub const F_UNLCK: i32 = libc::F_UNLCK as i32;
#[cfg(not(unix))]
pub const F_RDLCK: i32 = 0;
#[cfg(not(unix))]
pub const F_WRLCK: i32 = 1;
#[cfg(not(unix))]
pub const F_UNLCK: i32 = 2;

/// `fcntl(F_SETLK / F_SETLKW)` with a `struct flock`, as `fcntl.lockf` does.
pub fn lockf(fd: i32, lock: RecordLock) -> R<()> {
    #[cfg(unix)]
    {
        // SAFETY: a zeroed flock is valid; the fields set below are the portable ones.
        let mut l: libc::flock = unsafe { std::mem::zeroed() };
        l.l_type = lock.kind as _;
        l.l_whence = lock.whence as _;
        l.l_start = lock.start as _;
        l.l_len = lock.len as _;
        let cmd = if lock.wait { libc::F_SETLKW } else { libc::F_SETLK };
        // SAFETY: `l` is a live struct flock.
        check(unsafe { libc::fcntl(fd, cmd, &mut l as *mut libc::flock) }).map(|_| ())
    }
    #[cfg(not(unix))]
    {
        let _ = (fd, lock);
        Err(FsError("ENOSYS"))
    }
}

/// The names and values of the `fcntl` module's constants on this platform.
pub fn fcntl_constants() -> Vec<(&'static str, i64)> {
    let mut v: Vec<(&'static str, i64)> = Vec::new();
    #[cfg(unix)]
    v.extend([
        ("LOCK_SH", 1),
        ("LOCK_EX", 2),
        ("LOCK_NB", 4),
        ("LOCK_UN", 8),
        ("F_DUPFD", libc::F_DUPFD as i64),
        ("F_DUPFD_CLOEXEC", libc::F_DUPFD_CLOEXEC as i64),
        ("F_GETFD", libc::F_GETFD as i64),
        ("F_SETFD", libc::F_SETFD as i64),
        ("F_GETFL", libc::F_GETFL as i64),
        ("F_SETFL", libc::F_SETFL as i64),
        ("F_GETLK", libc::F_GETLK as i64),
        ("F_SETLK", libc::F_SETLK as i64),
        ("F_SETLKW", libc::F_SETLKW as i64),
        ("F_GETOWN", libc::F_GETOWN as i64),
        ("F_SETOWN", libc::F_SETOWN as i64),
        ("F_RDLCK", libc::F_RDLCK as i64),
        ("F_WRLCK", libc::F_WRLCK as i64),
        ("F_UNLCK", libc::F_UNLCK as i64),
        ("FD_CLOEXEC", libc::FD_CLOEXEC as i64),
    ]);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    v.extend([
        ("LOCK_MAND", 32),
        ("LOCK_READ", 64),
        ("LOCK_WRITE", 128),
        ("LOCK_RW", 192),
        ("F_OFD_GETLK", 36),
        ("F_OFD_SETLK", 37),
        ("F_OFD_SETLKW", 38),
        ("F_GETSIG", 11),
        ("F_SETSIG", 10),
        ("F_GETLK64", 5),
        ("F_SETLK64", 6),
        ("F_SETLKW64", 7),
        ("FASYNC", 0o20000),
        ("F_SETLEASE", 1024),
        ("F_GETLEASE", 1025),
        ("F_NOTIFY", 1026),
        ("F_EXLCK", 4),
        ("F_SHLCK", 8),
        ("F_SETPIPE_SZ", 1031),
        ("F_GETPIPE_SZ", 1032),
        ("F_ADD_SEALS", 1033),
        ("F_GET_SEALS", 1034),
        ("F_SEAL_SEAL", 1),
        ("F_SEAL_SHRINK", 2),
        ("F_SEAL_GROW", 4),
        ("F_SEAL_WRITE", 8),
        ("FICLONE", 0x4004_9409),
        ("FICLONERANGE", 0x4020_940d),
        ("DN_ACCESS", 1),
        ("DN_MODIFY", 2),
        ("DN_CREATE", 4),
        ("DN_DELETE", 8),
        ("DN_RENAME", 0x10),
        ("DN_ATTRIB", 0x20),
        ("DN_MULTISHOT", 0x8000_0000),
    ]);
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    v.extend([
        ("F_GETPATH", 50),
        ("F_FULLFSYNC", 51),
        ("F_NOCACHE", 48),
        ("F_RDAHEAD", 45),
        ("FASYNC", 0x40),
        ("F_OFD_GETLK", 92),
        ("F_OFD_SETLK", 90),
        ("F_OFD_SETLKW", 91),
        ("F_GETNOSIGPIPE", 74),
        ("F_SETNOSIGPIPE", 73),
        ("F_GETLEASE", 107),
        ("F_SETLEASE", 106),
    ]);
    v
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn flags_round_trip() {
        let (r, w) = crate::fs::pipe().unwrap();
        assert!(!get_inheritable(r).unwrap());
        set_inheritable(r, true).unwrap();
        assert!(get_inheritable(r).unwrap());
        assert!(get_blocking(w).unwrap());
        set_blocking(w, false).unwrap();
        assert!(!get_blocking(w).unwrap());
        let d = dup2(w, 100, false).unwrap();
        assert_eq!(d, 100);
        assert!(!get_inheritable(d).unwrap());
        crate::fs::close(d).unwrap();
        crate::fs::close(r).unwrap();
        crate::fs::close(w).unwrap();
    }
}
