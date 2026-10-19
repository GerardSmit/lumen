//! Linux and Android: `epoll` with `EPOLLONESHOT` registrations and an `eventfd` for wakes.

use super::{Backend, Interest, Ready, Source, WAKE_TOKEN};
use crate::event::{self, EpollEvent};
use crate::sched::SchedError;
use std::time::Duration;

const BATCH: usize = 256;

pub(super) struct EpollBackend {
    epfd: i32,
    wakefd: i32,
}

fn close(fd: i32) {
    // SAFETY: closing a descriptor this module opened.
    unsafe { libc::close(fd) };
}

impl EpollBackend {
    pub(super) fn new() -> Result<Self, SchedError> {
        let epfd = event::epoll_create().map_err(SchedError::Os)?;
        let wakefd = match event::eventfd() {
            Ok(fd) => fd,
            Err(e) => {
                close(epfd);
                return Err(SchedError::Os(e));
            }
        };
        if let Err(e) = event::epoll_ctl_data(epfd, libc::EPOLL_CTL_ADD, wakefd, libc::EPOLLIN as u32, WAKE_TOKEN) {
            close(epfd);
            close(wakefd);
            return Err(SchedError::Os(e));
        }
        Ok(Self { epfd, wakefd })
    }

    fn events_of(interest: Interest) -> u32 {
        let mut events = (libc::EPOLLONESHOT | libc::EPOLLRDHUP) as u32;
        if interest.contains(Interest::READ) {
            events |= libc::EPOLLIN as u32;
        }
        if interest.contains(Interest::WRITE) {
            events |= libc::EPOLLOUT as u32;
        }
        events
    }

    fn fd(src: Source) -> Result<i32, SchedError> {
        match src {
            Source::Fd(fd) => Ok(fd),
            Source::Host(_) => Err(SchedError::Unsupported("host sources on a poller")),
        }
    }

    fn ctl(&self, op: i32, src: Source, interest: Interest, token: u64) -> Result<(), SchedError> {
        let fd = Self::fd(src)?;
        event::epoll_ctl_data(self.epfd, op, fd, Self::events_of(interest), token).map_err(SchedError::Os)
    }
}

impl Drop for EpollBackend {
    fn drop(&mut self) {
        close(self.epfd);
        close(self.wakefd);
    }
}

pub(super) fn ready_of(events: u32) -> Ready {
    let mut ready = Ready::NONE;
    if events & libc::EPOLLIN as u32 != 0 {
        ready = ready | Ready::READ;
    }
    if events & libc::EPOLLOUT as u32 != 0 {
        ready = ready | Ready::WRITE;
    }
    if events & libc::EPOLLERR as u32 != 0 {
        ready = ready | Ready::ERROR;
    }
    if events & (libc::EPOLLHUP | libc::EPOLLRDHUP) as u32 != 0 {
        ready = ready | Ready::HUP;
    }
    ready
}

impl Backend for EpollBackend {
    fn add(&self, src: Source, interest: Interest, token: u64) -> Result<(), SchedError> {
        self.ctl(libc::EPOLL_CTL_ADD, src, interest, token)
    }

    fn rearm(&self, src: Source, interest: Interest, token: u64) -> Result<(), SchedError> {
        self.ctl(libc::EPOLL_CTL_MOD, src, interest, token)
    }

    fn remove(&self, src: Source, _token: u64) {
        if let Source::Fd(fd) = src {
            let _ = event::epoll_ctl_data(self.epfd, libc::EPOLL_CTL_DEL, fd, 0, 0);
        }
    }

    fn trigger(&self) {
        let one = 1u64;
        // SAFETY: writes eight bytes from a live u64 to the eventfd; a full counter (EAGAIN)
        // already means a wake is pending.
        unsafe { libc::write(self.wakefd, (&one as *const u64).cast(), 8) };
    }

    fn wait(&self, timeout: Option<Duration>, out: &mut Vec<(u64, Ready)>) -> Result<bool, SchedError> {
        let timeout_ms = timeout.map_or(-1, |d| {
            d.as_nanos().div_ceil(1_000_000).min(i32::MAX as u128) as i32
        });
        let mut buf = [EpollEvent { events: 0, u64: 0 }; BATCH];
        let n = match event::epoll_wait_into(self.epfd, &mut buf, timeout_ms) {
            Ok(n) => n,
            Err(e) if e.code() == "EINTR" => return Ok(false),
            Err(e) => return Err(SchedError::Os(e)),
        };
        let mut woken = false;
        for ev in &buf[..n] {
            let (events, token) = (ev.events, ev.u64);
            if token == WAKE_TOKEN {
                let mut count = 0u64;
                // SAFETY: reads eight bytes into a live u64; the eventfd is nonblocking.
                unsafe { libc::read(self.wakefd, (&mut count as *mut u64).cast(), 8) };
                woken = true;
            } else {
                out.push((token, ready_of(events)));
            }
        }
        Ok(woken)
    }
}
