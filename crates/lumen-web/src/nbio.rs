//! Nonblocking sockets on the loop's readiness reactor.
//!
//! The server listener, every server connection, and the WebSocket and EventSource clients run on
//! this layer instead of on a pool thread blocked in `accept`/`read`. A connection owns a
//! one-shot [`Registration`] with the loop's reactor (`LoopReactor` in the realm's `OpState`) and
//! one [`TaskRegistry`] task, which is what keeps the loop alive while the socket is open and what
//! carries the connection's work back to the loop thread with a `&mut Ctx`:
//!
//! 1. The reactor wakes the registration on the loop thread, inside the loop's own turn. The wake
//!    only raises a flag ([`Remote::raise`]) and sends a completion for the connection's current
//!    task.
//! 2. The task's decoder runs [`Link::take`] to claim the flags, reads until `WouldBlock` (a TLS
//!    session may hold decrypted bytes the socket will never signal again), produces at most one
//!    event for JS, registers the next task ([`Link::next_task`]) and rearms the registration.
//!
//! One task per wake keeps the old event granularity: every message or chunk is its own loop
//! completion with a microtask checkpoint behind it. Anything that has to reach the connection
//! from another thread (a stall timer, a shutdown request) raises a flag the same way; a flag
//! raised while a decode is running is noticed by [`Link::next_task`], so no wake is lost.
//!
//! Nothing here polls: an idle connection has no thread, no timer and no wake-up. Stall timers
//! ([`Conn::watch_reads`], the write timeout) are scheduler timers that exist only while bytes
//! are owed, and they re-arm themselves lazily instead of being rebuilt per read or write.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use lumen_host::loop_reactor::LoopReactor;
use lumen_host::{register_task, CompletionSender, Ctx, TaskDecoder, TaskRegistry, Value};
use lumen_os::reactor::{Interest, Registration, Source};
use lumen_os::sched::Timer;

/// The reactor reported the socket ready.
pub(crate) const IO: u8 = 1;
/// The owner left work behind (a buffered message, bytes it did not read) and wants another pass.
pub(crate) const RESUME: u8 = 2;
/// The read-inactivity timer fired.
pub(crate) const READ_STALL: u8 = 4;
/// The write-stall timer fired.
pub(crate) const WRITE_STALL: u8 = 8;
/// The owner asked for the connection to be shut down.
pub(crate) const CLOSE: u8 = 16;

/// A peer that accepts no bytes for this long is dropped.
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the blocking part of a client handshake (TLS, request, response head) waits for the
/// peer before the connection attempt fails.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes one wake reads before yielding to the rest of the loop.
const FILL_BUDGET: usize = 256 << 10;
const MIN_READ: usize = 4 << 10;
const INITIAL_RX: usize = 16 << 10;
const RELEASE_ABOVE: usize = 1 << 20;
const NO_TASK: u64 = u64::MAX;

/// A socket the reactor can watch: a plain `TcpStream` or a TLS session over one.
pub(crate) trait NbStream: Read + Write + Send + 'static {
    fn source(&self) -> Source;
}

#[cfg(unix)]
pub(crate) fn source_of(socket: &impl std::os::fd::AsRawFd) -> Source {
    Source::Fd(socket.as_raw_fd())
}

#[cfg(windows)]
pub(crate) fn source_of(socket: &impl std::os::windows::io::AsRawSocket) -> Source {
    Source::Socket(socket.as_raw_socket() as usize)
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn source_of<T>(_: &T) -> Source {
    Source::Host(0)
}

impl NbStream for TcpStream {
    fn source(&self) -> Source {
        source_of(self)
    }
}

impl NbStream for lumen_tls::TlsStream {
    fn source(&self) -> Source {
        source_of(self.socket())
    }
}

impl NbStream for Box<dyn NbStream> {
    fn source(&self) -> Source {
        (**self).source()
    }
}

pub(crate) fn listener_source(listener: &TcpListener) -> Source {
    source_of(listener)
}

struct Shared {
    task: AtomicU64,
    flags: AtomicU8,
}

/// The `Send` half of a connection's wake path: what reactor wakes and timers hold.
#[derive(Clone)]
pub(crate) struct Remote {
    shared: Arc<Shared>,
    sender: CompletionSender,
    id: u64,
}

impl Remote {
    /// Flags `flag` and wakes the connection's current task, from any thread.
    pub(crate) fn raise(&self, flag: u8) {
        self.shared.flags.fetch_or(flag, Ordering::SeqCst);
        let task = self.shared.task.load(Ordering::SeqCst);
        if task != NO_TASK {
            self.sender.send(task, Box::new(self.id));
        }
    }
}

/// The loop-thread half: registers the connection's tasks and claims its flags.
#[derive(Clone)]
pub(crate) struct Link {
    remote: Remote,
    on_ok: Value,
    decode: TaskDecoder,
}

impl Link {
    fn new(ctx: &mut Ctx, id: u64, on_ok: Value, decode: TaskDecoder) -> Result<Link, String> {
        let sender = ctx
            .op_state()
            .get::<CompletionSender>()
            .cloned()
            .ok_or("sockets require an event loop")?;
        let shared = Arc::new(Shared {
            task: AtomicU64::new(NO_TASK),
            flags: AtomicU8::new(0),
        });
        Ok(Link {
            remote: Remote { shared, sender, id },
            on_ok,
            decode,
        })
    }

    pub(crate) fn remote(&self) -> Remote {
        self.remote.clone()
    }

    /// Claims the flags raised since the last call.
    pub(crate) fn take(&self) -> u8 {
        self.remote.shared.flags.swap(0, Ordering::SeqCst)
    }

    /// Flags work for the next pass without waking anything: [`Link::next_task`] does that once.
    pub(crate) fn mark(&self, flag: u8) {
        self.remote.shared.flags.fetch_or(flag, Ordering::SeqCst);
    }

    /// Registers the task the next wake completes. A flag raised since [`Link::take`], when the
    /// previous task was already gone, is delivered to this one.
    pub(crate) fn next_task(&self, ctx: &mut Ctx) {
        let task = register_task(ctx, self.on_ok.clone(), None, self.decode);
        self.remote.shared.task.store(task, Ordering::SeqCst);
        if self.remote.shared.flags.load(Ordering::SeqCst) != 0 {
            self.remote.sender.send(task, Box::new(self.remote.id));
        }
    }

    /// Drops the pending task, so the loop no longer waits for this connection.
    pub(crate) fn cancel(&self, ctx: &mut Ctx) {
        let task = self.remote.shared.task.swap(NO_TASK, Ordering::SeqCst);
        if task != NO_TASK {
            if let Some(tasks) = ctx.host_mut::<TaskRegistry>() {
                tasks.cancel(task);
            }
        }
    }
}

/// A reactor registration plus its task link. Dropping it deregisters the source, so it must be
/// declared before the socket it watches.
pub(crate) struct Watch {
    reg: Registration,
    link: Link,
}

impl Watch {
    /// Watches `source` for `interest` on the realm's loop. `on_ok` receives the arguments the
    /// decoder returns; the decoder gets the connection's `id` as a `Box<u64>` payload.
    pub(crate) fn open(
        ctx: &mut Ctx,
        source: Source,
        interest: Interest,
        id: u64,
        on_ok: Value,
        decode: TaskDecoder,
    ) -> Result<Watch, String> {
        let reactor = ctx
            .op_state()
            .get::<LoopReactor>()
            .and_then(LoopReactor::reactor)
            .ok_or("this platform has no readiness reactor")?;
        let link = Link::new(ctx, id, on_ok, decode)?;
        let remote = link.remote();
        let reg = reactor
            .register(source, interest, Arc::new(move || remote.raise(IO)))
            .map_err(|error| error.to_string())?;
        link.next_task(ctx);
        Ok(Watch { reg, link })
    }

    pub(crate) fn link(&self) -> &Link {
        &self.link
    }

    pub(crate) fn rearm(&self, interest: Interest) -> io::Result<()> {
        self.reg.rearm(interest).map_err(io::Error::other)
    }
}

/// Received bytes not consumed yet.
#[derive(Default)]
pub(crate) struct RecvBuf {
    data: Vec<u8>,
    start: usize,
    end: usize,
}

impl RecvBuf {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.data[self.start..self.end]
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.start == self.end
    }

    pub(crate) fn consume(&mut self, count: usize) {
        self.start = (self.start + count).min(self.end);
        if self.start == self.end {
            self.start = 0;
            self.end = 0;
            if self.data.len() > RELEASE_ABOVE {
                self.data = Vec::new();
            }
        }
    }

    fn read_from(&mut self, reader: &mut impl Read) -> io::Result<usize> {
        if self.data.len() - self.end < MIN_READ {
            if self.start > 0 {
                self.data.copy_within(self.start..self.end, 0);
                self.end -= self.start;
                self.start = 0;
            }
            if self.data.len() - self.end < MIN_READ {
                let size = (self.data.len() * 2).max(INITIAL_RX);
                self.data.resize(size, 0);
            }
        }
        let count = reader.read(&mut self.data[self.end..])?;
        self.end += count;
        Ok(count)
    }
}

/// How a [`Conn::fill`] ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fill {
    /// The socket has nothing more for now.
    Drained,
    /// The peer closed its end.
    Eof,
    /// The wake's read budget ran out with bytes possibly left.
    More,
}

/// Which timer a [`Conn::on_stall`] answered for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stalled {
    Read,
    Write,
}

impl Stalled {
    pub(crate) fn error(self) -> io::Error {
        let what = match self {
            Stalled::Read => "read timed out",
            Stalled::Write => "write timed out",
        };
        io::Error::new(io::ErrorKind::TimedOut, what)
    }
}

/// A nonblocking connection on the reactor: the socket, its receive buffer, its queued output and
/// its stall timers.
pub(crate) struct Conn<S: NbStream> {
    watch: Watch,
    stream: S,
    pub(crate) rx: RecvBuf,
    out: Vec<u8>,
    out_pos: usize,
    /// The TLS session took bytes whose records the socket has not taken yet.
    unflushed: bool,
    sent_total: u64,
    recv_total: u64,
    write_timer: Option<Timer>,
    write_mark: u64,
    read_limit: Option<Duration>,
    read_timer: Option<Timer>,
    read_mark: u64,
}

impl<S: NbStream> Conn<S> {
    /// Takes over a nonblocking `stream` and watches it for `interest`.
    pub(crate) fn open(
        ctx: &mut Ctx,
        stream: S,
        interest: Interest,
        id: u64,
        on_ok: Value,
        decode: TaskDecoder,
    ) -> Result<Conn<S>, String> {
        let watch = Watch::open(ctx, stream.source(), interest, id, on_ok, decode)?;
        Ok(Conn {
            watch,
            stream,
            rx: RecvBuf::default(),
            out: Vec::new(),
            out_pos: 0,
            unflushed: false,
            sent_total: 0,
            recv_total: 0,
            write_timer: None,
            write_mark: 0,
            read_limit: None,
            read_timer: None,
            read_mark: 0,
        })
    }

    pub(crate) fn link(&self) -> Link {
        self.watch.link().clone()
    }

    /// Ends the watch and returns the socket; the caller cancels the task through its [`Link`].
    pub(crate) fn into_stream(self) -> S {
        let Conn { watch, stream, .. } = self;
        drop(watch);
        stream
    }

    /// Whether output is owed to the socket.
    pub(crate) fn pending(&self) -> bool {
        self.out_pos < self.out.len() || self.unflushed
    }

    /// Bytes queued behind the socket.
    pub(crate) fn queued(&self) -> usize {
        self.out.len() - self.out_pos
    }

    /// Waits for readability (`read`) and, while output is queued, writability. With nothing to
    /// wait for the registration stays disarmed.
    pub(crate) fn rearm(&self, read: bool) -> io::Result<()> {
        let mut interest = Interest::default();
        if read {
            interest = interest | Interest::READ;
        }
        if self.pending() {
            interest = interest | Interest::WRITE;
        }
        if interest.is_empty() {
            return Ok(());
        }
        self.watch.rearm(interest)
    }

    /// Reads until the socket would block, the peer closes or the wake's budget is spent.
    /// Bytes read before an error stay in [`Conn::rx`].
    pub(crate) fn fill(&mut self) -> io::Result<Fill> {
        let mut budget = FILL_BUDGET;
        loop {
            match self.rx.read_from(&mut self.stream) {
                Ok(0) => return Ok(Fill::Eof),
                Ok(count) => {
                    self.recv_total += count as u64;
                    budget = budget.saturating_sub(count);
                    if budget == 0 {
                        return Ok(Fill::More);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(Fill::Drained),
                Err(error) => return Err(error),
            }
        }
    }

    /// Sends `bytes` after anything already queued. What the socket does not take now is queued
    /// and goes out when the registration reports it writable.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut rest = bytes;
        if !self.pending() {
            let count = write_some(&mut self.stream, rest)?;
            self.sent_total += count as u64;
            rest = &rest[count..];
            if rest.is_empty() {
                self.flush_stream()?;
                self.sync_write_timer();
                return Ok(());
            }
        }
        self.out.extend_from_slice(rest);
        self.sync_write_timer();
        Ok(())
    }

    /// Sends queued output as far as the socket takes it.
    pub(crate) fn flush(&mut self) -> io::Result<()> {
        while self.out_pos < self.out.len() {
            let count = write_some(&mut self.stream, &self.out[self.out_pos..])?;
            self.out_pos += count;
            self.sent_total += count as u64;
            if count == 0 {
                break;
            }
        }
        if self.out_pos == self.out.len() {
            self.out.clear();
            self.out_pos = 0;
            if self.out.capacity() > RELEASE_ABOVE {
                self.out = Vec::new();
            }
            self.flush_stream()?;
        }
        self.sync_write_timer();
        Ok(())
    }

    fn flush_stream(&mut self) -> io::Result<()> {
        match self.stream.flush() {
            Ok(()) => self.unflushed = false,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => self.unflushed = true,
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn timer(&self, flag: u8, delay: Duration) -> Option<Timer> {
        let remote = self.watch.link().remote();
        lumen_os::sched::current()
            .after(delay, Box::new(move || remote.raise(flag)))
            .ok()
    }

    /// Runs the write timer exactly while output is owed.
    fn sync_write_timer(&mut self) {
        if !self.pending() {
            self.write_timer = None;
        } else if self.write_timer.is_none() {
            self.write_mark = self.sent_total;
            self.write_timer = self.timer(WRITE_STALL, WRITE_TIMEOUT);
        }
    }

    /// Fails the connection with [`Stalled::Read`] when `limit` passes without a byte arriving.
    pub(crate) fn watch_reads(&mut self, limit: Duration) {
        self.read_limit = Some(limit);
        self.read_mark = self.recv_total;
        self.read_timer = self.timer(READ_STALL, limit);
    }

    /// Answers a stall flag: `Some` when the connection made no progress since the timer was
    /// armed, else the timer is armed again for another period.
    pub(crate) fn on_stall(&mut self, flags: u8) -> Option<Stalled> {
        if flags & WRITE_STALL != 0 {
            self.write_timer = None;
            if self.pending() {
                if self.sent_total == self.write_mark {
                    return Some(Stalled::Write);
                }
                self.sync_write_timer();
            }
        }
        if flags & READ_STALL != 0 {
            self.read_timer = None;
            if let Some(limit) = self.read_limit {
                if self.recv_total == self.read_mark {
                    return Some(Stalled::Read);
                }
                self.watch_reads(limit);
            }
        }
        None
    }
}

/// Writes until `bytes` is out or the socket would block; returns how much went out.
pub(crate) fn write_some(writer: &mut impl Write, bytes: &[u8]) -> io::Result<usize> {
    let mut done = 0;
    while done < bytes.len() {
        match writer.write(&bytes[done..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(count) => done += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(error),
        }
    }
    Ok(done)
}

/// Why [`read_head`] gave up.
pub(crate) enum HeadError {
    TooLarge,
    Io(io::Error),
}

/// Reads a response head up to its blank line from a blocking socket, one byte at a time so that
/// nothing behind the head is consumed. A stalled peer fails the read after [`HANDSHAKE_TIMEOUT`]
/// (the socket carries that read timeout), instead of pinning the pool thread.
pub(crate) fn read_head(stream: &mut impl Read, limit: usize) -> Result<Vec<u8>, HeadError> {
    let deadline = std::time::Instant::now() + HANDSHAKE_TIMEOUT;
    let mut head = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() > limit {
            return Err(HeadError::TooLarge);
        }
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(HeadError::Io(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "failed to fill whole buffer",
                )))
            }
            Ok(_) => head.push(byte[0]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) && std::time::Instant::now() < deadline => {}
            Err(error) => return Err(HeadError::Io(error)),
        }
    }
    Ok(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recv_buf_compacts_and_grows_without_losing_bytes() {
        let mut buf = RecvBuf::default();
        let mut source: &[u8] = &[7u8; 20_000];
        let mut total = 0;
        loop {
            let count = buf.read_from(&mut source).unwrap();
            if count == 0 {
                break;
            }
            total += count;
            buf.consume(count / 2);
            total -= count / 2;
        }
        assert_eq!(buf.bytes().len(), total);
        assert!(buf.bytes().iter().all(|byte| *byte == 7));
        buf.consume(total);
        assert!(buf.is_empty());
    }

    #[test]
    fn write_some_stops_at_would_block() {
        struct Limited(Vec<u8>, usize);
        impl Write for Limited {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.1 == 0 {
                    return Err(io::ErrorKind::WouldBlock.into());
                }
                let count = bytes.len().min(self.1);
                self.1 -= count;
                self.0.extend_from_slice(&bytes[..count]);
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut sink = Limited(Vec::new(), 5);
        assert_eq!(write_some(&mut sink, b"0123456789").unwrap(), 5);
        assert_eq!(sink.0, b"01234");
    }
}
