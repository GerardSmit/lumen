//! Cancellable waits for the blocking socket threads (unix).
//!
//! A reader or accept thread parks in `poll(2)` on its descriptor plus the read end of the current
//! "epoch" pipe. Cancelling a wait (closing a server, releasing a socket whose descriptor was
//! handed to another process) sets the waiter's flag and retires the epoch: one byte written to the
//! old pipe wakes every thread parked on it, each re-checks its own flag, and the ones still wanted
//! park again on the fresh epoch. This replaces `shutdown(2)` / throwaway connections as the way to
//! wake a thread, which matters once a descriptor is shared with other processes: shutting it down
//! or connecting to it would act on everyone's copy.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

struct Epoch {
    read: OwnedFd,
    write: OwnedFd,
}

static CURRENT: Mutex<Option<Arc<Epoch>>> = Mutex::new(None);

fn new_epoch() -> std::io::Result<Epoch> {
    let os_err = |e: lumen_os::FsError| std::io::Error::other(e.code());
    let (read, write) = lumen_os::fdctl::os_pipe().map_err(os_err)?;
    for fd in [&read, &write] {
        lumen_os::fdctl::set_blocking(fd.as_raw_fd(), false).map_err(os_err)?;
    }
    Ok(Epoch { read, write })
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
pub(super) fn wait(
    fd: RawFd,
    events: libc::c_short,
    cancelled: &AtomicBool,
) -> std::io::Result<bool> {
    loop {
        if cancelled.load(Ordering::SeqCst) {
            return Ok(false);
        }
        let epoch = current()?;
        if cancelled.load(Ordering::SeqCst) {
            return Ok(false);
        }
        use lumen_os::poll::{poll, PollFd, POLLIN};
        let mut fds = [
            PollFd::new(fd, events),
            PollFd::new(epoch.read.as_raw_fd(), POLLIN),
        ];
        if let Err(e) = poll(&mut fds, -1) {
            if e.errno() == libc::EINTR {
                continue;
            }
            return Err(std::io::Error::from_raw_os_error(e.errno()));
        }
        if fds[0].revents != 0 {
            return Ok(true);
        }
    }
}
