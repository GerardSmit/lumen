//! Any Unix: `poll(2)` over the armed registrations plus a self-pipe for wakes. Readiness is
//! level-triggered by the kernel; one-shot is emulated by dropping a registration from the armed
//! set when it fires. The `pollfd` array is rebuilt only when that set changes.

use super::{Backend, Interest, Ready, Source};
use crate::poll::{self, PollFd};
use crate::sched::SchedError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

struct Armed {
    token: u64,
    fd: i32,
    interest: Interest,
    armed: bool,
}

#[derive(Default)]
struct Table {
    entries: Vec<Armed>,
    dirty: bool,
    fds: Vec<PollFd>,
    tokens: Vec<u64>,
}

pub(super) struct PollBackend {
    read: i32,
    write: i32,
    table: Mutex<Table>,
    user_wake: AtomicBool,
}

fn os_error() -> SchedError {
    SchedError::Os(std::io::Error::last_os_error().into())
}

impl PollBackend {
    pub(super) fn new() -> Result<Self, SchedError> {
        let mut fds = [0i32; 2];
        // SAFETY: a two-int out array.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            return Err(os_error());
        }
        for fd in fds {
            // SAFETY: fcntl on descriptors just created above.
            let ok = unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) >= 0
                    && libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) >= 0
            };
            if !ok {
                let err = os_error();
                // SAFETY: closing the descriptors opened above.
                unsafe {
                    libc::close(fds[0]);
                    libc::close(fds[1]);
                }
                return Err(err);
            }
        }
        let table = Table { dirty: true, ..Table::default() };
        Ok(Self { read: fds[0], write: fds[1], table: Mutex::new(table), user_wake: AtomicBool::new(false) })
    }

    fn table(&self) -> MutexGuard<'_, Table> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn fd(src: Source) -> Result<i32, SchedError> {
        match src {
            Source::Fd(fd) => Ok(fd),
            Source::Host(_) => Err(SchedError::Unsupported("host sources on a poller")),
        }
    }

    /// Interrupts a blocked `poll` so it sees the changed table.
    fn kick(&self) {
        let byte = 1u8;
        // SAFETY: writes one byte from a live local; a full pipe already means a wake is pending.
        unsafe { libc::write(self.write, (&byte as *const u8).cast(), 1) };
    }

    fn drain(&self) {
        let mut buf = [0u8; 64];
        // SAFETY: reads into a live buffer from the nonblocking pipe.
        while unsafe { libc::read(self.read, buf.as_mut_ptr().cast(), buf.len()) } == buf.len() as isize {}
    }

    fn modify(&self, f: impl FnOnce(&mut Vec<Armed>)) {
        let mut table = self.table();
        f(&mut table.entries);
        table.dirty = true;
        drop(table);
        self.kick();
    }
}

impl Drop for PollBackend {
    fn drop(&mut self) {
        // SAFETY: closing the pipe this backend opened.
        unsafe {
            libc::close(self.read);
            libc::close(self.write);
        }
    }
}

fn events_of(interest: Interest) -> i16 {
    let mut events = 0;
    if interest.contains(Interest::READ) {
        events |= poll::POLLIN;
    }
    if interest.contains(Interest::WRITE) {
        events |= poll::POLLOUT;
    }
    events
}

fn ready_of(revents: i16) -> Ready {
    let mut ready = Ready::NONE;
    if revents & poll::POLLIN != 0 {
        ready = ready | Ready::READ;
    }
    if revents & poll::POLLOUT != 0 {
        ready = ready | Ready::WRITE;
    }
    if revents & (poll::POLLERR | poll::POLLNVAL) != 0 {
        ready = ready | Ready::ERROR;
    }
    if revents & poll::POLLHUP != 0 {
        ready = ready | Ready::HUP;
    }
    ready
}

impl Backend for PollBackend {
    fn add(&self, src: Source, interest: Interest, token: u64) -> Result<(), SchedError> {
        let fd = Self::fd(src)?;
        self.modify(|entries| entries.push(Armed { token, fd, interest, armed: true }));
        Ok(())
    }

    fn rearm(&self, _src: Source, interest: Interest, token: u64) -> Result<(), SchedError> {
        self.modify(|entries| {
            if let Some(e) = entries.iter_mut().find(|e| e.token == token) {
                e.interest = interest;
                e.armed = true;
            }
        });
        Ok(())
    }

    fn remove(&self, _src: Source, token: u64) {
        self.modify(|entries| entries.retain(|e| e.token != token));
    }

    fn trigger(&self) {
        self.user_wake.store(true, Ordering::Release);
        self.kick();
    }

    fn wait(&self, timeout: Option<Duration>, out: &mut Vec<(u64, Ready)>) -> Result<bool, SchedError> {
        let deadline = timeout.and_then(|t| Instant::now().checked_add(t));
        loop {
            let (mut fds, tokens) = {
                let mut table = self.table();
                if table.dirty {
                    let table = &mut *table;
                    table.fds.clear();
                    table.tokens.clear();
                    table.fds.push(PollFd::new(self.read, poll::POLLIN));
                    for e in table.entries.iter().filter(|e| e.armed) {
                        table.fds.push(PollFd::new(e.fd, events_of(e.interest)));
                        table.tokens.push(e.token);
                    }
                    table.dirty = false;
                }
                (std::mem::take(&mut table.fds), std::mem::take(&mut table.tokens))
            };
            let timeout_ms = match (timeout, deadline) {
                (Some(_), Some(deadline)) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    left.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32
                }
                (Some(_), None) | (None, _) => -1,
            };
            for fd in fds.iter_mut() {
                fd.revents = 0;
            }
            let polled = poll::poll(&mut fds, timeout_ms);
            let result = match polled {
                Ok(_) => Ok(()),
                Err(e) if e.code() == "EINTR" => Ok(()),
                Err(e) => Err(SchedError::Os(e)),
            };
            let mut fired = Vec::new();
            if result.is_ok() {
                if fds[0].revents != 0 {
                    self.drain();
                }
                for (fd, &token) in fds[1..].iter().zip(&tokens) {
                    if fd.revents != 0 {
                        out.push((token, ready_of(fd.revents)));
                        fired.push(token);
                    }
                }
            }
            {
                let mut table = self.table();
                if !fired.is_empty() {
                    for e in table.entries.iter_mut().filter(|e| fired.contains(&e.token)) {
                        e.armed = false;
                    }
                    table.dirty = true;
                }
                if !table.dirty {
                    table.fds = fds;
                    table.tokens = tokens;
                }
            }
            result?;
            let woken = self.user_wake.swap(false, Ordering::AcqRel);
            if woken || !out.is_empty() || deadline.is_some_and(|d| Instant::now() >= d) {
                return Ok(woken);
            }
        }
    }
}
