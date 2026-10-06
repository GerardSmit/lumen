//! Kernel event queues: `epoll` (Linux) and `kqueue` (macOS, BSD), the backends of Python's
//! `select.epoll` and `select.kqueue`. Descriptors are raw and owned by the caller. Where a
//! queue does not exist the constructors fail with `ENOSYS`.

use crate::errno::FsError;

pub type R<T> = Result<T, FsError>;

/// Whether `epoll` exists on this platform.
pub const HAVE_EPOLL: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// Whether `kqueue` exists on this platform.
pub const HAVE_KQUEUE: bool = cfg!(any(target_os = "macos", target_os = "ios", target_os = "freebsd"));

#[cfg(unix)]
fn last() -> FsError {
    std::io::Error::last_os_error().into()
}

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
fn set_cloexec(fd: i32) -> R<()> {
    // SAFETY: fcntl on a descriptor just created by the caller.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
            return Err(last());
        }
    }
    Ok(())
}

/// `epoll_create1`: a new epoll descriptor, close-on-exec.
pub fn epoll_create() -> R<i32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: plain flag argument.
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(last());
        }
        Ok(fd)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    Err(FsError("ENOSYS"))
}

/// One `struct epoll_event`.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub type EpollEvent = libc::epoll_event;

/// Stand-in so signatures exist where `epoll` does not.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
#[derive(Clone, Copy, Debug, Default)]
pub struct EpollEvent {
    pub events: u32,
    pub u64: u64,
}

/// `epoll_ctl(epfd, op, fd, events)`.
pub fn epoll_ctl(epfd: i32, op: i32, fd: i32, events: u32) -> R<()> {
    epoll_ctl_data(epfd, op, fd, events, fd as u64)
}

/// `epoll_ctl` with caller-chosen `data`, returned verbatim by [`epoll_wait_into`].
pub fn epoll_ctl_data(epfd: i32, op: i32, fd: i32, events: u32, data: u64) -> R<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut ev = libc::epoll_event { events, u64: data };
        // SAFETY: `ev` is a live epoll_event.
        if unsafe { libc::epoll_ctl(epfd, op, fd, &mut ev) } < 0 {
            return Err(last());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = (epfd, op, fd, events, data);
        Err(FsError("ENOSYS"))
    }
}

/// `epoll_wait` into `out`, without allocating: the number of events written after at most
/// `timeout_ms` (negative: forever). `EINTR` is returned as an error.
pub fn epoll_wait_into(epfd: i32, out: &mut [EpollEvent], timeout_ms: i32) -> R<usize> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let max = out.len().min(libc::c_int::MAX as usize) as libc::c_int;
        // SAFETY: `out` has room for `max` events.
        let n = unsafe { libc::epoll_wait(epfd, out.as_mut_ptr(), max, timeout_ms) };
        if n < 0 {
            return Err(last());
        }
        Ok(n as usize)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let _ = (epfd, out, timeout_ms);
        Err(FsError("ENOSYS"))
    }
}

/// `epoll_wait`: up to `max` `(fd, events)` pairs after at most `timeout_ms` (negative: forever).
/// `EINTR` is returned as an error.
pub fn epoll_wait(epfd: i32, max: usize, timeout_ms: i32) -> R<Vec<(i32, u32)>> {
    let mut buf = vec![EpollEvent { events: 0, u64: 0 }; max.max(1)];
    let n = epoll_wait_into(epfd, &mut buf, timeout_ms)?;
    Ok(buf[..n].iter().map(|e| { let events = e.events; (e.u64 as i32, events) }).collect())
}

/// `eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC)`: a counter descriptor, for cross-thread wakes.
pub fn eventfd() -> R<i32> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: plain integer arguments.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(last());
        }
        Ok(fd)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    Err(FsError("ENOSYS"))
}

/// The `EPOLL*` constants of this platform.
pub fn epoll_constants() -> Vec<(&'static str, i64)> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        vec![
            ("EPOLLIN", libc::EPOLLIN as i64),
            ("EPOLLOUT", libc::EPOLLOUT as i64),
            ("EPOLLPRI", libc::EPOLLPRI as i64),
            ("EPOLLERR", libc::EPOLLERR as i64),
            ("EPOLLHUP", libc::EPOLLHUP as i64),
            ("EPOLLET", libc::EPOLLET as i64),
            ("EPOLLONESHOT", libc::EPOLLONESHOT as i64),
            ("EPOLLEXCLUSIVE", libc::EPOLLEXCLUSIVE as i64),
            ("EPOLLRDHUP", libc::EPOLLRDHUP as i64),
            ("EPOLLRDNORM", libc::EPOLLRDNORM as i64),
            ("EPOLLRDBAND", libc::EPOLLRDBAND as i64),
            ("EPOLLWRNORM", libc::EPOLLWRNORM as i64),
            ("EPOLLWRBAND", libc::EPOLLWRBAND as i64),
            ("EPOLLMSG", libc::EPOLLMSG as i64),
            ("EPOLL_CLOEXEC", libc::EPOLL_CLOEXEC as i64),
            ("EPOLL_CTL_ADD", libc::EPOLL_CTL_ADD as i64),
            ("EPOLL_CTL_MOD", libc::EPOLL_CTL_MOD as i64),
            ("EPOLL_CTL_DEL", libc::EPOLL_CTL_DEL as i64),
        ]
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    Vec::new()
}

/// One `struct kevent`, with the fields widened to what Python reads and writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Kevent {
    pub ident: usize,
    pub filter: i16,
    pub flags: u16,
    pub fflags: u32,
    pub data: isize,
    pub udata: usize,
}

/// `kqueue(2)`: a new queue descriptor, close-on-exec.
pub fn kqueue() -> R<i32> {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        // SAFETY: no arguments.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(last());
        }
        set_cloexec(fd)?;
        Ok(fd)
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    Err(FsError("ENOSYS"))
}

/// One native `struct kevent`.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
pub type RawKevent = libc::kevent;

/// Stand-in so signatures exist where `kqueue` does not.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
pub type RawKevent = Kevent;

/// `EVFILT_USER`: a filter triggered from user space.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
pub const EVFILT_USER: i16 = libc::EVFILT_USER as i16;
/// `NOTE_TRIGGER`: the `fflags` that fire an `EVFILT_USER` event.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
pub const NOTE_TRIGGER: u32 = libc::NOTE_TRIGGER as u32;

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
impl Kevent {
    /// The native form of this event.
    pub fn to_raw(&self) -> RawKevent {
        // SAFETY: an all-zero kevent is valid; the named fields are then set.
        let mut e: libc::kevent = unsafe { std::mem::zeroed() };
        e.ident = self.ident as _;
        e.filter = self.filter as _;
        e.flags = self.flags as _;
        e.fflags = self.fflags as _;
        e.data = self.data as _;
        e.udata = self.udata as *mut libc::c_void;
        e
    }

    pub fn from_raw(e: &RawKevent) -> Kevent {
        Kevent {
            ident: e.ident as usize,
            filter: e.filter as i16,
            flags: e.flags as u16,
            fflags: e.fflags as u32,
            data: e.data as isize,
            udata: e.udata as usize,
        }
    }
}

/// `kevent(2)` without allocating: applies `changes`, then writes events into `out`, waiting at
/// most `timeout` (seconds, nanoseconds; `None`: forever). Returns the event count. `EINTR` is
/// returned as an error.
pub fn kevent_into(
    kq: i32,
    changes: &[RawKevent],
    out: &mut [RawKevent],
    timeout: Option<(i64, i64)>,
) -> R<usize> {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        let ts = timeout.map(|(s, n)| libc::timespec { tv_sec: s as _, tv_nsec: n as _ });
        let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec);
        // SAFETY: both arrays are live and sized as passed; `tsp` is null or a live timespec.
        let n = unsafe {
            libc::kevent(kq, changes.as_ptr(), changes.len() as _, out.as_mut_ptr(), out.len() as _, tsp)
        };
        if n < 0 {
            return Err(last());
        }
        Ok(n as usize)
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    {
        let _ = (kq, changes, out, timeout);
        Err(FsError("ENOSYS"))
    }
}

/// `kevent(2)`: applies `changes`, then collects up to `max` events, waiting at most `timeout`
/// (seconds, nanoseconds; `None`: forever). `EINTR` is returned as an error.
pub fn kevent(kq: i32, changes: &[Kevent], max: usize, timeout: Option<(i64, i64)>) -> R<Vec<Kevent>> {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        let ch: Vec<RawKevent> = changes.iter().map(Kevent::to_raw).collect();
        // SAFETY: an all-zero kevent is valid.
        let mut out: Vec<RawKevent> = vec![unsafe { std::mem::zeroed() }; max];
        let n = kevent_into(kq, &ch, &mut out, timeout)?;
        Ok(out[..n].iter().map(Kevent::from_raw).collect())
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    {
        let _ = (kq, changes, max, timeout);
        Err(FsError("ENOSYS"))
    }
}

/// The `KQ_*` constants of this platform.
pub fn kqueue_constants() -> Vec<(&'static str, i64)> {
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    {
        vec![
            ("KQ_FILTER_READ", -1),
            ("KQ_FILTER_WRITE", -2),
            ("KQ_FILTER_AIO", -3),
            ("KQ_FILTER_VNODE", -4),
            ("KQ_FILTER_PROC", -5),
            ("KQ_FILTER_SIGNAL", -6),
            ("KQ_FILTER_TIMER", -7),
            ("KQ_EV_ADD", 0x1),
            ("KQ_EV_DELETE", 0x2),
            ("KQ_EV_ENABLE", 0x4),
            ("KQ_EV_DISABLE", 0x8),
            ("KQ_EV_ONESHOT", 0x10),
            ("KQ_EV_CLEAR", 0x20),
            ("KQ_EV_SYSFLAGS", 0xF000),
            ("KQ_EV_FLAG1", 0x2000),
            ("KQ_EV_EOF", 0x8000),
            ("KQ_EV_ERROR", 0x4000),
            ("KQ_NOTE_DELETE", 0x1),
            ("KQ_NOTE_WRITE", 0x2),
            ("KQ_NOTE_EXTEND", 0x4),
            ("KQ_NOTE_ATTRIB", 0x8),
            ("KQ_NOTE_LINK", 0x10),
            ("KQ_NOTE_RENAME", 0x20),
            ("KQ_NOTE_REVOKE", 0x40),
            ("KQ_NOTE_EXIT", 0x8000_0000),
            ("KQ_NOTE_FORK", 0x4000_0000),
            ("KQ_NOTE_EXEC", 0x2000_0000),
            ("KQ_NOTE_PCTRLMASK", -0x10_0000),
            ("KQ_NOTE_PDATAMASK", 0x000f_ffff),
            ("KQ_NOTE_TRACK", 0x1),
            ("KQ_NOTE_CHILD", 0x4),
            ("KQ_NOTE_TRACKERR", 0x2),
            ("KQ_NOTE_LOWAT", 0x1),
        ]
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    Vec::new()
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use super::*;

    #[test]
    fn epoll_reports_a_readable_pipe() {
        let mut fds = [0; 2];
        // SAFETY: a two-int out array.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let ep = epoll_create().unwrap();
        epoll_ctl(ep, libc::EPOLL_CTL_ADD, fds[0], libc::EPOLLIN as u32).unwrap();
        assert!(epoll_wait(ep, 4, 0).unwrap().is_empty());
        // SAFETY: writes one byte from a live buffer.
        unsafe { libc::write(fds[1], b"x".as_ptr().cast(), 1) };
        assert_eq!(epoll_wait(ep, 4, 100).unwrap(), vec![(fds[0], libc::EPOLLIN as u32)]);
        // SAFETY: closing descriptors opened above.
        unsafe {
            libc::close(ep);
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
    }
}
