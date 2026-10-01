//! Cancellable waits for the blocking socket threads (unix).
//!
//! A reader or accept thread parks in `poll(2)` on its descriptor plus the read end of the current
//! "epoch" pipe. Cancelling a wait (closing a server, releasing a socket whose descriptor was
//! handed to another process) sets the waiter's flag and retires the epoch: one byte written to the
//! old pipe wakes every thread parked on it, each re-checks its own flag, and the ones still wanted
//! park again on the fresh epoch. This replaces `shutdown(2)` / throwaway connections as the way to
//! wake a thread, which matters once a descriptor is shared with other processes: shutting it down
//! or connecting to it would act on everyone's copy.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct Epoch {
    read: OwnedFd,
    write: OwnedFd,
}

static CURRENT: Mutex<Option<Arc<Epoch>>> = Mutex::new(None);

fn new_epoch() -> std::io::Result<Epoch> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a valid two-element array for pipe(2).
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    for fd in fds {
        // SAFETY: both descriptors were just created by pipe(2).
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    // SAFETY: the descriptors are fresh and owned by nobody else.
    Ok(unsafe {
        Epoch {
            read: OwnedFd::from_raw_fd(fds[0]),
            write: OwnedFd::from_raw_fd(fds[1]),
        }
    })
}

fn current() -> std::io::Result<Arc<Epoch>> {
    let mut slot = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(epoch) = slot.as_ref() {
        return Ok(epoch.clone());
    }
    let epoch = Arc::new(new_epoch()?);
    *slot = Some(epoch.clone());
    Ok(epoch)
}

/// Wake every parked waiter so each re-checks its cancel flag. Set the flag first.
pub(super) fn wake_all() {
    let old = CURRENT.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(epoch) = old {
        let byte = 1u8;
        // SAFETY: writing one byte from a live buffer to the epoch's own pipe.
        unsafe { libc::write(epoch.write.as_raw_fd(), (&byte as *const u8).cast(), 1) };
    }
}

/// Park until `fd` reports one of `events` (or an error/hangup): `Ok(true)`. `Ok(false)` once
/// `cancelled` is set and [`wake_all`] has run.
pub(super) fn wait(fd: RawFd, events: libc::c_short, cancelled: &AtomicBool) -> std::io::Result<bool> {
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let epoch = current()?;
        if cancelled.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let mut fds = [
            libc::pollfd { fd, events, revents: 0 },
            libc::pollfd { fd: epoch.read.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: `fds` is a valid array of two pollfd entries.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fds[0].revents != 0 {
            return Ok(true);
        }
    }
}
