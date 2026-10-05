//! Descriptor readiness through `poll(2)` and `select(2)`: the Python `select` module and the runtimes' parked
//! readers and stdin waits.

use crate::errno::FsError;

/// One `struct pollfd`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C)]
pub struct PollFd {
    pub fd: i32,
    pub events: i16,
    pub revents: i16,
}

impl PollFd {
    pub fn new(fd: i32, events: i16) -> PollFd {
        PollFd {
            fd,
            events,
            revents: 0,
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::*;

    pub const POLLIN: i16 = libc::POLLIN;
    pub const POLLPRI: i16 = libc::POLLPRI;
    pub const POLLOUT: i16 = libc::POLLOUT;
    pub const POLLERR: i16 = libc::POLLERR;
    pub const POLLHUP: i16 = libc::POLLHUP;
    pub const POLLNVAL: i16 = libc::POLLNVAL;
    pub const FD_SETSIZE: usize = libc::FD_SETSIZE;

    pub fn constants() -> &'static [(&'static str, i32)] {
        &[
            ("POLLIN", libc::POLLIN as i32),
            ("POLLPRI", libc::POLLPRI as i32),
            ("POLLOUT", libc::POLLOUT as i32),
            ("POLLERR", libc::POLLERR as i32),
            ("POLLHUP", libc::POLLHUP as i32),
            ("POLLNVAL", libc::POLLNVAL as i32),
            ("POLLRDNORM", libc::POLLRDNORM as i32),
            ("POLLRDBAND", libc::POLLRDBAND as i32),
            ("POLLWRNORM", libc::POLLWRNORM as i32),
            ("POLLWRBAND", libc::POLLWRBAND as i32),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            ("POLLMSG", 0x400),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            ("POLLRDHUP", libc::POLLRDHUP as i32),
            ("PIPE_BUF", libc::PIPE_BUF as i32),
        ]
    }

    /// Waits up to `timeout_ms` (negative: forever) for any of `select`'s descriptor lists to
    /// become ready; returns a ready flag per descriptor of each list. `EINVAL` when a descriptor
    /// is at or above `FD_SETSIZE`; `EINTR` as for [`poll`].
    pub fn select(lists: [&[i32]; 3], timeout_ms: i32) -> Result<[Vec<bool>; 3], FsError> {
        // SAFETY: zeroed fd_sets are valid empty sets, FD_SET/FD_ISSET get fds below
        // FD_SETSIZE only, and every pointer passed to select(2) is a live local.
        unsafe {
            let mut sets: [libc::fd_set; 3] = std::mem::zeroed();
            let mut nfds = 0;
            for (set, list) in sets.iter_mut().zip(lists) {
                for &fd in list {
                    if fd < 0 || fd as usize >= FD_SETSIZE {
                        return Err(FsError("EINVAL"));
                    }
                    libc::FD_SET(fd, set);
                    nfds = nfds.max(fd + 1);
                }
            }
            let mut tv = libc::timeval {
                tv_sec: (timeout_ms.max(0) / 1000) as _,
                tv_usec: ((timeout_ms.max(0) % 1000) * 1000) as _,
            };
            let tvp = if timeout_ms < 0 {
                std::ptr::null_mut()
            } else {
                &mut tv as *mut libc::timeval
            };
            let [r, w, x] = &mut sets;
            if libc::select(nfds, r, w, x, tvp) < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let flags = |set: &libc::fd_set, list: &[i32]| {
                list.iter().map(|&fd| libc::FD_ISSET(fd, set)).collect()
            };
            Ok([
                flags(&sets[0], lists[0]),
                flags(&sets[1], lists[1]),
                flags(&sets[2], lists[2]),
            ])
        }
    }

    /// Waits up to `timeout_ms` (negative: forever) for any of `fds`; returns how many have
    /// `revents` set. `EINTR` is returned as an error, for the caller to handle signals.
    pub fn poll(fds: &mut [PollFd], timeout_ms: i32) -> Result<usize, FsError> {
        // SAFETY: `PollFd` has `struct pollfd`'s layout and `fds` is a live slice.
        let n = unsafe {
            libc::poll(
                fds.as_mut_ptr().cast::<libc::pollfd>(),
                fds.len() as libc::nfds_t,
                timeout_ms,
            )
        };
        if n < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(n as usize)
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    pub const POLLIN: i16 = 0x100;
    pub const POLLPRI: i16 = 0x400;
    pub const POLLOUT: i16 = 0x10;
    pub const POLLERR: i16 = 0x1;
    pub const POLLHUP: i16 = 0x2;
    pub const POLLNVAL: i16 = 0x4;
    pub const FD_SETSIZE: usize = 64;

    pub fn constants() -> &'static [(&'static str, i32)] {
        &[]
    }

    pub fn select(_lists: [&[i32]; 3], _timeout_ms: i32) -> Result<[Vec<bool>; 3], FsError> {
        Err(FsError("ENOSYS"))
    }

    pub fn poll(_fds: &mut [PollFd], _timeout_ms: i32) -> Result<usize, FsError> {
        Err(FsError("ENOSYS"))
    }
}

pub use imp::*;
