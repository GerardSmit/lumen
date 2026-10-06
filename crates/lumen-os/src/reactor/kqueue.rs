//! macOS, iOS and FreeBSD: `kqueue` with `EV_ONESHOT` read and write filters and an `EVFILT_USER`
//! event for wakes.

use super::{Backend, Interest, Ready, Source, WAKE_TOKEN};
use crate::event::{self, Kevent, RawKevent, EVFILT_USER, NOTE_TRIGGER};
use crate::sched::SchedError;
use std::time::Duration;

const BATCH: usize = 256;
const WAKE_IDENT: usize = 1;

pub(super) struct KqueueBackend {
    kq: i32,
}

fn change(ident: usize, filter: i16, flags: u16, fflags: u32, token: u64) -> RawKevent {
    Kevent { ident, filter, flags, fflags, data: 0, udata: token as usize }.to_raw()
}

impl KqueueBackend {
    pub(super) fn new() -> Result<Self, SchedError> {
        let kq = event::kqueue().map_err(SchedError::Os)?;
        let backend = Self { kq };
        let add = change(WAKE_IDENT, EVFILT_USER, (libc::EV_ADD | libc::EV_CLEAR) as u16, 0, WAKE_TOKEN);
        event::kevent_into(kq, &[add], &mut [], Some((0, 0))).map_err(SchedError::Os)?;
        Ok(backend)
    }

    fn fd(src: Source) -> Result<usize, SchedError> {
        match src {
            Source::Fd(fd) => Ok(fd as usize),
            Source::Host(_) => Err(SchedError::Unsupported("host sources on a poller")),
        }
    }

    fn submit(&self, changes: &[RawKevent]) -> Result<(), SchedError> {
        event::kevent_into(self.kq, changes, &mut [], Some((0, 0))).map(|_| ()).map_err(SchedError::Os)
    }

    fn arm(&self, src: Source, interest: Interest, token: u64) -> Result<(), SchedError> {
        let fd = Self::fd(src)?;
        let flags = (libc::EV_ADD | libc::EV_ONESHOT) as u16;
        let mut changes = Vec::with_capacity(2);
        if interest.contains(Interest::READ) {
            changes.push(change(fd, libc::EVFILT_READ, flags, 0, token));
        }
        if interest.contains(Interest::WRITE) {
            changes.push(change(fd, libc::EVFILT_WRITE, flags, 0, token));
        }
        self.submit(&changes)
    }

    fn delete(&self, fd: usize, filter: i16) {
        let _ = self.submit(&[change(fd, filter, libc::EV_DELETE as u16, 0, 0)]);
    }
}

impl Drop for KqueueBackend {
    fn drop(&mut self) {
        // SAFETY: closing the descriptor this backend opened.
        unsafe { libc::close(self.kq) };
    }
}

impl Backend for KqueueBackend {
    fn add(&self, src: Source, interest: Interest, token: u64) -> Result<(), SchedError> {
        self.arm(src, interest, token)
    }

    fn rearm(&self, src: Source, interest: Interest, token: u64) -> Result<(), SchedError> {
        self.arm(src, interest, token)?;
        let fd = Self::fd(src)?;
        if !interest.contains(Interest::READ) {
            self.delete(fd, libc::EVFILT_READ);
        }
        if !interest.contains(Interest::WRITE) {
            self.delete(fd, libc::EVFILT_WRITE);
        }
        Ok(())
    }

    fn remove(&self, src: Source, _token: u64) {
        if let Source::Fd(fd) = src {
            self.delete(fd as usize, libc::EVFILT_READ);
            self.delete(fd as usize, libc::EVFILT_WRITE);
        }
    }

    fn fired(&self, src: Source, armed: Interest, fired: Ready) {
        let Source::Fd(fd) = src else { return };
        if armed.contains(Interest::READ) && !fired.contains(Ready::READ) {
            self.delete(fd as usize, libc::EVFILT_READ);
        }
        if armed.contains(Interest::WRITE) && !fired.contains(Ready::WRITE) {
            self.delete(fd as usize, libc::EVFILT_WRITE);
        }
    }

    fn trigger(&self) {
        let _ = self.submit(&[change(WAKE_IDENT, EVFILT_USER, 0, NOTE_TRIGGER, WAKE_TOKEN)]);
    }

    fn wait(&self, timeout: Option<Duration>, out: &mut Vec<(u64, Ready)>) -> Result<bool, SchedError> {
        let ts = timeout.map(|d| (d.as_secs().min(i64::MAX as u64) as i64, i64::from(d.subsec_nanos())));
        // SAFETY: an all-zero kevent is valid.
        let mut buf: [RawKevent; BATCH] = unsafe { std::mem::zeroed() };
        let n = match event::kevent_into(self.kq, &[], &mut buf, ts) {
            Ok(n) => n,
            Err(e) if e.code() == "EINTR" => return Ok(false),
            Err(e) => return Err(SchedError::Os(e)),
        };
        let mut woken = false;
        for raw in &buf[..n] {
            let ev = Kevent::from_raw(raw);
            let token = ev.udata as u64;
            if ev.filter == EVFILT_USER {
                woken = true;
                continue;
            }
            let mut ready = Ready::NONE;
            if ev.filter == libc::EVFILT_READ {
                ready = ready | Ready::READ;
            } else if ev.filter == libc::EVFILT_WRITE {
                ready = ready | Ready::WRITE;
            }
            if ev.flags & libc::EV_EOF as u16 != 0 {
                ready = ready | Ready::HUP;
            }
            if ev.flags & libc::EV_ERROR as u16 != 0 {
                ready = ready | Ready::ERROR;
            }
            out.push((token, ready));
        }
        Ok(woken)
    }
}
