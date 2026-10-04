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
        (
            std::os::fd::OwnedFd::from_raw_fd(fds[0]),
            std::os::fd::OwnedFd::from_raw_fd(fds[1]),
        )
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
        let new = if inheritable {
            flags & !libc::FD_CLOEXEC
        } else {
            flags | libc::FD_CLOEXEC
        };
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
        let new = if blocking {
            flags & !libc::O_NONBLOCK
        } else {
            flags | libc::O_NONBLOCK
        };
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
