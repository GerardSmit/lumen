//! Windows: an I/O completion port. `GetQueuedCompletionStatusEx` waits, `PostQueuedCompletionStatus`
//! with [`WAKE_KEY`] wakes, and socket readiness arrives as completion packets from
//! `ProcessSocketNotifications` (one-shot, level-triggered registrations; Windows 10 build 20348
//! and later, resolved at run time from `ws2_32`). Where that function is missing or a probe
//! registration fails, one helper thread blocks in `WSAPoll` over the armed sockets plus a
//! loopback UDP wake socket and posts the same kind of packet to the port. Only documented
//! Win32 and Winsock calls are used.
//!
//! Completion keys are never reused: every registration gets a fresh key, so a late packet of a
//! dropped registration finds no entry and is ignored.

use super::{Backend, Interest, Ready, Source};
use crate::poll::win::{ensure_winsock, wsa_poll, WsaPollFd};
use crate::sched::SchedError;
use crate::FsError;
use std::collections::HashMap;
use std::ffi::c_void;
use std::net::UdpSocket;
use std::os::windows::io::AsRawSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

type Handle = isize;

const BATCH: usize = 256;
const WAKE_KEY: usize = 0;
const INFINITE: u32 = 0xFFFF_FFFF;
const WAIT_TIMEOUT: i32 = 258;
const INVALID_HANDLE_VALUE: Handle = -1;

const EVENT_IN: u32 = 0x01;
const EVENT_OUT: u32 = 0x02;
const EVENT_HANGUP: u32 = 0x04;
const EVENT_ERR: u32 = 0x40;
const EVENT_REMOVE: u32 = 0x80;

const FILTER_IN: u16 = 0x01;
const FILTER_OUT: u16 = 0x02;
const FILTER_HANGUP: u16 = 0x04;
const OP_ENABLE: u8 = 0x01;
const OP_REMOVE: u8 = 0x04;
const TRIGGER_ONESHOT_LEVEL: u8 = 0x01 | 0x04;

const POLLRDNORM: i16 = 0x100;
const POLLWRNORM: i16 = 0x10;
const POLLERR: i16 = 0x1;
const POLLHUP: i16 = 0x2;
const POLLNVAL: i16 = 0x4;

#[repr(C)]
#[derive(Clone, Copy)]
struct OverlappedEntry {
    key: usize,
    overlapped: *mut c_void,
    internal: usize,
    bytes: u32,
}

const EMPTY_ENTRY: OverlappedEntry =
    OverlappedEntry { key: 0, overlapped: std::ptr::null_mut(), internal: 0, bytes: 0 };

#[repr(C)]
struct SockNotifyRegistration {
    socket: usize,
    completion_key: *mut c_void,
    event_filter: u16,
    operation: u8,
    trigger_flags: u8,
    registration_result: u32,
}

type NotifyFn = unsafe extern "system" fn(
    port: Handle,
    count: u32,
    regs: *mut SockNotifyRegistration,
    timeout_ms: u32,
    completion_count: u32,
    entries: *mut OverlappedEntry,
    received: *mut u32,
) -> u32;

#[link(name = "kernel32")]
extern "system" {
    fn CreateIoCompletionPort(file: Handle, existing: Handle, key: usize, threads: u32) -> Handle;
    fn GetQueuedCompletionStatusEx(
        port: Handle,
        entries: *mut OverlappedEntry,
        count: u32,
        removed: *mut u32,
        timeout_ms: u32,
        alertable: i32,
    ) -> i32;
    fn PostQueuedCompletionStatus(port: Handle, bytes: u32, key: usize, overlapped: *mut c_void) -> i32;
    fn CloseHandle(h: Handle) -> i32;
    fn LoadLibraryA(name: *const u8) -> Handle;
    fn GetProcAddress(module: Handle, name: *const u8) -> *const c_void;
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn os_code(code: u32) -> SchedError {
    SchedError::Os(FsError::from(std::io::Error::from_raw_os_error(code as i32)))
}

fn io_error(e: std::io::Error) -> SchedError {
    SchedError::Os(FsError::from(e))
}

fn socket_of(src: Source) -> Result<usize, SchedError> {
    match src {
        Source::Socket(s) => Ok(s),
        Source::Host(_) => Err(SchedError::Unsupported("host sources on a poller")),
    }
}

fn post(port: Handle, bytes: u32, key: usize) -> bool {
    // SAFETY: posts a packet with no OVERLAPPED to a port handle that is open.
    unsafe { PostQueuedCompletionStatus(port, bytes, key, std::ptr::null_mut()) != 0 }
}

fn ready_of(events: u32) -> Ready {
    let mut ready = Ready::NONE;
    if events & EVENT_IN != 0 {
        ready = ready | Ready::READ;
    }
    if events & EVENT_OUT != 0 {
        ready = ready | Ready::WRITE;
    }
    if events & EVENT_ERR != 0 {
        ready = ready | Ready::ERROR;
    }
    if events & EVENT_HANGUP != 0 {
        ready = ready | Ready::HUP;
    }
    ready
}

#[derive(Default)]
struct Keys {
    next: usize,
    by_key: HashMap<usize, u64>,
    by_token: HashMap<u64, usize>,
}

impl Keys {
    fn insert(&mut self, token: u64) -> usize {
        loop {
            self.next = self.next.wrapping_add(1);
            if self.next != WAKE_KEY && !self.by_key.contains_key(&self.next) {
                break;
            }
        }
        self.by_key.insert(self.next, token);
        self.by_token.insert(token, self.next);
        self.next
    }

    fn take(&mut self, token: u64) -> Option<usize> {
        let key = self.by_token.remove(&token)?;
        self.by_key.remove(&key);
        Some(key)
    }
}

enum Mode {
    Notify(NotifyFn),
    Poll(Fallback),
}

pub(super) struct IocpBackend {
    port: Handle,
    keys: Mutex<Keys>,
    mode: Mode,
}

fn resolve_notify() -> Option<NotifyFn> {
    // SAFETY: loads a system DLL by a NUL-terminated name and looks up a NUL-terminated export.
    unsafe {
        let lib = LoadLibraryA(b"ws2_32.dll\0".as_ptr());
        if lib == 0 {
            return None;
        }
        let p = GetProcAddress(lib, b"ProcessSocketNotifications\0".as_ptr());
        if p.is_null() {
            return None;
        }
        Some(std::mem::transmute::<*const c_void, NotifyFn>(p))
    }
}

fn notify(
    f: NotifyFn,
    port: Handle,
    socket: usize,
    key: usize,
    operation: u8,
    filter: u16,
) -> Result<(), SchedError> {
    let mut reg = SockNotifyRegistration {
        socket,
        completion_key: key as *mut c_void,
        event_filter: filter,
        operation,
        trigger_flags: TRIGGER_ONESHOT_LEVEL,
        registration_result: 0,
    };
    // SAFETY: one live registration, no completions requested (so the entry and count
    // pointers may be null and the timeout must be zero).
    let rc = unsafe { f(port, 1, &mut reg, 0, 0, std::ptr::null_mut(), std::ptr::null_mut()) };
    if rc != 0 {
        return Err(os_code(rc));
    }
    if reg.registration_result != 0 {
        return Err(os_code(reg.registration_result));
    }
    Ok(())
}

fn filter_of(interest: Interest) -> u16 {
    let mut filter = FILTER_HANGUP;
    if interest.contains(Interest::READ) {
        filter |= FILTER_IN;
    }
    if interest.contains(Interest::WRITE) {
        filter |= FILTER_OUT;
    }
    filter
}

/// Registers and removes a loopback UDP socket, to find out whether the notification API really
/// works on this system.
fn probe(f: NotifyFn, port: Handle) -> bool {
    let Ok(sock) = UdpSocket::bind("127.0.0.1:0") else { return false };
    let socket = sock.as_raw_socket() as usize;
    let ok = notify(f, port, socket, usize::MAX, OP_ENABLE, FILTER_IN).is_ok();
    if ok {
        let _ = notify(f, port, socket, usize::MAX, OP_REMOVE, 0);
    }
    ok
}

impl IocpBackend {
    pub(super) fn new() -> Result<Self, SchedError> {
        Self::create(false)
    }

    /// A backend on the `WSAPoll` helper thread even where `ProcessSocketNotifications` exists.
    #[cfg(test)]
    pub(super) fn with_fallback() -> Result<Self, SchedError> {
        Self::create(true)
    }

    fn create(force_fallback: bool) -> Result<Self, SchedError> {
        ensure_winsock();
        // SAFETY: creates a fresh port, not associated with any handle.
        let port = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, 0, 0, 0) };
        if port == 0 {
            return Err(io_error(std::io::Error::last_os_error()));
        }
        let notify_fn = if force_fallback { None } else { resolve_notify().filter(|f| probe(*f, port)) };
        let mode = match notify_fn {
            Some(f) => Mode::Notify(f),
            None => match Fallback::start(port) {
                Ok(fb) => Mode::Poll(fb),
                Err(e) => {
                    // SAFETY: closing the port opened above.
                    unsafe { CloseHandle(port) };
                    return Err(e);
                }
            },
        };
        Ok(Self { port, keys: Mutex::new(Keys::default()), mode })
    }

    #[cfg(test)]
    pub(super) fn is_fallback(&self) -> bool {
        matches!(self.mode, Mode::Poll(_))
    }

    fn arm(&self, key: usize, socket: usize, interest: Interest) -> Result<(), SchedError> {
        match &self.mode {
            Mode::Notify(f) => notify(*f, self.port, socket, key, OP_ENABLE, filter_of(interest)),
            Mode::Poll(fb) => {
                fb.arm(key, socket, interest);
                Ok(())
            }
        }
    }
}

impl Drop for IocpBackend {
    fn drop(&mut self) {
        if let Mode::Poll(fb) = &self.mode {
            fb.stop();
        }
        // SAFETY: closing the port this backend created; the helper thread is joined.
        unsafe { CloseHandle(self.port) };
    }
}

impl Backend for IocpBackend {
    fn add(&self, src: Source, interest: Interest, token: u64) -> Result<(), SchedError> {
        let socket = socket_of(src)?;
        let key = lock(&self.keys).insert(token);
        let result = self.arm(key, socket, interest);
        if result.is_err() {
            lock(&self.keys).take(token);
        }
        result
    }

    fn rearm(&self, src: Source, interest: Interest, token: u64) -> Result<(), SchedError> {
        let socket = socket_of(src)?;
        let key = lock(&self.keys)
            .by_token
            .get(&token)
            .copied()
            .ok_or_else(|| SchedError::Exhausted("registration is gone".into()))?;
        self.arm(key, socket, interest)
    }

    fn remove(&self, src: Source, token: u64) {
        let Some(key) = lock(&self.keys).take(token) else { return };
        let Ok(socket) = socket_of(src) else { return };
        match &self.mode {
            Mode::Notify(f) => {
                let _ = notify(*f, self.port, socket, key, OP_REMOVE, 0);
            }
            Mode::Poll(fb) => fb.remove(key),
        }
    }

    fn trigger(&self) {
        post(self.port, 0, WAKE_KEY);
    }

    fn wait(&self, timeout: Option<Duration>, out: &mut Vec<(u64, Ready)>) -> Result<bool, SchedError> {
        let timeout_ms = timeout.map_or(INFINITE, |d| {
            d.as_nanos().div_ceil(1_000_000).min(u128::from(INFINITE - 1)) as u32
        });
        let mut buf = [EMPTY_ENTRY; BATCH];
        let mut removed = 0u32;
        // SAFETY: `buf` holds BATCH entries and `removed` is a live out parameter.
        let ok = unsafe {
            GetQueuedCompletionStatusEx(self.port, buf.as_mut_ptr(), BATCH as u32, &mut removed, timeout_ms, 0)
        };
        if ok == 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(WAIT_TIMEOUT) {
                return Ok(false);
            }
            return Err(io_error(err));
        }
        let keys = lock(&self.keys);
        let mut woken = false;
        for entry in &buf[..removed as usize] {
            if entry.key == WAKE_KEY {
                woken = true;
                continue;
            }
            if entry.bytes & EVENT_REMOVE != 0 {
                continue;
            }
            let Some(&token) = keys.by_key.get(&entry.key) else { continue };
            let ready = ready_of(entry.bytes);
            if !ready.is_empty() {
                out.push((token, ready));
            }
        }
        Ok(woken)
    }
}

struct FbEntry {
    key: usize,
    socket: usize,
    events: i16,
    armed: bool,
}

struct FbShared {
    port: Handle,
    entries: Mutex<Vec<FbEntry>>,
    stop: AtomicBool,
    wake_rx: UdpSocket,
    wake_tx: UdpSocket,
}

fn events_of(revents: i16) -> u32 {
    let mut events = 0;
    if revents & POLLRDNORM != 0 {
        events |= EVENT_IN;
    }
    if revents & POLLWRNORM != 0 {
        events |= EVENT_OUT;
    }
    if revents & (POLLERR | POLLNVAL) != 0 {
        events |= EVENT_ERR;
    }
    if revents & POLLHUP != 0 {
        events |= EVENT_HANGUP;
    }
    events
}

impl FbShared {
    fn kick(&self) {
        let _ = self.wake_tx.send(&[1]);
    }

    fn drain(&self) {
        let mut buf = [0u8; 64];
        while self.wake_rx.recv(&mut buf).is_ok() {}
    }

    fn run(&self) {
        let mut fds: Vec<WsaPollFd> = Vec::new();
        let mut keys: Vec<usize> = Vec::new();
        while !self.stop.load(Ordering::Acquire) {
            fds.clear();
            keys.clear();
            fds.push(WsaPollFd { socket: self.wake_rx.as_raw_socket() as usize, events: POLLRDNORM, revents: 0 });
            for e in lock(&self.entries).iter().filter(|e| e.armed) {
                fds.push(WsaPollFd { socket: e.socket, events: e.events, revents: 0 });
                keys.push(e.key);
            }
            if wsa_poll(&mut fds, -1).is_err() {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            if fds[0].revents != 0 {
                self.drain();
            }
            let mut entries = lock(&self.entries);
            for (fd, &key) in fds[1..].iter().zip(&keys) {
                if fd.revents == 0 {
                    continue;
                }
                if let Some(e) = entries.iter_mut().find(|e| e.key == key && e.armed) {
                    e.armed = false;
                    post(self.port, events_of(fd.revents), key);
                }
            }
        }
    }
}

struct Fallback {
    shared: Arc<FbShared>,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Fallback {
    fn start(port: Handle) -> Result<Self, SchedError> {
        let wake_rx = UdpSocket::bind("127.0.0.1:0").map_err(io_error)?;
        wake_rx.set_nonblocking(true).map_err(io_error)?;
        let wake_tx = UdpSocket::bind("127.0.0.1:0").map_err(io_error)?;
        wake_tx.connect(wake_rx.local_addr().map_err(io_error)?).map_err(io_error)?;
        wake_tx.set_nonblocking(true).map_err(io_error)?;
        let shared = Arc::new(FbShared {
            port,
            entries: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
            wake_rx,
            wake_tx,
        });
        let worker = shared.clone();
        let handle = std::thread::Builder::new()
            .name("lumen-wsapoll".into())
            .spawn(move || worker.run())
            .map_err(io_error)?;
        Ok(Self { shared, thread: Mutex::new(Some(handle)) })
    }

    fn arm(&self, key: usize, socket: usize, interest: Interest) {
        let mut events = 0;
        if interest.contains(Interest::READ) {
            events |= POLLRDNORM;
        }
        if interest.contains(Interest::WRITE) {
            events |= POLLWRNORM;
        }
        {
            let mut entries = lock(&self.shared.entries);
            match entries.iter_mut().find(|e| e.key == key) {
                Some(e) => {
                    e.events = events;
                    e.armed = true;
                }
                None => entries.push(FbEntry { key, socket, events, armed: true }),
            }
        }
        self.shared.kick();
    }

    fn remove(&self, key: usize) {
        lock(&self.shared.entries).retain(|e| e.key != key);
        self.shared.kick();
    }

    fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.kick();
        if let Some(handle) = lock(&self.thread).take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reactor::{Poller, Reactor, Wake};
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    const LONG: Duration = Duration::from_secs(30);
    const SHORT: Duration = Duration::from_millis(100);

    fn tcp_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let a = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (b, _) = listener.accept().unwrap();
        (a, b)
    }

    fn counter() -> (Arc<AtomicUsize>, Arc<dyn Wake>) {
        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        (count, Arc::new(move || {
            c.fetch_add(1, SeqCst);
        }))
    }

    fn native() -> Poller {
        Poller::with_backend(Box::new(IocpBackend::new().unwrap()))
    }

    fn fallback() -> Poller {
        let backend = IocpBackend::with_fallback().unwrap();
        assert!(backend.is_fallback());
        Poller::with_backend(Box::new(backend))
    }

    macro_rules! backend_tests {
        ($name:ident, $make:path) => {
            mod $name {
                use super::*;

                #[test]
                fn socket_read_readiness() {
                    let poller = $make();
                    let (a, mut b) = tcp_pair();
                    let (count, wake) = counter();
                    let reg = poller
                        .register(Source::Socket(a.as_raw_socket() as usize), Interest::READ, wake)
                        .unwrap();
                    assert_eq!(poller.turn(Some(SHORT)).unwrap(), 0);
                    assert_eq!(count.load(SeqCst), 0);
                    b.write_all(b"x").unwrap();
                    assert_eq!(poller.turn(Some(LONG)).unwrap(), 1);
                    assert_eq!(count.load(SeqCst), 1);
                    assert!(reg.take_ready().contains(Ready::READ));
                }

                #[test]
                fn registration_is_one_shot_until_rearmed() {
                    let poller = $make();
                    let (mut a, mut b) = tcp_pair();
                    let (count, wake) = counter();
                    let reg = poller
                        .register(Source::Socket(a.as_raw_socket() as usize), Interest::READ, wake)
                        .unwrap();
                    b.write_all(b"x").unwrap();
                    assert_eq!(poller.turn(Some(LONG)).unwrap(), 1);
                    assert_eq!(poller.turn(Some(SHORT)).unwrap(), 0);
                    assert_eq!(count.load(SeqCst), 1);
                    let mut byte = [0u8; 1];
                    a.read_exact(&mut byte).unwrap();
                    b.write_all(b"y").unwrap();
                    assert_eq!(poller.turn(Some(SHORT)).unwrap(), 0);
                    reg.rearm(Interest::READ).unwrap();
                    assert_eq!(poller.turn(Some(LONG)).unwrap(), 1);
                    assert_eq!(count.load(SeqCst), 2);
                }

                #[test]
                fn writable_socket_is_ready_at_once() {
                    let poller = $make();
                    let (a, _b) = tcp_pair();
                    let (count, wake) = counter();
                    let reg = poller
                        .register(Source::Socket(a.as_raw_socket() as usize), Interest::WRITE, wake)
                        .unwrap();
                    assert_eq!(poller.turn(Some(LONG)).unwrap(), 1);
                    assert_eq!(count.load(SeqCst), 1);
                    assert!(reg.take_ready().contains(Ready::WRITE));
                }

                #[test]
                fn peer_close_reports_hangup_or_read() {
                    let poller = $make();
                    let (a, b) = tcp_pair();
                    let (_count, wake) = counter();
                    let reg = poller
                        .register(Source::Socket(a.as_raw_socket() as usize), Interest::READ, wake)
                        .unwrap();
                    drop(b);
                    assert_eq!(poller.turn(Some(LONG)).unwrap(), 1);
                    let ready = reg.take_ready();
                    assert!(ready.contains(Ready::READ) || ready.contains(Ready::HUP));
                }

                #[test]
                fn dropped_registration_is_not_woken() {
                    let poller = $make();
                    let (a, mut b) = tcp_pair();
                    let (count, wake) = counter();
                    let reg = poller
                        .register(Source::Socket(a.as_raw_socket() as usize), Interest::READ, wake)
                        .unwrap();
                    drop(reg);
                    b.write_all(b"x").unwrap();
                    assert_eq!(poller.turn(Some(SHORT)).unwrap(), 0);
                    assert_eq!(count.load(SeqCst), 0);
                }

                #[test]
                fn waker_interrupts_a_blocked_turn_from_another_thread() {
                    let poller = $make();
                    let waker = poller.waker();
                    let t = std::thread::spawn(move || {
                        std::thread::sleep(SHORT);
                        waker.wake();
                    });
                    let start = std::time::Instant::now();
                    assert_eq!(poller.turn(Some(LONG)).unwrap(), 0);
                    assert!(start.elapsed() < LONG);
                    t.join().unwrap();
                }

                #[test]
                fn wake_burst_costs_one_trigger() {
                    let poller = $make();
                    let waker = poller.waker();
                    for _ in 0..100 {
                        waker.wake();
                    }
                    assert_eq!(poller.trigger_count(), 1);
                    poller.turn(Some(LONG)).unwrap();
                    waker.wake();
                    assert_eq!(poller.trigger_count(), 2);
                    poller.turn(Some(LONG)).unwrap();
                }

                #[test]
                fn turn_times_out() {
                    let poller = $make();
                    let start = std::time::Instant::now();
                    assert_eq!(poller.turn(Some(Duration::from_millis(50))).unwrap(), 0);
                    assert!(start.elapsed() >= Duration::from_millis(40));
                }
            }
        };
    }

    backend_tests!(native_backend, native);
    backend_tests!(wsapoll_fallback, fallback);
}
