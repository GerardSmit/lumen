//! Real TCP (`node:net`) and UDP (`node:dgram`) sockets on `std::net`, exposed to JS as the
//! `__net` and `__udp` op namespaces (the JS glue in `js/net.js` / `js/dgram.js` wraps them into
//! the public Socket/Server/dgram.Socket surface).
//!
//! ## How it runs on the loop
//! The engine is single-threaded and `!Send`, so JS callbacks never leave the loop thread. Socket
//! I/O is readiness-driven on the loop's reactor ([`ready::Io`], the runtime's
//! `lumen_os::reactor::Poller`); each operation settles through the [`TaskRegistry`] as a
//! [`TaskCompletion`], exactly like `node:child_process` (see `child.rs`):
//! - **reads / accept / recv / write / send-message** try their syscall on the loop thread at
//!   once. Only on `WouldBlock` is the descriptor's one-shot registration armed; the wake then
//!   reads straight into the buffer that becomes the JS `Uint8Array` and sends the completion.
//!   The pending task is what keeps the loop alive while a socket or server is open (an idle
//!   server's pending `accept` is its keep-alive handle); the registration alone does not.
//! - **connect** is a nonblocking `connect(2)` plus a writable registration on unix; the
//!   `getaddrinfo` of a host name runs on the shared pool. Windows connects, and named-pipe
//!   operations, still block a dedicated thread ([`CompletionSender::run_blocking`]) because their
//!   handles cannot be waited on.
//!
//! Live handles live in [`NetRegistry`] / [`DgramRegistry`] in `OpState`, keyed by the id handed
//! to JS. A socket is shared as an `Arc` between the registry entry and its `Io`; JS callbacks are
//! never moved into a closure that leaves the loop thread. A descriptor is never put in
//! `O_NONBLOCK` on unix (it may be shared with another process): every call passes `MSG_DONTWAIT`.
//!
//! ## What's real vs. std-limited
//! Plain TCP and UDP are fully real (loopback + cross-runtime verified against the Node oracle):
//! connect/listen/accept/read/write/half-close/close, error codes (ECONNREFUSED, EADDRINUSE, …),
//! `setNoDelay`, addresses; UDP bind/send/recv-with-source, broadcast, TTL, and IPv4 multicast
//! membership/loopback/TTL. `setKeepAlive` uses `setsockopt(SO_KEEPALIVE)` on unix (std exposes no
//! keepalive). Genuinely std-impossible bits throw honestly from JS: `setMulticastInterface`
//! (needs `IP_MULTICAST_IF`) and IPv6 multicast TTL (no std setter). `backlog` and dgram
//! `reuseAddr` are accepted but inert (std binds without exposing either).

use lumen_bind::NativeError;
use lumen_host::OpError;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs,
    UdpSocket,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};

use lumen_host::{CallbackQueue, CompletionSender, Ctx, TaskId, TaskRegistry, Value};
use lumen_os::reactor::{Reactor, Source};
use ready::{Dir, Io, Step, Trigger};

mod ready;
#[cfg(unix)]
mod fdpass;
/// Descriptor passing is unix-only (Windows IPC rides the child's stdio instead).
#[cfg(not(unix))]
mod fdpass {
    use super::*;

    fn unsupported() -> OpError {
        NativeError::runtime("socket descriptor passing is not supported on this platform").into()
    }
    pub(super) fn adopt_fd(_c: &mut Ctx, _fd: f64) -> Result<Value, OpError> {
        Err(unsupported())
    }
    pub(super) fn socket_fd(_c: &mut Ctx, _sid: u64) -> f64 {
        -1.0
    }
    pub(super) fn server_fd(_c: &mut Ctx, _sid: u64) -> f64 {
        -1.0
    }
    pub(super) fn udp_fd(_c: &mut Ctx, _sid: u64) -> f64 {
        -1.0
    }
    pub(super) fn release(_c: &mut Ctx, _sid: u64) {}
    pub(super) fn read_msg(
        _c: &mut Ctx,
        _sid: u64,
        _resolve: Value,
        _reject: Value,
    ) -> Result<(), OpError> {
        Err(unsupported())
    }
    pub(super) fn try_send_msg(_c: &mut Ctx, _sid: u64, _data: &[u8], _fd: Option<f64>) -> f64 {
        0.0
    }
    pub(super) fn write_msg(
        _c: &mut Ctx,
        _sid: u64,
        _data: Vec<u8>,
        _fd: Option<f64>,
        _resolve: Value,
        _reject: Value,
    ) -> Result<(), OpError> {
        Err(unsupported())
    }
    pub(super) fn guess_handle(_fd: Option<f64>) -> &'static str {
        "UNKNOWN"
    }
    pub(super) fn dup_fd(_fd: Option<f64>) -> f64 {
        -1.0
    }
    pub(super) fn close_fd(_fd: Option<f64>) {}
}

// ---- registries ---------------------------------------------------------------------------------

struct SockEntry {
    stream: Arc<NetStream>,
    /// The socket was `unref`'d: its pending read must not by itself keep the loop alive.
    unref: bool,
    /// The in-flight read task, so `ref`/`unref` can retroactively toggle it.
    pending: Option<TaskId>,
    /// The socket's readiness queues, created by its first operation.
    io: Option<Arc<Io>>,
}

struct ServerEntry {
    listener: Arc<NetListener>,
    /// Ends a pipe listener's blocked accept thread (socket listeners are woken by the reactor).
    #[cfg_attr(unix, allow(dead_code))]
    closed: Arc<AtomicBool>,
    io: Option<Arc<Io>>,
    local_addr: ServerAddress,
    unref: bool,
    pending: Option<TaskId>,
    /// This process bound the socket path, so closing the listener unlinks it (as libuv does).
    owns_path: bool,
}

enum NetStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
    /// A Windows named pipe: what a `net` path means on Windows.
    #[cfg(windows)]
    Pipe(crate::win_pipe::PipeStream),
}

impl NetStream {
    fn local_addr(&self) -> std::io::Result<Option<SocketAddr>> {
        match self {
            Self::Tcp(stream) => stream.local_addr().map(Some),
            #[cfg(unix)]
            Self::Unix(_) => Ok(None),
            #[cfg(windows)]
            Self::Pipe(_) => Ok(None),
        }
    }

    fn peer_addr(&self) -> std::io::Result<Option<SocketAddr>> {
        match self {
            Self::Tcp(stream) => stream.peer_addr().map(Some),
            #[cfg(unix)]
            Self::Unix(_) => Ok(None),
            #[cfg(windows)]
            Self::Pipe(_) => Ok(None),
        }
    }

    fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.shutdown(how),
            #[cfg(unix)]
            Self::Unix(stream) => stream.shutdown(how),
            // Named pipes have no half-close: ending the writable side closes the pipe after a
            // short linger (see PipeStream::shutdown_write).
            #[cfg(windows)]
            Self::Pipe(pipe) => {
                if how == Shutdown::Write {
                    pipe.shutdown_write();
                } else {
                    pipe.close();
                }
                Ok(())
            }
        }
    }

    fn set_nodelay(&self, on: bool) -> std::io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.set_nodelay(on),
            #[cfg(unix)]
            Self::Unix(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "TCP_NODELAY is not available for Unix sockets",
            )),
            #[cfg(windows)]
            Self::Pipe(_) => Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "TCP_NODELAY is not available for pipes",
            )),
        }
    }

    #[cfg(unix)]
    fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        match self {
            Self::Tcp(stream) => stream.as_raw_fd(),
            Self::Unix(stream) => stream.as_raw_fd(),
        }
    }

    /// What the reactor waits on; `None` for a named pipe, which has no readiness registration.
    fn source(&self) -> Option<Source> {
        match self {
            #[cfg(unix)]
            Self::Tcp(_) | Self::Unix(_) => Some(Source::Fd(self.raw_fd())),
            #[cfg(windows)]
            Self::Tcp(stream) => Some(socket_source(stream)),
            #[cfg(windows)]
            Self::Pipe(_) => None,
            #[cfg(not(any(unix, windows)))]
            Self::Tcp(_) => None,
        }
    }

    /// One `recv` into `buf`'s spare capacity without blocking (`WouldBlock` when nothing is
    /// there); `buf.len()` is the byte count afterwards. `Ok(0)` is end of stream.
    fn recv_nb(&self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        #[cfg(unix)]
        {
            let spare = buf.spare_capacity_mut();
            // SAFETY: the kernel writes at most `spare.len()` bytes into the uninitialised spare
            // capacity of `buf`, and `set_len` below covers exactly the `n` it wrote.
            let n = unsafe {
                libc::recv(
                    self.raw_fd(),
                    spare.as_mut_ptr().cast(),
                    spare.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `recv` initialised the first `n` bytes of the spare capacity.
            unsafe { buf.set_len(buf.len() + n as usize) };
            Ok(n as usize)
        }
        #[cfg(not(unix))]
        {
            let start = buf.len();
            buf.resize(buf.capacity(), 0);
            let result = (&*self).read(&mut buf[start..]);
            buf.truncate(start + result.as_ref().copied().unwrap_or(0));
            result
        }
    }

    /// One `send` of a prefix of `data` without blocking; `WouldBlock` when the socket takes
    /// nothing right now.
    fn send_nb(&self, data: &[u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        {
            let data = sendable(self.raw_fd(), data);
            if data.is_empty() {
                return Err(std::io::ErrorKind::WouldBlock.into());
            }
            lumen_os::net::send(self.raw_fd(), data, libc::MSG_DONTWAIT).map_err(os_error)
        }
        #[cfg(not(unix))]
        {
            (&*self).write(data)
        }
    }

    fn tcp(&self) -> Option<&TcpStream> {
        match self {
            Self::Tcp(stream) => Some(stream),
            #[cfg(unix)]
            Self::Unix(_) => None,
            #[cfg(windows)]
            Self::Pipe(_) => None,
        }
    }
}

impl Read for &NetStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            NetStream::Tcp(stream) => (&*stream).read(buf),
            #[cfg(unix)]
            NetStream::Unix(stream) => (&*stream).read(buf),
            #[cfg(windows)]
            NetStream::Pipe(pipe) => pipe.read(buf),
        }
    }
}

impl Write for &NetStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            NetStream::Tcp(stream) => (&*stream).write(buf),
            #[cfg(unix)]
            NetStream::Unix(stream) => (&*stream).write(buf),
            #[cfg(windows)]
            NetStream::Pipe(pipe) => pipe.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            NetStream::Tcp(stream) => (&*stream).flush(),
            #[cfg(unix)]
            NetStream::Unix(stream) => (&*stream).flush(),
            #[cfg(windows)]
            NetStream::Pipe(_) => Ok(()),
        }
    }
}

enum NetListener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixListener),
    #[cfg(windows)]
    Pipe(crate::win_pipe::PipeListener),
}

impl NetListener {
    #[cfg(unix)]
    fn raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        match self {
            Self::Tcp(listener) => listener.as_raw_fd(),
            Self::Unix(listener) => listener.as_raw_fd(),
        }
    }

    /// Socket listeners are non-blocking: a pending accept is a reactor registration, and a
    /// connection that another process sharing the socket won is just a `WouldBlock`.
    fn set_nonblocking(&self) {
        let _ = match self {
            Self::Tcp(listener) => listener.set_nonblocking(true),
            #[cfg(unix)]
            Self::Unix(listener) => listener.set_nonblocking(true),
            #[cfg(windows)]
            Self::Pipe(_) => Ok(()),
        };
    }

    /// What the reactor waits on; `None` for a named-pipe listener.
    fn source(&self) -> Option<Source> {
        match self {
            #[cfg(unix)]
            Self::Tcp(_) | Self::Unix(_) => Some(Source::Fd(self.raw_fd())),
            #[cfg(windows)]
            Self::Tcp(listener) => Some(socket_source(listener)),
            #[cfg(windows)]
            Self::Pipe(_) => None,
            #[cfg(not(any(unix, windows)))]
            Self::Tcp(_) => None,
        }
    }

    fn accept(&self) -> std::io::Result<NetStream> {
        match self {
            Self::Tcp(listener) => listener.accept().map(|(stream, _)| NetStream::Tcp(stream)),
            #[cfg(unix)]
            Self::Unix(listener) => listener.accept().map(|(stream, _)| NetStream::Unix(stream)),
            #[cfg(windows)]
            Self::Pipe(listener) => listener.accept().map(NetStream::Pipe),
        }
    }
}

#[derive(Clone)]
enum ServerAddress {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(String),
    #[cfg(windows)]
    Pipe(String),
}

#[derive(Default)]
pub struct NetRegistry {
    next_socket: u64,
    sockets: HashMap<u64, SockEntry>,
    next_server: u64,
    servers: HashMap<u64, ServerEntry>,
    /// Nonblocking connects in flight, by their task; dropping one deregisters its socket.
    #[cfg(unix)]
    connecting: HashMap<TaskId, Arc<ready::ConnectAttempt>>,
}

struct UdpEntry {
    socket: Arc<UdpSocket>,
    kind6: bool,
    unref: bool,
    pending: Option<TaskId>,
    io: Option<Arc<Io>>,
}

#[derive(Default)]
pub struct DgramRegistry {
    next: u64,
    sockets: HashMap<u64, UdpEntry>,
}

// ---- shared error/value helpers -----------------------------------------------------------------

/// A `Send` socket error carried back to the loop thread, with the errno `code` Node users switch
/// on and the syscall context Node attaches.
struct NetErr {
    code: &'static str,
    syscall: &'static str,
    message: String,
    address: Option<String>,
    port: Option<u16>,
    errno: Option<i32>,
}

/// libuv's name for a socket error; `UNKNOWN` for one that carries no OS error of its own.
fn io_code(e: &std::io::Error) -> &'static str {
    match lumen_os::errno::uv_code(e) {
        "EIO" if e.raw_os_error().is_none() => "UNKNOWN",
        code => code,
    }
}

fn net_err(syscall: &'static str, e: &std::io::Error, addr: Option<(String, u16)>) -> NetErr {
    let (address, port) = match addr {
        Some((a, p)) => (Some(a), Some(p)),
        None => (None, None),
    };
    NetErr {
        code: io_code(e),
        syscall,
        message: e.to_string(),
        address,
        port,
        errno: e.raw_os_error(),
    }
}

/// The error a rejected socket op throws: message shaped like Node's
/// (`connect ECONNREFUSED 127.0.0.1:80`) with `code`/`syscall`/`address`/`port`.
fn net_error(e: &NetErr) -> NativeError {
    let mut msg = format!("{} {}", e.syscall, e.code);
    if let Some(a) = &e.address {
        msg.push(' ');
        msg.push_str(a);
        if let Some(p) = e.port {
            msg.push(':');
            msg.push_str(&p.to_string());
        }
    } else if e.code == "UNKNOWN" {
        msg = format!("{}: {}", e.syscall, e.message);
    }
    let mut err = NativeError::runtime(msg);
    if let Some(n) = e.errno.filter(|_| cfg!(unix)) {
        err = err.with_prop("errno", -n);
    }
    err = err
        .with_prop("code", e.code)
        .with_prop("syscall", e.syscall);
    if let Some(a) = &e.address {
        err = err.with_prop("address", a.clone());
    }
    if let Some(p) = e.port {
        err = err.with_prop("port", p as i64);
    }
    err
}

/// [`net_error`] as a JS error value, for callbacks that settle a promise with it.
fn net_error_value(ctx: &mut Ctx, e: &NetErr) -> Value {
    OpError::from(net_error(e)).to_value(ctx)
}

fn family_of(addr: &SocketAddr) -> &'static str {
    if addr.is_ipv6() {
        "IPv6"
    } else {
        "IPv4"
    }
}

/// `{ address, family, port }` — the shape of Node's `socket.address()` / `server.address()`.
fn addr_object(ctx: &mut Ctx, addr: &SocketAddr) -> Value {
    let o = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&o, "address", Value::from_string(addr.ip().to_string()));
    let _ = ctx.set_member(&o, "family", Value::str(family_of(addr)));
    let _ = ctx.set_member(&o, "port", Value::Num(addr.port() as f64));
    o
}

#[cfg(windows)]
fn socket_source<T: std::os::windows::io::AsRawSocket>(socket: &T) -> Source {
    Source::Socket(socket.as_raw_socket() as usize)
}

/// What the reactor waits on for a datagram socket.
#[cfg(unix)]
fn udp_source(socket: &UdpSocket) -> Option<Source> {
    use std::os::fd::AsRawFd;
    Some(Source::Fd(socket.as_raw_fd()))
}

#[cfg(windows)]
fn udp_source(socket: &UdpSocket) -> Option<Source> {
    Some(socket_source(socket))
}

#[cfg(not(any(unix, windows)))]
fn udp_source(_socket: &UdpSocket) -> Option<Source> {
    None
}

fn no_readiness_source() -> OpError {
    NativeError::runtime("this socket has no readiness source on this platform").into()
}

/// The loop's readiness reactor, or the error a socket op throws where there is none.
fn loop_reactor(ctx: &mut Ctx) -> Result<Arc<dyn Reactor>, OpError> {
    ctx.op_state()
        .get::<lumen_host::loop_reactor::LoopReactor>()
        .and_then(|reactor| reactor.reactor())
        .ok_or_else(|| NativeError::runtime("no I/O readiness reactor on this platform").into())
}

/// A socket's `Io`, created on first use around `owner`, which keeps its descriptor open.
fn io_for(
    ctx: &mut Ctx,
    slot: Option<Arc<Io>>,
    source: Source,
    owner: Arc<dyn std::any::Any + Send + Sync>,
) -> Result<(Arc<Io>, bool), OpError> {
    if let Some(io) = slot {
        return Ok((io, false));
    }
    let reactor = loop_reactor(ctx)?;
    Ok((Io::new(reactor, source, completions(ctx), owner), true))
}

/// The error that fails writes still queued on a socket that is closing.
fn closed_write_error() -> std::io::Error {
    #[cfg(unix)]
    return std::io::Error::from_raw_os_error(libc::EPIPE);
    #[cfg(not(unix))]
    std::io::Error::from_raw_os_error(10058)
}

/// A `Uint8Array` over `bytes`, adopted without a copy.
fn uint8array_owned(ctx: &mut Ctx, bytes: Vec<u8>) -> Result<Value, Value> {
    Ok(<lumen::embed::JsHost as lumen_bind::Host>::from_bytes(ctx, bytes))
}

fn completions(ctx: &mut Ctx) -> CompletionSender {
    ctx.op_state()
        .get::<CompletionSender>()
        .expect("runtime installs the completion sender")
        .clone()
}

fn take_resolve_reject(
    res: Option<&Value>,
    rej: Option<&Value>,
) -> Result<(Value, Value), OpError> {
    match (res, rej) {
        (Some(r), Some(j)) if r.is_callable() && j.is_callable() => Ok((r.clone(), j.clone())),
        _ => Err(NativeError::type_error("net op expects (resolve, reject)").into()),
    }
}

fn enqueue(ctx: &mut Ctx, cb: Value, args: Vec<Value>) {
    CallbackQueue::enqueue(ctx.op_state(), cb, args);
}

// ---- TCP: register a connected stream (accept/connect share this) ------------------------------

/// Insert a freshly connected `TcpStream` into the registry and produce the JS descriptor
/// `[socketId, localAddress, localPort, remoteAddress, remotePort, family]`.
fn register_stream(ctx: &mut Ctx, stream: NetStream) -> Result<Vec<Value>, OpError> {
    let local = stream
        .local_addr()
        .map_err(|e| NativeError::runtime(format!("local_addr: {e}")))?;
    let peer = stream
        .peer_addr()
        .map_err(|e| NativeError::runtime(format!("peer_addr: {e}")))?;
    // Windows has no per-call non-blocking flag: the socket itself must not block.
    #[cfg(windows)]
    if let NetStream::Tcp(tcp) = &stream {
        let _ = tcp.set_nonblocking(true);
    }
    let reg = ctx
        .host_mut::<NetRegistry>()
        .expect("net registry installed");
    let id = reg.next_socket;
    reg.next_socket += 1;
    reg.sockets.insert(
        id,
        SockEntry {
            stream: Arc::new(stream),
            unref: false,
            pending: None,
            io: None,
        },
    );
    match (local, peer) {
        (Some(local), Some(peer)) => Ok(vec![
            Value::Num(id as f64),
            Value::from_string(local.ip().to_string()),
            Value::Num(local.port() as f64),
            Value::from_string(peer.ip().to_string()),
            Value::Num(peer.port() as f64),
            Value::str(family_of(&peer)),
        ]),
        _ => Ok(vec![
            Value::Num(id as f64),
            Value::Undefined,
            Value::Undefined,
            Value::Undefined,
            Value::Undefined,
            Value::Undefined,
        ]),
    }
}

fn path_err(syscall: &'static str, path: String, error: std::io::Error) -> NetErr {
    NetErr {
        code: io_code(&error),
        syscall,
        message: error.to_string(),
        address: Some(path),
        port: None,
        errno: error.raw_os_error(),
    }
}

// ---- TCP client ops -----------------------------------------------------------------------------

/// `TcpStream::connect`, except that on Windows a loopback connect fails fast when nothing
/// listens: like libuv, the socket is told not to retransmit its SYN (`SIO_TCP_INITIAL_RTO`), so
/// ECONNREFUSED arrives at once instead of after Windows' ~2 s of retries. Unix connects without
/// blocking (see [`drive_connect`]).
#[cfg(not(unix))]
fn connect_tcp(addr: SocketAddr) -> std::io::Result<TcpStream> {
    #[cfg(windows)]
    {
        if addr.ip().is_loopback() {
            return win_loopback::connect(addr);
        }
    }
    TcpStream::connect(addr)
}

/// The local address a connect binds to first (`None` for an unbound connect), in `addr`'s family.
#[cfg(unix)]
fn connect_local(
    addr: SocketAddr,
    local: Option<(Option<IpAddr>, u16)>,
) -> std::io::Result<Option<SocketAddr>> {
    let Some((ip, port)) = local else {
        return Ok(None);
    };
    let ip = ip.unwrap_or(if addr.is_ipv6() {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    } else {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    });
    if ip.is_ipv6() != addr.is_ipv6() {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(Some(SocketAddr::new(ip, port)))
}

/// The `io::Error` of a `lumen_os` error, keeping its errno.
#[cfg(unix)]
pub(crate) fn os_error(e: lumen_os::FsError) -> std::io::Error {
    std::io::Error::from_raw_os_error(e.errno())
}

#[cfg(windows)]
mod win_loopback {
    use std::ffi::c_void;
    use std::net::{SocketAddr, TcpStream};
    use std::os::windows::io::FromRawSocket;

    type Socket = usize;
    const INVALID_SOCKET: Socket = !0;
    const AF_INET: i32 = 2;
    const AF_INET6: i32 = 23;
    const SOCK_STREAM: i32 = 1;
    const IPPROTO_TCP: i32 = 6;
    const WSA_FLAG_OVERLAPPED: u32 = 0x01;
    const WSA_FLAG_NO_HANDLE_INHERIT: u32 = 0x80;
    // _WSAIOW(IOC_VENDOR, 17)
    const SIO_TCP_INITIAL_RTO: u32 = 0x9800_0011;

    #[repr(C)]
    struct InitialRto {
        rtt: u16,
        max_syn_retransmissions: u8,
    }

    #[repr(C)]
    struct SockaddrIn {
        family: u16,
        port: u16,
        addr: [u8; 4],
        zero: [u8; 8],
    }

    #[repr(C)]
    struct SockaddrIn6 {
        family: u16,
        port: u16,
        flowinfo: u32,
        addr: [u8; 16],
        scope_id: u32,
    }

    #[repr(C)]
    struct WsaData {
        _opaque: [u8; 512],
    }

    #[link(name = "ws2_32")]
    extern "system" {
        fn WSAStartup(version: u16, data: *mut WsaData) -> i32;
        fn WSASocketW(
            af: i32,
            ty: i32,
            protocol: i32,
            info: *mut c_void,
            group: u32,
            flags: u32,
        ) -> Socket;
        fn WSAIoctl(
            s: Socket,
            code: u32,
            inbuf: *const c_void,
            inlen: u32,
            outbuf: *mut c_void,
            outlen: u32,
            returned: *mut u32,
            overlapped: *mut c_void,
            completion: *mut c_void,
        ) -> i32;
        #[link_name = "connect"]
        fn wsa_connect(s: Socket, name: *const c_void, namelen: i32) -> i32;
        fn closesocket(s: Socket) -> i32;
        fn WSAGetLastError() -> i32;
    }

    pub(super) fn connect(addr: SocketAddr) -> std::io::Result<TcpStream> {
        // SAFETY: plain Winsock calls on a socket this function owns until it is handed to
        // `TcpStream` (or closed on failure); every buffer outlives the call it is passed to.
        unsafe {
            let mut data = WsaData { _opaque: [0; 512] };
            WSAStartup(0x0202, &mut data);
            let af = if addr.is_ipv4() { AF_INET } else { AF_INET6 };
            let s = WSASocketW(
                af,
                SOCK_STREAM,
                IPPROTO_TCP,
                std::ptr::null_mut(),
                0,
                WSA_FLAG_OVERLAPPED | WSA_FLAG_NO_HANDLE_INHERIT,
            );
            if s == INVALID_SOCKET {
                return Err(std::io::Error::from_raw_os_error(WSAGetLastError()));
            }
            let rto = InitialRto {
                rtt: u16::MAX,                 // TCP_INITIAL_RTO_UNSPECIFIED_RTT
                max_syn_retransmissions: 0xfe, // TCP_INITIAL_RTO_NO_SYN_RETRANSMISSIONS
            };
            let mut returned = 0u32;
            // Best effort, as in libuv: an older Windows without the ioctl still connects.
            WSAIoctl(
                s,
                SIO_TCP_INITIAL_RTO,
                (&rto as *const InitialRto).cast(),
                std::mem::size_of::<InitialRto>() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            let rc = match addr {
                SocketAddr::V4(v4) => {
                    let sa = SockaddrIn {
                        family: AF_INET as u16,
                        port: v4.port().to_be(),
                        addr: v4.ip().octets(),
                        zero: [0; 8],
                    };
                    wsa_connect(
                        s,
                        (&sa as *const SockaddrIn).cast(),
                        std::mem::size_of::<SockaddrIn>() as i32,
                    )
                }
                SocketAddr::V6(v6) => {
                    let sa = SockaddrIn6 {
                        family: AF_INET6 as u16,
                        port: v6.port().to_be(),
                        flowinfo: v6.flowinfo(),
                        addr: v6.ip().octets(),
                        scope_id: v6.scope_id(),
                    };
                    wsa_connect(
                        s,
                        (&sa as *const SockaddrIn6).cast(),
                        std::mem::size_of::<SockaddrIn6>() as i32,
                    )
                }
            };
            if rc != 0 {
                let err = std::io::Error::from_raw_os_error(WSAGetLastError());
                closesocket(s);
                return Err(err);
            }
            Ok(TcpStream::from_raw_socket(s as u64))
        }
    }
}

/// The addresses of `host:port`; a host name goes through `getaddrinfo`, which blocks.
fn resolve_host(host: &str, port: u16) -> Result<Vec<SocketAddr>, NetErr> {
    (host, port)
        .to_socket_addrs()
        .map(|addrs| addrs.collect())
        .map_err(|e| NetErr {
            code: "ENOTFOUND",
            syscall: "getaddrinfo",
            message: e.to_string(),
            address: Some(host.to_string()),
            port: Some(port),
            errno: None,
        })
}

/// What a connect task settles with: the socket or the error, and the task so the loop can
/// forget the attempt.
struct ConnectOutcome {
    #[cfg_attr(not(unix), allow(dead_code))]
    id: TaskId,
    result: Result<NetStream, NetErr>,
}

fn decode_connect(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    let outcome = *payload
        .downcast::<ConnectOutcome>()
        .expect("connect payload");
    #[cfg(unix)]
    if let Some(reg) = ctx.host_mut::<NetRegistry>() {
        reg.connecting.remove(&outcome.id);
    }
    match outcome.result {
        Ok(stream) => register_stream(ctx, stream).map_err(|e| e.to_value(ctx)),
        Err(e) => Err(net_error_value(ctx, &e)),
    }
}

fn decode_read(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<Vec<u8>, NetErr>>()
        .expect("read payload")
    {
        Ok(bytes) if bytes.is_empty() => Ok(vec![Value::Null]),
        Ok(bytes) => Ok(vec![uint8array_owned(ctx, bytes)?]),
        Err(e) => Err(net_error_value(ctx, &e)),
    }
}

/// The prefix of `data` that one send can take without blocking. BSD/Darwin sockets ignore
/// `MSG_DONTWAIT` once a send outgrows the free buffer space and block until it all fits, so the
/// slice is capped to what the buffer can take right now (empty: no room).
#[cfg(unix)]
fn sendable(fd: std::os::fd::RawFd, data: &[u8]) -> &[u8] {
    #[cfg(target_vendor = "apple")]
    {
        let query = |name: i32| -> Option<usize> {
            lumen_os::net::getsockopt_int(fd, libc::SOL_SOCKET, name)
                .ok()
                .and_then(|v| usize::try_from(v).ok())
        };
        let space = match (query(libc::SO_SNDBUF), query(libc::SO_NWRITE)) {
            (Some(buf), Some(queued)) => buf.saturating_sub(queued),
            _ => 0,
        };
        return &data[..data.len().min(space)];
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        let _ = fd;
        data
    }
}

/// libuv's `uv_try_write`: what the socket takes right now (0 when nothing, or on an error, which
/// the async write that follows reports).
fn try_write(stream: &NetStream, data: &[u8]) -> usize {
    // Like uv_try_write on Windows, a large write takes a partial first slice synchronously; the
    // rest is queued, so writeQueueSize shows progress (Socket#_onTimeout suppresses its timeout
    // on it).
    #[cfg(windows)]
    let data = &data[..data.len().min(64 * 1024)];
    if data.is_empty() {
        return 0;
    }
    stream.send_nb(data).unwrap_or(0)
}

/// A queued write: sends `data` in as many `send`s as the socket takes, finishing when all of it
/// is out. Several writes on one socket complete in the order they were issued.
fn write_step(
    stream: Arc<NetStream>,
    data: Vec<u8>,
) -> impl FnMut(Trigger) -> Step + Send + 'static {
    let mut sent = 0;
    move |trigger| {
        let fail = |e: &std::io::Error| Step::Done(Box::new(Err::<(), NetErr>(net_err("write", e, None))));
        if let Trigger::Failed(e) = &trigger {
            return fail(e);
        }
        while sent < data.len() {
            match stream.send_nb(&data[sent..]) {
                Ok(n) => sent += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Step::Again,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return fail(&e),
            }
        }
        Step::Done(Box::new(Ok::<(), NetErr>(())))
    }
}

/// How many bytes one read takes off a socket at most.
const READ_CHUNK: usize = 64 * 1024;

/// Fills a buffer for a read result: reads that returned little give the rest back.
fn trimmed(mut buf: Vec<u8>) -> Vec<u8> {
    if buf.len() < READ_CHUNK / 2 {
        buf.shrink_to_fit();
    }
    buf
}

/// A queued read: one chunk, straight into the `Vec` that becomes the JS `Uint8Array`.
fn read_step(stream: Arc<NetStream>) -> impl FnMut(Trigger) -> Step + Send + 'static {
    move |trigger| {
        let result: Result<Vec<u8>, NetErr> = match trigger {
            Trigger::Failed(e) => Err(net_err("read", &e, None)),
            Trigger::Ready => {
                let mut buf = Vec::with_capacity(READ_CHUNK);
                loop {
                    match stream.recv_nb(&mut buf) {
                        Ok(_) => break Ok(trimmed(buf)),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Step::Again,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => break Err(net_err("read", &e, None)),
                    }
                }
            }
        };
        Step::Done(Box::new(result))
    }
}

/// A connect on unix: the addresses still to try, and what the loop needs to carry on from a wake.
#[cfg(unix)]
struct ConnectJob {
    id: TaskId,
    addrs: std::collections::VecDeque<SocketAddr>,
    last: Option<std::io::Error>,
    host: String,
    port: u16,
    local: Option<(Option<IpAddr>, u16)>,
    attempt: Arc<ready::ConnectAttempt>,
    reactor: Arc<dyn Reactor>,
    completions: CompletionSender,
}

#[cfg(unix)]
impl ConnectJob {
    fn finish(&self, result: Result<NetStream, NetErr>) {
        self.completions
            .send(self.id, Box::new(ConnectOutcome { id: self.id, result }));
    }
}

/// Tries the addresses in turn: starts a nonblocking connect and, while it is in progress, waits
/// for the socket to become writable on the reactor. Runs on the loop thread (a wake) or, for the
/// first address, on whichever thread resolved the host name.
#[cfg(unix)]
fn drive_connect(mut job: ConnectJob) {
    use std::os::fd::AsRawFd;
    loop {
        if job.attempt.is_cancelled() {
            return;
        }
        let Some(addr) = job.addrs.pop_front() else {
            let error = job
                .last
                .take()
                .unwrap_or_else(|| std::io::Error::other("no address"));
            let result = Err(net_err("connect", &error, Some((job.host.clone(), job.port))));
            job.finish(result);
            return;
        };
        let started = connect_local(addr, job.local)
            .and_then(|local| lumen_os::net::connect_start(addr, local));
        match started {
            Err(e) => job.last = Some(e),
            Ok((stream, false)) => {
                let _ = stream.set_nonblocking(false);
                job.finish(Ok(NetStream::Tcp(stream)));
                return;
            }
            Ok((stream, true)) => {
                let stream = Arc::new(stream);
                let io = Io::new(
                    job.reactor.clone(),
                    Source::Fd(stream.as_raw_fd()),
                    job.completions.clone(),
                    stream.clone(),
                );
                if !job.attempt.set_current(io.clone()) {
                    return;
                }
                let id = job.id;
                io.submit(Dir::Write, id, false, connect_step(stream, job));
                return;
            }
        }
    }
}

/// Finishes a connect when its socket becomes writable: success, or the failure, which moves on to
/// the next address.
#[cfg(unix)]
fn connect_step(
    stream: Arc<TcpStream>,
    job: ConnectJob,
) -> impl FnMut(Trigger) -> Step + Send + 'static {
    let mut job = Some(job);
    move |trigger| {
        let failure = match trigger {
            Trigger::Failed(e) => Some(e),
            Trigger::Ready => match stream.take_error() {
                Ok(Some(e)) | Err(e) => Some(e),
                Ok(None) => match stream.peer_addr() {
                    Ok(_) => None,
                    Err(e) if e.kind() == std::io::ErrorKind::NotConnected => return Step::Again,
                    Err(e) => Some(e),
                },
            },
        };
        let mut job = job.take().expect("a connect step finishes once");
        let connected = match failure {
            None => stream.try_clone().and_then(|dup| {
                dup.set_nonblocking(false)?;
                Ok(dup)
            }),
            Some(e) => Err(e),
        };
        match connected {
            Ok(dup) => {
                job.finish(Ok(NetStream::Tcp(dup)));
            }
            Err(e) => {
                job.last = Some(e);
                drive_connect(job);
            }
        }
        Step::Handled
    }
}

fn decode_write(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<(), NetErr>>()
        .expect("write payload")
    {
        Ok(()) => Ok(vec![]),
        Err(e) => Err(net_error_value(ctx, &e)),
    }
}

// ---- TCP server ops -----------------------------------------------------------------------------

/// Bind and listen the way libuv does: `SO_REUSEADDR` on, `IPV6_V6ONLY` per the flag for an
/// IPv6 address (so `::` is dual-stack unless asked otherwise), the real backlog.
#[cfg(unix)]
fn listen_tcp(host: &str, port: u16, backlog: i32, flags: u32) -> std::io::Result<TcpListener> {
    use lumen_os::net as os;
    use std::os::fd::FromRawFd;
    const IPV6ONLY: u32 = 1;
    let addr = match host.parse::<IpAddr>() {
        Ok(ip) => SocketAddr::new(ip, port),
        Err(_) => (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::from_raw_os_error(libc::EADDRNOTAVAIL))?,
    };
    let domain = if addr.is_ipv6() {
        libc::AF_INET6
    } else {
        libc::AF_INET
    };
    let fd = os::socket(domain, libc::SOCK_STREAM, 0).map_err(os_error)?;
    // SAFETY: `fd` is a fresh descriptor owned by nothing else.
    let listener = unsafe { TcpListener::from_raw_fd(fd) };
    os::setsockopt_int(fd, libc::SOL_SOCKET, libc::SO_REUSEADDR, 1).map_err(os_error)?;
    if addr.is_ipv6() {
        os::setsockopt_int(
            fd,
            libc::IPPROTO_IPV6,
            libc::IPV6_V6ONLY,
            (flags & IPV6ONLY != 0) as i32,
        )
        .map_err(os_error)?;
    }
    os::bind(fd, &addr.into()).map_err(os_error)?;
    os::listen(fd, backlog).map_err(os_error)?;
    Ok(listener)
}

#[cfg(not(unix))]
fn listen_tcp(host: &str, port: u16, _backlog: i32, _flags: u32) -> std::io::Result<TcpListener> {
    TcpListener::bind((host, port))
}

enum AcceptResult {
    Conn(NetStream),
    Closed,
}

fn decode_accept(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<AcceptResult>().expect("accept payload") {
        AcceptResult::Conn(stream) => register_stream(ctx, stream).map_err(|e| e.to_value(ctx)),
        AcceptResult::Closed => Ok(vec![Value::Null]),
    }
}

/// Closes a listener: its pending accept settles `null`, its reactor registration goes, and a
/// unix socket path this process bound is unlinked. A named-pipe listener cancels its own blocked
/// accept thread.
fn close_server_entry(entry: ServerEntry, completions: Option<&CompletionSender>) {
    let ServerEntry {
        closed,
        local_addr,
        listener,
        owns_path,
        io,
        pending,
        ..
    } = entry;
    closed.store(true, Ordering::SeqCst);
    #[cfg(unix)]
    if let (true, ServerAddress::Unix(path)) = (owns_path, &local_addr) {
        let _ = std::fs::remove_file(path);
    }
    #[cfg(not(unix))]
    let _ = (owns_path, local_addr);
    #[cfg(windows)]
    if let NetListener::Pipe(pipe) = &*listener {
        pipe.close();
    }
    if let (Some(id), Some(_), Some(completions)) = (pending, &io, completions) {
        completions.send(id, Box::new(AcceptResult::Closed));
    }
    drop(io);
    drop(listener);
}

/// Releases every socket and listener: a terminated or dropped realm has nobody left to settle
/// their operations, and a descriptor must not stay registered with the reactor.
pub(crate) fn close_all(ctx: &mut Ctx) {
    let sender = ctx.op_state().get::<CompletionSender>().cloned();
    if let Some(reg) = ctx.host_mut::<NetRegistry>() {
        for (_, entry) in reg.sockets.drain() {
            let _ = entry.stream.shutdown(Shutdown::Both);
        }
        #[cfg(unix)]
        for (_, attempt) in reg.connecting.drain() {
            attempt.cancel();
        }
        let servers: Vec<ServerEntry> = reg.servers.drain().map(|(_, e)| e).collect();
        for entry in servers {
            close_server_entry(entry, sender.as_ref());
        }
    }
    if let Some(reg) = ctx.host_mut::<DgramRegistry>() {
        reg.sockets.clear();
    }
}

// ---- UDP ops ------------------------------------------------------------------------------------

/// An IP literal for a `udp4`/`udp6` handle; a literal of the other family is `EINVAL`, as in
/// libuv's `uv_ip4_addr`/`uv_ip6_addr`.
fn parse_udp_addr(host: &str, port: u16, kind6: bool) -> std::io::Result<SocketAddr> {
    let invalid = || std::io::Error::from_raw_os_error(invalid_argument_errno());
    let (literal, scope) = match host.split_once('%') {
        Some((ip, scope)) => (ip, Some(scope)),
        None => (host, None),
    };
    let ip: IpAddr = literal.parse().map_err(|_| invalid())?;
    if ip.is_ipv6() != kind6 {
        return Err(invalid());
    }
    Ok(match ip {
        IpAddr::V4(ip) => SocketAddr::from((ip, port)),
        IpAddr::V6(ip) => {
            let scope_id = scope.and_then(|s| s.parse::<u32>().ok()).unwrap_or(0);
            SocketAddr::V6(std::net::SocketAddrV6::new(ip, port, 0, scope_id))
        }
    })
}

#[cfg(unix)]
fn invalid_argument_errno() -> i32 {
    libc::EINVAL
}

#[cfg(not(unix))]
fn invalid_argument_errno() -> i32 {
    10022
}

/// Bind a UDP socket with the libuv bind flags std cannot express (`UV_UDP_IPV6ONLY` = 1,
/// `UV_UDP_REUSEADDR` = 4).
#[cfg(unix)]
fn bind_udp(addr: SocketAddr, flags: u32) -> std::io::Result<UdpSocket> {
    use lumen_os::net as os;
    use std::os::fd::FromRawFd;
    const IPV6ONLY: u32 = 1;
    const REUSEADDR: u32 = 4;
    let domain = if addr.is_ipv6() {
        libc::AF_INET6
    } else {
        libc::AF_INET
    };
    let fd = os::socket(domain, libc::SOCK_DGRAM, 0).map_err(os_error)?;
    // SAFETY: `fd` is a fresh descriptor owned by nothing else.
    let socket = unsafe { UdpSocket::from_raw_fd(fd) };
    let set = |level: i32, name: i32| os::setsockopt_int(fd, level, name, 1).map_err(os_error);
    if addr.is_ipv6() && flags & IPV6ONLY != 0 {
        set(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY)?;
    }
    if flags & REUSEADDR != 0 {
        set(libc::SOL_SOCKET, libc::SO_REUSEADDR)?;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        set(libc::SOL_SOCKET, libc::SO_REUSEPORT)?;
    }
    os::bind(fd, &addr.into()).map_err(os_error)?;
    Ok(socket)
}

#[cfg(not(unix))]
fn bind_udp(addr: SocketAddr, _flags: u32) -> std::io::Result<UdpSocket> {
    let socket = UdpSocket::bind(addr)?;
    socket.set_nonblocking(true)?;
    Ok(socket)
}

#[cfg(unix)]
fn disconnect_udp(socket: &UdpSocket) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    lumen_os::net::disconnect(socket.as_raw_fd()).map_err(os_error)
}

#[cfg(not(unix))]
fn disconnect_udp(_socket: &UdpSocket) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

enum RecvResult {
    Msg(Vec<u8>, SocketAddr),
    Err(NetErr),
}

/// One datagram without blocking (`WouldBlock` when none is queued).
fn recv_datagram(socket: &UdpSocket) -> std::io::Result<(Vec<u8>, SocketAddr)> {
    let mut buf = vec![0u8; 65536];
    #[cfg(unix)]
    let (n, from) = {
        use std::os::fd::AsRawFd;
        use lumen_os::net::SockAddr;
        let (n, from) = lumen_os::net::recvfrom(socket.as_raw_fd(), &mut buf, libc::MSG_DONTWAIT)
            .map_err(os_error)?;
        let from = match from {
            Some(SockAddr::V4(a)) => Some(SocketAddr::V4(a)),
            Some(SockAddr::V6(a)) => Some(SocketAddr::V6(a)),
            _ => None,
        };
        (n, from.map_or_else(|| socket.peer_addr(), Ok)?)
    };
    #[cfg(not(unix))]
    let (n, from) = socket.recv_from(&mut buf)?;
    buf.truncate(n);
    Ok((trimmed(buf), from))
}

/// A queued receive: one datagram with its source.
fn recv_step(socket: Arc<UdpSocket>) -> impl FnMut(Trigger) -> Step + Send + 'static {
    move |trigger| {
        let result = match trigger {
            Trigger::Failed(e) => RecvResult::Err(net_err("recv", &e, None)),
            Trigger::Ready => loop {
                match recv_datagram(&socket) {
                    Ok((bytes, from)) => break RecvResult::Msg(bytes, from),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Step::Again,
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(e) => break RecvResult::Err(net_err("recv", &e, None)),
                }
            },
        };
        Step::Done(Box::new(result))
    }
}

fn decode_recv(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<RecvResult>().expect("recv payload") {
        RecvResult::Err(e) => Err(net_error_value(ctx, &e)),
        RecvResult::Msg(bytes, from) => {
            let size = bytes.len();
            let data = uint8array_owned(ctx, bytes)?;
            let o = Value::Obj(ctx.new_object());
            let _ = ctx.set_member(&o, "data", data);
            let _ = ctx.set_member(&o, "address", Value::from_string(from.ip().to_string()));
            let _ = ctx.set_member(&o, "port", Value::Num(from.port() as f64));
            let _ = ctx.set_member(&o, "family", Value::str(family_of(&from)));
            let _ = ctx.set_member(&o, "size", Value::Num(size as f64));
            Ok(vec![o])
        }
    }
}

/// Look a UDP socket up and run `f` on it, mapping an error to an errno-tagged JS throw.
fn with_udp(
    ctx: &mut Ctx,
    sid: u64,
    syscall: &'static str,
    f: impl FnOnce(&Arc<UdpSocket>, bool) -> std::io::Result<()>,
) -> Result<(), OpError> {
    let found = ctx
        .host_mut::<DgramRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| (e.socket.clone(), e.kind6));
    let Some((socket, kind6)) = found else {
        return Err(NativeError::runtime("dgram: unknown socket").into());
    };
    match f(&socket, kind6) {
        Ok(()) => Ok(()),
        Err(e) => {
            let err = net_err(syscall, &e, None);
            Err(net_error(&err).into())
        }
    }
}

#[cfg(unix)]
fn set_ipv6_multicast_hops(socket: &UdpSocket, hops: i32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    lumen_os::net::setsockopt_int(
        socket.as_raw_fd(),
        libc::IPPROTO_IPV6,
        libc::IPV6_MULTICAST_HOPS,
        hops,
    )
    .map_err(os_error)
}

#[cfg(not(unix))]
fn set_ipv6_multicast_hops(_socket: &UdpSocket, _hops: i32) -> std::io::Result<()> {
    Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
}

#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
fn set_multicast_interface(
    socket: &UdpSocket,
    kind6: bool,
    interface: &str,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    const IPPROTO_IP: i32 = 0;
    const IPPROTO_IPV6: i32 = 41;
    #[cfg(target_os = "linux")]
    const IP_MULTICAST_IF: i32 = 32;
    #[cfg(target_os = "macos")]
    const IP_MULTICAST_IF: i32 = 9;
    #[cfg(target_os = "linux")]
    const IPV6_MULTICAST_IF: i32 = 17;
    #[cfg(target_os = "macos")]
    const IPV6_MULTICAST_IF: i32 = 9;
    extern "C" {
        fn setsockopt(
            socket: i32,
            level: i32,
            name: i32,
            value: *const std::os::raw::c_void,
            length: u32,
        ) -> i32;
    }
    let (level, option, value) = if kind6 {
        let index = interface
            .trim_start_matches('%')
            .parse::<u32>()
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "IPv6 multicast interface must be a numeric index",
                )
            })?;
        (IPPROTO_IPV6, IPV6_MULTICAST_IF, index)
    } else {
        let address = interface.parse::<Ipv4Addr>().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "IPv4 multicast interface must be an IPv4 address",
            )
        })?;
        (
            IPPROTO_IP,
            IP_MULTICAST_IF,
            u32::from_ne_bytes(address.octets()),
        )
    };
    // SAFETY: the socket fd is live and value points to a valid 32-bit socket-option payload.
    let result = unsafe {
        setsockopt(
            socket.as_raw_fd(),
            level,
            option,
            &value as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn set_multicast_interface(
    socket: &UdpSocket,
    kind6: bool,
    interface: &str,
) -> std::io::Result<()> {
    const IPPROTO_IP: i32 = 0;
    const IPPROTO_IPV6: i32 = 41;
    const IP_MULTICAST_IF: i32 = 9;
    const IPV6_MULTICAST_IF: i32 = 9;
    if kind6 {
        let index = interface
            .trim_start_matches('%')
            .parse::<u32>()
            .map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "IPv6 multicast interface must be a numeric index",
                )
            })?;
        win_setsockopt(socket, IPPROTO_IPV6, IPV6_MULTICAST_IF, &index)
    } else {
        let address = interface.parse::<Ipv4Addr>().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "IPv4 multicast interface must be an IPv4 address",
            )
        })?;
        let value = u32::from_ne_bytes(address.octets());
        win_setsockopt(socket, IPPROTO_IP, IP_MULTICAST_IF, &value)
    }
}

#[cfg(not(any(windows, all(unix, any(target_os = "macos", target_os = "linux")))))]
fn set_multicast_interface(
    _socket: &UdpSocket,
    _kind6: bool,
    _interface: &str,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "multicast interface selection is unavailable",
    ))
}

fn parse_v4(s: &str) -> Result<Ipv4Addr, ()> {
    s.parse::<Ipv4Addr>().map_err(|_| ())
}
fn parse_v6(s: &str) -> Result<Ipv6Addr, ()> {
    s.parse::<Ipv6Addr>().map_err(|_| ())
}

fn udp_source_membership(
    ctx: &mut Ctx,
    sid: u64,
    source_text: String,
    group_text: String,
    interface_text: Option<String>,
    join: bool,
) -> Result<(), OpError> {
    let interface_text = interface_text.unwrap_or_default();
    let source = parse_v4(&source_text)
        .map_err(|_| NativeError::type_error(format!("Invalid source address: {source_text}")))?;
    let group = parse_v4(&group_text)
        .map_err(|_| NativeError::type_error(format!("Invalid multicast address: {group_text}")))?;
    if !group.is_multicast() {
        return Err(
            NativeError::type_error(format!("Invalid multicast address: {group_text}")).into(),
        );
    }
    let interface = if interface_text.is_empty() {
        Ipv4Addr::UNSPECIFIED
    } else {
        parse_v4(&interface_text).map_err(|_| {
            NativeError::type_error(format!("Invalid interface address: {interface_text}"))
        })?
    };
    let kind6 = ctx
        .host_mut::<DgramRegistry>()
        .and_then(|registry| registry.sockets.get(&sid))
        .map(|entry| entry.kind6)
        .unwrap_or(false);
    if kind6 {
        return Err(NativeError::runtime(
            "source-specific multicast is only supported for udp4 sockets",
        )
        .into());
    }
    let syscall = if join {
        "addSourceSpecificMembership"
    } else {
        "dropSourceSpecificMembership"
    };
    with_udp(ctx, sid, syscall, |socket, _| {
        set_source_membership(socket, source, group, interface, join)
    })
}

#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
fn set_source_membership(
    socket: &UdpSocket,
    source: Ipv4Addr,
    group: Ipv4Addr,
    interface: Ipv4Addr,
    join: bool,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // Linux orders `ip_mreq_source` as (multiaddr, interface, sourceaddr); macOS as
    // (multiaddr, sourceaddr, interface).
    #[cfg(target_os = "linux")]
    #[repr(C)]
    struct IpMreqSource {
        group: u32,
        interface: u32,
        source: u32,
    }
    #[cfg(target_os = "macos")]
    #[repr(C)]
    struct IpMreqSource {
        group: u32,
        source: u32,
        interface: u32,
    }
    const IPPROTO_IP: i32 = 0;
    #[cfg(target_os = "linux")]
    const IP_ADD_SOURCE_MEMBERSHIP: i32 = 39;
    #[cfg(target_os = "linux")]
    const IP_DROP_SOURCE_MEMBERSHIP: i32 = 40;
    #[cfg(target_os = "macos")]
    const IP_ADD_SOURCE_MEMBERSHIP: i32 = 70;
    #[cfg(target_os = "macos")]
    const IP_DROP_SOURCE_MEMBERSHIP: i32 = 71;
    extern "C" {
        fn setsockopt(
            socket: i32,
            level: i32,
            name: i32,
            value: *const std::os::raw::c_void,
            length: u32,
        ) -> i32;
    }
    let request = IpMreqSource {
        group: u32::from_ne_bytes(group.octets()),
        source: u32::from_ne_bytes(source.octets()),
        interface: u32::from_ne_bytes(interface.octets()),
    };
    let option = if join {
        IP_ADD_SOURCE_MEMBERSHIP
    } else {
        IP_DROP_SOURCE_MEMBERSHIP
    };
    let result = unsafe {
        setsockopt(
            socket.as_raw_fd(),
            IPPROTO_IP,
            option,
            &request as *const IpMreqSource as *const _,
            std::mem::size_of::<IpMreqSource>() as u32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Winsock's `ip_mreq_source` is (multiaddr, sourceaddr, interface).
#[cfg(windows)]
fn set_source_membership(
    socket: &UdpSocket,
    source: Ipv4Addr,
    group: Ipv4Addr,
    interface: Ipv4Addr,
    join: bool,
) -> std::io::Result<()> {
    #[repr(C)]
    struct IpMreqSource {
        group: u32,
        source: u32,
        interface: u32,
    }
    const IPPROTO_IP: i32 = 0;
    const IP_ADD_SOURCE_MEMBERSHIP: i32 = 15;
    const IP_DROP_SOURCE_MEMBERSHIP: i32 = 16;
    let request = IpMreqSource {
        group: u32::from_ne_bytes(group.octets()),
        source: u32::from_ne_bytes(source.octets()),
        interface: u32::from_ne_bytes(interface.octets()),
    };
    let option = if join {
        IP_ADD_SOURCE_MEMBERSHIP
    } else {
        IP_DROP_SOURCE_MEMBERSHIP
    };
    win_setsockopt(socket, IPPROTO_IP, option, &request)
}

/// `setsockopt` on a Winsock socket with a plain-data option value.
#[cfg(windows)]
fn win_setsockopt<T>(socket: &UdpSocket, level: i32, name: i32, value: &T) -> std::io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    #[link(name = "ws2_32")]
    extern "system" {
        fn setsockopt(socket: usize, level: i32, name: i32, value: *const u8, length: i32) -> i32;
        fn WSAGetLastError() -> i32;
    }
    // SAFETY: the socket is live for the borrow; `value` points to a `T` of the given length.
    let rc = unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            level,
            name,
            value as *const T as *const u8,
            std::mem::size_of::<T>() as i32,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        // SAFETY: reads this thread's last Winsock error.
        Err(std::io::Error::from_raw_os_error(unsafe {
            WSAGetLastError()
        }))
    }
}

#[cfg(not(any(windows, all(unix, any(target_os = "macos", target_os = "linux")))))]
fn set_source_membership(
    _socket: &UdpSocket,
    _source: Ipv4Addr,
    _group: Ipv4Addr,
    _interface: Ipv4Addr,
    _join: bool,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "source-specific multicast is unavailable on this platform",
    ))
}

fn udp_membership(
    ctx: &mut Ctx,
    sid: u64,
    mcast: String,
    iface: Option<String>,
    join: bool,
) -> Result<(), OpError> {
    let iface = iface.unwrap_or_default();
    let kind6 = ctx
        .host_mut::<DgramRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| e.kind6)
        .unwrap_or(false);
    let syscall = if join {
        "addMembership"
    } else {
        "dropMembership"
    };

    if kind6 {
        let group = parse_v6(&mcast)
            .map_err(|_| NativeError::type_error(format!("Invalid multicast address: {mcast}")))?;
        let idx = iface.parse::<u32>().unwrap_or(0);
        return with_udp(ctx, sid, syscall, |s, _| {
            if join {
                s.join_multicast_v6(&group, idx)
            } else {
                s.leave_multicast_v6(&group, idx)
            }
        });
    }
    let group = parse_v4(&mcast)
        .map_err(|_| NativeError::type_error(format!("Invalid multicast address: {mcast}")))?;
    let iface_addr = if iface.is_empty() {
        Ipv4Addr::UNSPECIFIED
    } else {
        parse_v4(&iface)
            .map_err(|_| NativeError::type_error(format!("Invalid interface address: {iface}")))?
    };
    with_udp(ctx, sid, syscall, |s, _| {
        if join {
            s.join_multicast_v4(&group, &iface_addr)
        } else {
            s.leave_multicast_v4(&group, &iface_addr)
        }
    })
}

#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
fn socket_buffer_size(socket: &UdpSocket, receive: bool) -> std::io::Result<i32> {
    use std::os::fd::AsRawFd;

    #[cfg(target_os = "linux")]
    const SOL_SOCKET: i32 = 1;
    #[cfg(target_os = "linux")]
    const SO_RCVBUF: i32 = 8;
    #[cfg(target_os = "linux")]
    const SO_SNDBUF: i32 = 7;
    #[cfg(target_os = "macos")]
    const SOL_SOCKET: i32 = 0xffff;
    #[cfg(target_os = "macos")]
    const SO_RCVBUF: i32 = 0x1002;
    #[cfg(target_os = "macos")]
    const SO_SNDBUF: i32 = 0x1001;

    extern "C" {
        fn getsockopt(
            socket: i32,
            level: i32,
            name: i32,
            value: *mut std::os::raw::c_void,
            length: *mut u32,
        ) -> i32;
    }
    let mut value = 0i32;
    let mut length = std::mem::size_of::<i32>() as u32;
    // SAFETY: the socket fd is live and both output pointers reference initialized writable values.
    let result = unsafe {
        getsockopt(
            socket.as_raw_fd(),
            SOL_SOCKET,
            if receive { SO_RCVBUF } else { SO_SNDBUF },
            &mut value as *mut i32 as *mut _,
            &mut length,
        )
    };
    if result == 0 {
        Ok(value)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
fn set_socket_buffer_size(socket: &UdpSocket, receive: bool, size: i32) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    #[cfg(target_os = "linux")]
    const SOL_SOCKET: i32 = 1;
    #[cfg(target_os = "linux")]
    const SO_RCVBUF: i32 = 8;
    #[cfg(target_os = "linux")]
    const SO_SNDBUF: i32 = 7;
    #[cfg(target_os = "macos")]
    const SOL_SOCKET: i32 = 0xffff;
    #[cfg(target_os = "macos")]
    const SO_RCVBUF: i32 = 0x1002;
    #[cfg(target_os = "macos")]
    const SO_SNDBUF: i32 = 0x1001;

    extern "C" {
        fn setsockopt(
            socket: i32,
            level: i32,
            name: i32,
            value: *const std::os::raw::c_void,
            length: u32,
        ) -> i32;
    }
    // SAFETY: the socket fd is live and the input pointer references a valid i32.
    let result = unsafe {
        setsockopt(
            socket.as_raw_fd(),
            SOL_SOCKET,
            if receive { SO_RCVBUF } else { SO_SNDBUF },
            &size as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
const WIN_SOL_SOCKET: i32 = 0xffff;
#[cfg(windows)]
const WIN_SO_SNDBUF: i32 = 0x1001;
#[cfg(windows)]
const WIN_SO_RCVBUF: i32 = 0x1002;

#[cfg(windows)]
fn socket_buffer_size(socket: &UdpSocket, receive: bool) -> std::io::Result<i32> {
    use std::os::windows::io::AsRawSocket;
    #[link(name = "ws2_32")]
    extern "system" {
        fn getsockopt(
            socket: usize,
            level: i32,
            name: i32,
            value: *mut u8,
            length: *mut i32,
        ) -> i32;
        fn WSAGetLastError() -> i32;
    }
    let mut value = 0i32;
    let mut length = std::mem::size_of::<i32>() as i32;
    let name = if receive {
        WIN_SO_RCVBUF
    } else {
        WIN_SO_SNDBUF
    };
    // SAFETY: the socket is live for the borrow; value/length describe a valid i32 buffer.
    let rc = unsafe {
        getsockopt(
            socket.as_raw_socket() as usize,
            WIN_SOL_SOCKET,
            name,
            &mut value as *mut i32 as *mut u8,
            &mut length,
        )
    };
    if rc == 0 {
        Ok(value)
    } else {
        // SAFETY: reads this thread's last Winsock error.
        Err(std::io::Error::from_raw_os_error(unsafe {
            WSAGetLastError()
        }))
    }
}

#[cfg(windows)]
fn set_socket_buffer_size(socket: &UdpSocket, receive: bool, size: i32) -> std::io::Result<()> {
    let name = if receive {
        WIN_SO_RCVBUF
    } else {
        WIN_SO_SNDBUF
    };
    win_setsockopt(socket, WIN_SOL_SOCKET, name, &size)
}

#[cfg(not(any(windows, all(unix, any(target_os = "macos", target_os = "linux")))))]
fn socket_buffer_size(_socket: &UdpSocket, _receive: bool) -> std::io::Result<i32> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "socket buffer sizes are unavailable",
    ))
}

#[cfg(not(any(windows, all(unix, any(target_os = "macos", target_os = "linux")))))]
fn set_socket_buffer_size(_socket: &UdpSocket, _receive: bool, _size: i32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "socket buffer sizes are unavailable",
    ))
}

// ---- IPV6_UNICAST_HOPS via setsockopt (std exposes no IPv6 hop-limit setter) --------------------

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn set_ipv6_unicast_hops(socket: &UdpSocket, hops: i32) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    const IPPROTO_IPV6: i32 = 41;
    #[cfg(target_os = "macos")]
    const IPV6_UNICAST_HOPS: i32 = 4;
    #[cfg(target_os = "linux")]
    const IPV6_UNICAST_HOPS: i32 = 16;

    extern "C" {
        fn setsockopt(
            sockfd: i32,
            level: i32,
            optname: i32,
            optval: *const std::os::raw::c_void,
            optlen: u32,
        ) -> i32;
    }

    // SAFETY: the fd is a live socket owned by `socket`; optval points to a valid i32.
    let rc = unsafe {
        setsockopt(
            socket.as_raw_fd(),
            IPPROTO_IPV6,
            IPV6_UNICAST_HOPS,
            &hops as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
fn set_ipv6_unicast_hops(socket: &UdpSocket, hops: i32) -> std::io::Result<()> {
    const IPPROTO_IPV6: i32 = 41;
    const IPV6_UNICAST_HOPS: i32 = 4;
    win_setsockopt(socket, IPPROTO_IPV6, IPV6_UNICAST_HOPS, &hops)
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
fn set_ipv6_unicast_hops(_socket: &UdpSocket, _hops: i32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "setTTL for udp6 is not supported in lumen on this platform (no IPV6_UNICAST_HOPS access)",
    ))
}

// ---- SO_KEEPALIVE via setsockopt (std exposes no keepalive) -------------------------------------

#[cfg(all(unix, any(target_os = "macos", target_os = "linux")))]
fn set_keep_alive(stream: &TcpStream, on: bool, idle_secs: i32) -> bool {
    use std::os::unix::io::AsRawFd;

    #[cfg(target_os = "linux")]
    const SOL_SOCKET: i32 = 1;
    #[cfg(target_os = "linux")]
    const SO_KEEPALIVE: i32 = 9;
    #[cfg(target_os = "linux")]
    const TCP_KEEPIDLE: i32 = 4;
    #[cfg(target_os = "macos")]
    const SOL_SOCKET: i32 = 0xffff;
    #[cfg(target_os = "macos")]
    const SO_KEEPALIVE: i32 = 0x0008;
    #[cfg(target_os = "macos")]
    const TCP_KEEPALIVE: i32 = 0x10;
    const IPPROTO_TCP: i32 = 6;

    extern "C" {
        fn setsockopt(
            sockfd: i32,
            level: i32,
            optname: i32,
            optval: *const std::os::raw::c_void,
            optlen: u32,
        ) -> i32;
    }

    let fd = stream.as_raw_fd();
    let flag: i32 = if on { 1 } else { 0 };
    // SAFETY: fd is a live socket owned by `stream`; optval points to a valid i32 of optlen bytes.
    let rc = unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_KEEPALIVE,
            &flag as *const i32 as *const _,
            std::mem::size_of::<i32>() as u32,
        )
    };
    if rc != 0 {
        return false;
    }
    if on && idle_secs > 0 {
        #[cfg(target_os = "linux")]
        let idle_opt = TCP_KEEPIDLE;
        #[cfg(target_os = "macos")]
        let idle_opt = TCP_KEEPALIVE;
        // SAFETY: as above; failure to tune the idle interval is non-fatal (keepalive is still on).
        unsafe {
            setsockopt(
                fd,
                IPPROTO_TCP,
                idle_opt,
                &idle_secs as *const i32 as *const _,
                std::mem::size_of::<i32>() as u32,
            );
        }
    }
    true
}

#[cfg(not(all(unix, any(target_os = "macos", target_os = "linux"))))]
fn set_keep_alive(_stream: &TcpStream, _on: bool, _idle_secs: i32) -> bool {
    false
}

// ---- ops ----------------------------------------------------------------------------------------

pub(crate) use tcp_bindings::Module;
pub(crate) use udp_bindings::Module as UdpModule;

/// A JS number as an unsigned id (anything that is not a non-negative number is 0).
fn uid(n: f64) -> u64 {
    n as u64
}

/// A JS number as a port (truncating like a C cast).
fn port_of(n: f64) -> u16 {
    n as u64 as u16
}

/// A queued accept: one connection off a listener that is not blocking.
fn accept_step(listener: Arc<NetListener>) -> impl FnMut(Trigger) -> Step + Send + 'static {
    move |trigger| {
        if let Trigger::Failed(_) = trigger {
            return Step::Done(Box::new(AcceptResult::Closed));
        }
        loop {
            match listener.accept() {
                Ok(stream) => {
                    // BSD sockets inherit the listener's O_NONBLOCK; calls on the stream pass
                    // MSG_DONTWAIT themselves, and a descriptor handed to a child must block.
                    #[cfg(unix)]
                    let _ = match &stream {
                        NetStream::Tcp(s) => s.set_nonblocking(false),
                        NetStream::Unix(s) => s.set_nonblocking(false),
                    };
                    return Step::Done(Box::new(AcceptResult::Conn(stream)));
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Step::Again,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::Interrupted | std::io::ErrorKind::ConnectionAborted
                    ) => {}
                Err(_) => return Step::Done(Box::new(AcceptResult::Closed)),
            }
        }
    }
}

/// Named pipes (Windows) have no readiness registration: their reads, writes and accepts block a
/// dedicated thread, as they always did.
#[cfg(windows)]
fn pipe_read(
    ctx: &mut Ctx,
    sid: u64,
    stream: Arc<NetStream>,
    unref: bool,
    resolve: Value,
    reject: Value,
) -> Result<(), OpError> {
    let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_read);
    if unref {
        ctx.host_mut::<TaskRegistry>().expect("registry").set_unref(id);
    }
    if let Some(e) = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.sockets.get_mut(&sid))
    {
        e.pending = Some(id);
    }
    completions(ctx).run_blocking(id, move || {
        let mut buf = vec![0u8; READ_CHUNK];
        let mut s: &NetStream = &stream;
        let result: Result<Vec<u8>, NetErr> = match s.read(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                Ok(buf)
            }
            Err(e) => Err(net_err("read", &e, None)),
        };
        Box::new(result)
    });
    Ok(())
}

#[cfg(windows)]
fn pipe_write(
    ctx: &mut Ctx,
    stream: Arc<NetStream>,
    data: Vec<u8>,
    resolve: Value,
    reject: Value,
) -> Result<(), OpError> {
    let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_write);
    completions(ctx).run_blocking(id, move || {
        let mut s: &NetStream = &stream;
        let result: Result<(), NetErr> = s
            .write_all(&data)
            .and_then(|()| s.flush())
            .map_err(|e| net_err("write", &e, None));
        Box::new(result)
    });
    Ok(())
}

#[cfg(windows)]
fn pipe_accept(
    ctx: &mut Ctx,
    sid: u64,
    listener: Arc<NetListener>,
    closed: Arc<AtomicBool>,
    unref: bool,
    resolve: Value,
    reject: Value,
) -> Result<(), OpError> {
    let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_accept);
    if unref {
        ctx.host_mut::<TaskRegistry>().expect("registry").set_unref(id);
    }
    if let Some(e) = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.servers.get_mut(&sid))
    {
        e.pending = Some(id);
    }
    completions(ctx).run_blocking(id, move || {
        let result = match listener.accept() {
            Ok(stream) if !closed.load(Ordering::SeqCst) => AcceptResult::Conn(stream),
            _ => AcceptResult::Closed,
        };
        Box::new(result)
    });
    Ok(())
}

#[lumen_bind::module(name = "__net")]
mod tcp_bindings {
    use super::*;

    #[op(coerce, name = "adoptFd")]
    fn op_adopt_fd(ctx: &mut Ctx, fd: f64) -> Result<Value, OpError> {
        fdpass::adopt_fd(ctx, fd)
    }

    #[op(coerce, name = "socketFd")]
    fn op_socket_fd(ctx: &mut Ctx, sid: f64) -> f64 {
        fdpass::socket_fd(ctx, uid(sid))
    }

    #[op(coerce, name = "serverFd")]
    fn op_server_fd(ctx: &mut Ctx, sid: f64) -> f64 {
        fdpass::server_fd(ctx, uid(sid))
    }

    #[op(coerce, name = "release")]
    fn op_release(ctx: &mut Ctx, sid: f64) {
        fdpass::release(ctx, uid(sid));
    }

    #[op(coerce, name = "readMsg")]
    fn op_read_msg(ctx: &mut Ctx, sid: f64, resolve: Value, reject: Value) -> Result<(), OpError> {
        fdpass::read_msg(ctx, uid(sid), resolve, reject)
    }

    #[op(coerce, name = "trySendMsg")]
    fn op_try_send_msg(ctx: &mut Ctx, sid: f64, data: &[u8], fd: Option<f64>) -> f64 {
        fdpass::try_send_msg(ctx, uid(sid), data, fd)
    }

    #[op(coerce, name = "writeMsg")]
    fn op_write_msg(
        ctx: &mut Ctx,
        sid: f64,
        data: Vec<u8>,
        fd: Option<f64>,
        resolve: Value,
        reject: Value,
    ) -> Result<(), OpError> {
        fdpass::write_msg(ctx, uid(sid), data, fd, resolve, reject)
    }

    #[op(coerce, name = "guessHandle")]
    fn op_guess_handle(fd: Option<f64>) -> &'static str {
        fdpass::guess_handle(fd)
    }

    #[op(coerce, name = "dupFd")]
    fn op_dup_fd(fd: Option<f64>) -> f64 {
        fdpass::dup_fd(fd)
    }

    #[op(coerce, name = "closeFd")]
    fn op_close_fd(fd: Option<f64>) {
        fdpass::close_fd(fd);
    }

    /// `(host, port, localHost, localPort, resolve, reject)` — resolve the host, connect (from the
    /// given local address/port when set), and settle with the socket descriptor (see
    /// [`register_stream`]) or reject with an errno-tagged error.
    #[op(coerce, name = "connect")]
    fn op_connect(
        ctx: &mut Ctx,
        host: String,
        port: f64,
        local_host: String,
        local_port: f64,
        resolve: Value,
        reject: Value,
    ) -> Result<(), OpError> {
        let port = port_of(port);
        let local_port = port_of(local_port);
        let (resolve, reject) = take_resolve_reject(Some(&resolve), Some(&reject))?;
        let local_ip: Option<IpAddr> = if local_host.is_empty() {
            None
        } else {
            local_host.parse().ok()
        };
        let local = (local_ip.is_some() || local_port != 0).then_some((local_ip, local_port));

        #[cfg(unix)]
        {
            let reactor = loop_reactor(ctx)?;
            let sender = completions(ctx);
            let spawn = ctx.op_state().get::<lumen_host::SpawnHandle>().cloned();
            let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_connect);
            let attempt = ready::ConnectAttempt::new();
            if let Some(reg) = ctx.host_mut::<NetRegistry>() {
                reg.connecting.insert(id, attempt.clone());
            }
            let literal = host.parse::<IpAddr>().is_ok();
            let start = move || match resolve_host(&host, port) {
                Ok(addrs) => drive_connect(ConnectJob {
                    id,
                    addrs: addrs.into(),
                    last: None,
                    host,
                    port,
                    local,
                    attempt,
                    reactor,
                    completions: sender,
                }),
                Err(e) => sender.send(id, Box::new(ConnectOutcome { id, result: Err(e) })),
            };
            match spawn {
                Some(spawn) if !literal => spawn.spawn_detached(Box::new(start)),
                _ => start(),
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = local;
            let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_connect);
            completions(ctx).run_blocking(id, move || {
                let result = resolve_host(&host, port).and_then(|addrs| {
                    let mut last = None;
                    for addr in addrs {
                        match connect_tcp(addr) {
                            Ok(s) => return Ok(NetStream::Tcp(s)),
                            Err(e) => last = Some(e),
                        }
                    }
                    Err(net_err(
                        "connect",
                        &last.unwrap_or_else(|| std::io::Error::other("no address")),
                        Some((host.clone(), port)),
                    ))
                });
                Box::new(ConnectOutcome { id, result })
            });
            Ok(())
        }
    }

    /// `(path, resolve, reject)` — connect to a Unix-domain socket path (a named pipe on Windows).
    /// The connect of a local path ends at once, but may block while the listener's backlog is
    /// full, so it stays on a dedicated thread.
    #[op(coerce, name = "connectPath")]
    fn op_connect_path(
        ctx: &mut Ctx,
        path: String,
        resolve: Value,
        reject: Value,
    ) -> Result<(), OpError> {
        let (resolve, reject) = take_resolve_reject(Some(&resolve), Some(&reject))?;
        #[cfg(unix)]
        {
            let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_connect);
            completions(ctx).run_blocking(id, move || {
                let result = UnixStream::connect(&path)
                    .map(NetStream::Unix)
                    .map_err(|error| path_err("connect", path, error));
                Box::new(ConnectOutcome { id, result })
            });
            Ok(())
        }
        #[cfg(windows)]
        {
            let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_connect);
            completions(ctx).run_blocking(id, move || {
                let result = crate::win_pipe::PipeStream::connect(&path)
                    .map(NetStream::Pipe)
                    .map_err(|error| path_err("connect", path, error));
                Box::new(ConnectOutcome { id, result })
            });
            Ok(())
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (resolve, reject);
            Err(NativeError::runtime(format!(
                "Unix-domain sockets are not supported on this platform: {path}"
            )))
        }
    }

    /// `(socketId, resolve, reject)` — read one chunk; resolves with a Uint8Array, or `null` at EOF.
    #[op(coerce, name = "read")]
    fn op_read(ctx: &mut Ctx, sid: f64, resolve: Value, reject: Value) -> Result<(), OpError> {
        let sid = uid(sid);
        let (resolve, reject) = take_resolve_reject(Some(&resolve), Some(&reject))?;

        let found = ctx
            .host_mut::<NetRegistry>()
            .and_then(|r| r.sockets.get(&sid))
            .map(|e| (e.stream.clone(), e.unref, e.io.clone()));
        let Some((stream, unref, io)) = found else {
            enqueue(ctx, resolve, vec![Value::Null]); // gone → treat as EOF
            return Ok(());
        };
        let Some(source) = stream.source() else {
            #[cfg(windows)]
            return pipe_read(ctx, sid, stream, unref, resolve, reject);
            #[cfg(not(windows))]
            return Err(no_readiness_source());
        };
        let (io, created) = io_for(ctx, io, source, stream.clone())?;

        let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_read);
        if unref {
            ctx.host_mut::<TaskRegistry>().expect("registry").set_unref(id);
        }
        if let Some(e) = ctx
            .host_mut::<NetRegistry>()
            .and_then(|r| r.sockets.get_mut(&sid))
        {
            e.pending = Some(id);
            if created {
                e.io = Some(io.clone());
            }
        }
        io.submit(Dir::Read, id, true, read_step(stream));
        Ok(())
    }

    /// `(socketId, bytes, resolve, reject)` — write all bytes; resolves when flushed.
    #[op(coerce, name = "write")]
    fn op_write(
        ctx: &mut Ctx,
        sid: f64,
        data: Vec<u8>,
        resolve: Value,
        reject: Value,
    ) -> Result<(), OpError> {
        let sid = uid(sid);
        let (resolve, reject) = take_resolve_reject(Some(&resolve), Some(&reject))?;

        let found = ctx
            .host_mut::<NetRegistry>()
            .and_then(|r| r.sockets.get(&sid))
            .map(|e| (e.stream.clone(), e.io.clone()));
        let Some((stream, io)) = found else {
            let err = net_error_value(
                ctx,
                &NetErr {
                    code: "EPIPE",
                    syscall: "write",
                    message: "This socket has been ended by the other party".to_string(),
                    address: None,
                    port: None,
                    errno: None,
                },
            );
            enqueue(ctx, reject, vec![err]);
            return Ok(());
        };
        let Some(source) = stream.source() else {
            #[cfg(windows)]
            return pipe_write(ctx, stream, data, resolve, reject);
            #[cfg(not(windows))]
            return Err(no_readiness_source());
        };
        let (io, created) = io_for(ctx, io, source, stream.clone())?;

        let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_write);
        if created {
            if let Some(e) = ctx
                .host_mut::<NetRegistry>()
                .and_then(|r| r.sockets.get_mut(&sid))
            {
                e.io = Some(io.clone());
            }
        }
        io.submit(Dir::Write, id, true, write_step(stream, data));
        Ok(())
    }

    /// `(socketId, bytes)` — libuv's `uv_try_write`: write what the kernel takes right now without
    /// blocking, on the loop thread. Returns the byte count (0 when nothing could be written; errors
    /// are left to the async write that follows). The caller only uses it with no write in flight.
    #[op(coerce, name = "tryWrite")]
    fn op_try_write(ctx: &mut Ctx, sid: f64, data: &[u8]) -> f64 {
        let sid = uid(sid);
        let stream = ctx
            .host_mut::<NetRegistry>()
            .and_then(|r| r.sockets.get(&sid))
            .map(|e| e.stream.clone());
        let Some(stream) = stream else {
            return 0.0;
        };
        try_write(&stream, data) as f64
    }

    /// `(socketId)` — half-close: shut the write half so the peer sees EOF; our read half stays open.
    #[op(coerce, name = "endWritable")]
    fn op_end_writable(ctx: &mut Ctx, sid: f64) {
        let sid = uid(sid);
        if let Some(e) = ctx
            .host_mut::<NetRegistry>()
            .and_then(|r| r.sockets.get(&sid))
        {
            let _ = e.stream.shutdown(Shutdown::Write);
        }
    }

    /// `(socketId)` — full close: shut both halves (waking a blocked read) and drop the handle.
    #[op(coerce, name = "close")]
    fn op_close(ctx: &mut Ctx, sid: f64) {
        let sid = uid(sid);
        let pending = match ctx.host_mut::<NetRegistry>() {
            Some(reg) => reg.sockets.remove(&sid).and_then(|e| {
                let _ = e.stream.shutdown(Shutdown::Both);
                if let Some(io) = &e.io {
                    io.close(closed_write_error);
                }
                e.pending
            }),
            None => None,
        };
        // Closing cancels the in-flight read, as uv_close does: on Windows a recv blocked on a
        // shut-down socket only returns once the peer closes too, and it must not hold the loop
        // open (or run its callback) meanwhile.
        if let (Some(id), Some(tasks)) = (pending, ctx.host_mut::<TaskRegistry>()) {
            tasks.take(id);
        }
    }

    /// `(socketId, on)` — `socket.setNoDelay`. Returns whether it took effect.
    #[op(coerce, name = "setNoDelay")]
    fn op_set_no_delay(ctx: &mut Ctx, sid: f64, on: Option<bool>) -> bool {
        let sid = uid(sid);
        let on = on != Some(false);
        ctx.host_mut::<NetRegistry>()
            .and_then(|r| r.sockets.get(&sid))
            .map(|e| e.stream.set_nodelay(on).is_ok())
            .unwrap_or(false)
    }

    /// `(socketId, on, initialDelayMs)` — `socket.setKeepAlive`, via `setsockopt(SO_KEEPALIVE)` on
    /// unix (std exposes no keepalive). A no-op returning `false` elsewhere.
    #[op(coerce, name = "setKeepAlive")]
    fn op_set_keep_alive(ctx: &mut Ctx, sid: f64, on: bool, delay_ms: Option<f64>) -> bool {
        let sid = uid(sid);
        let delay_ms = delay_ms.unwrap_or(0.0);
        let stream = ctx
            .host_mut::<NetRegistry>()
            .and_then(|r| r.sockets.get(&sid))
            .map(|e| e.stream.clone());
        let Some(stream) = stream else {
            return false;
        };
        stream
            .tcp()
            .map(|tcp| set_keep_alive(tcp, on, (delay_ms / 1000.0) as i32))
            .unwrap_or(false)
    }

    /// `(socketId)` — `socket.address()`; `null` if the socket is gone.
    #[op(coerce, name = "address")]
    fn op_address(ctx: &mut Ctx, sid: f64) -> Value {
        let sid = uid(sid);
        let addr = ctx
            .host_mut::<NetRegistry>()
            .and_then(|r| r.sockets.get(&sid))
            .and_then(|e| e.stream.local_addr().ok().flatten());
        match addr {
            Some(a) => addr_object(ctx, &a),
            None => Value::Null,
        }
    }

    /// `(socketId, unref)` — `socket.ref()`/`unref()`; toggles whether the pending read keeps the
    /// loop alive.
    #[op(coerce, name = "socketRef")]
    fn op_socket_ref(ctx: &mut Ctx, sid: f64, unref: bool) {
        let sid = uid(sid);
        let pending = ctx.host_mut::<NetRegistry>().and_then(|r| {
            r.sockets.get_mut(&sid).map(|e| {
                e.unref = unref;
                e.pending
            })
        });
        if let (Some(Some(id)), Some(reg)) = (pending, ctx.host_mut::<TaskRegistry>()) {
            if unref {
                reg.set_unref(id);
            } else {
                reg.set_ref(id);
            }
        }
    }

    /// `(host, port, backlog, flags)` — bind a listener (synchronous, like `std`); returns
    /// `{ serverId, address, port, family }` or throws an errno-tagged error (`EADDRINUSE`, …).
    #[op(coerce, name = "listen")]
    fn op_listen(
        ctx: &mut Ctx,
        host: String,
        port: f64,
        backlog: Option<f64>,
        flags: Option<f64>,
    ) -> Result<Value, OpError> {
        let host = if host.is_empty() {
            "0.0.0.0".to_string()
        } else {
            host
        };
        let port = port_of(port);
        let backlog = backlog.filter(|b| *b > 0.0).unwrap_or(511.0) as i32;
        let flags = flags.map_or(0, uid) as u32;

        let listener = match listen_tcp(&host, port, backlog, flags) {
            Ok(l) => l,
            Err(e) => {
                let err = net_err("listen", &e, Some((host, port)));
                return Err(net_error(&err).into());
            }
        };
        let local_addr = listener
            .local_addr()
            .map_err(|e| NativeError::runtime(format!("local_addr: {e}")))?;

        let reg = ctx
            .host_mut::<NetRegistry>()
            .expect("net registry installed");
        let id = reg.next_server;
        reg.next_server += 1;
        reg.servers.insert(
            id,
            ServerEntry {
                listener: Arc::new({
                    let listener = NetListener::Tcp(listener);
                    listener.set_nonblocking();
                    listener
                }),
                closed: Arc::new(AtomicBool::new(false)),
                io: None,
                local_addr: ServerAddress::Tcp(local_addr),
                unref: false,
                pending: None,
                owns_path: false,
            },
        );

        let o = Value::Obj(ctx.new_object());
        let _ = ctx.set_member(&o, "serverId", Value::Num(id as f64));
        let _ = ctx.set_member(
            &o,
            "address",
            Value::from_string(local_addr.ip().to_string()),
        );
        let _ = ctx.set_member(&o, "port", Value::Num(local_addr.port() as f64));
        let _ = ctx.set_member(&o, "family", Value::str(family_of(&local_addr)));
        Ok(o)
    }

    #[op(coerce, name = "listenPath")]
    fn op_listen_path(ctx: &mut Ctx, path: String) -> Result<Value, OpError> {
        #[cfg(unix)]
        {
            let listener = UnixListener::bind(&path)
                .map_err(|error| net_error(&path_err("listen", path.clone(), error)))?;
            let reg = ctx
                .host_mut::<NetRegistry>()
                .expect("net registry installed");
            let id = reg.next_server;
            reg.next_server += 1;
            reg.servers.insert(
                id,
                ServerEntry {
                    listener: Arc::new({
                        let listener = NetListener::Unix(listener);
                        listener.set_nonblocking();
                        listener
                    }),
                    closed: Arc::new(AtomicBool::new(false)),
                    io: None,
                    local_addr: ServerAddress::Unix(path.clone()),
                    unref: false,
                    pending: None,
                    owns_path: true,
                },
            );
            let object = Value::Obj(ctx.new_object());
            let _ = ctx.set_member(&object, "serverId", Value::Num(id as f64));
            let _ = ctx.set_member(&object, "address", Value::from_string(path));
            Ok(object)
        }
        #[cfg(windows)]
        {
            let listener = crate::win_pipe::PipeListener::bind(&path)
                .map_err(|error| net_error(&path_err("listen", path.clone(), error)))?;
            let reg = ctx
                .host_mut::<NetRegistry>()
                .expect("net registry installed");
            let id = reg.next_server;
            reg.next_server += 1;
            reg.servers.insert(
                id,
                ServerEntry {
                    listener: Arc::new(NetListener::Pipe(listener)),
                    closed: Arc::new(AtomicBool::new(false)),
                    io: None,
                    local_addr: ServerAddress::Pipe(path.clone()),
                    unref: false,
                    pending: None,
                    owns_path: true,
                },
            );
            let object = Value::Obj(ctx.new_object());
            let _ = ctx.set_member(&object, "serverId", Value::Num(id as f64));
            let _ = ctx.set_member(&object, "address", Value::from_string(path));
            Ok(object)
        }
        #[cfg(not(any(unix, windows)))]
        {
            Err(NativeError::runtime(format!(
                "Unix-domain sockets are not supported on this platform: {path}"
            )))
        }
    }

    /// `(serverId, resolve, reject)` — accept one connection; resolves with a socket descriptor (see
    /// [`register_stream`]) or `null` once the server is closed.
    #[op(coerce, name = "accept")]
    fn op_accept(ctx: &mut Ctx, sid: f64, resolve: Value, reject: Value) -> Result<(), OpError> {
        let sid = uid(sid);
        let (resolve, reject) = take_resolve_reject(Some(&resolve), Some(&reject))?;

        let found = ctx.host_mut::<NetRegistry>().and_then(|r| {
            r.servers
                .get(&sid)
                .map(|e| (e.listener.clone(), e.closed.clone(), e.unref, e.io.clone()))
        });
        let Some((listener, closed, unref, io)) = found else {
            enqueue(ctx, resolve, vec![Value::Null]);
            return Ok(());
        };
        let Some(source) = listener.source() else {
            #[cfg(windows)]
            return pipe_accept(ctx, sid, listener, closed, unref, resolve, reject);
            #[cfg(not(windows))]
            return Err(no_readiness_source());
        };
        #[cfg(not(windows))]
        let _ = closed;
        let (io, created) = io_for(ctx, io, source, listener.clone())?;

        let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_accept);
        if unref {
            ctx.host_mut::<TaskRegistry>().expect("registry").set_unref(id);
        }
        if let Some(e) = ctx
            .host_mut::<NetRegistry>()
            .and_then(|r| r.servers.get_mut(&sid))
        {
            e.pending = Some(id);
            if created {
                e.io = Some(io.clone());
            }
        }
        io.submit(Dir::Read, id, true, accept_step(listener));
        Ok(())
    }

    /// `(serverId)` — stop the listener: its pending accept settles `null`.
    #[op(coerce, name = "closeServer")]
    fn op_close_server(ctx: &mut Ctx, sid: f64) {
        let sid = uid(sid);
        let entry = ctx
            .host_mut::<NetRegistry>()
            .and_then(|r| r.servers.remove(&sid));
        if let Some(entry) = entry {
            let sender = ctx.op_state().get::<CompletionSender>().cloned();
            close_server_entry(entry, sender.as_ref());
        }
    }

    /// `(serverId)` — `server.address()`; `null` if the server is gone.
    #[op(coerce, name = "serverAddress")]
    fn op_server_address(ctx: &mut Ctx, sid: f64) -> Value {
        let sid = uid(sid);
        let addr = ctx
            .host_mut::<NetRegistry>()
            .and_then(|r| r.servers.get(&sid))
            .map(|e| e.local_addr.clone());
        match addr {
            Some(ServerAddress::Tcp(a)) => addr_object(ctx, &a),
            #[cfg(unix)]
            Some(ServerAddress::Unix(path)) => Value::from_string(path),
            #[cfg(windows)]
            Some(ServerAddress::Pipe(path)) => Value::from_string(path),
            None => Value::Null,
        }
    }

    /// `(serverId, unref)` — `server.ref()`/`unref()`.
    #[op(coerce, name = "serverRef")]
    fn op_server_ref(ctx: &mut Ctx, sid: f64, unref: bool) {
        let sid = uid(sid);
        let pending = ctx.host_mut::<NetRegistry>().and_then(|r| {
            r.servers.get_mut(&sid).map(|e| {
                e.unref = unref;
                e.pending
            })
        });
        if let (Some(Some(id)), Some(reg)) = (pending, ctx.host_mut::<TaskRegistry>()) {
            if unref {
                reg.set_unref(id);
            } else {
                reg.set_ref(id);
            }
        }
    }
}

#[lumen_bind::module(name = "__udp")]
mod udp_bindings {
    use super::*;

    #[op(coerce, name = "fd")]
    fn op_udp_fd(ctx: &mut Ctx, sid: f64) -> f64 {
        fdpass::udp_fd(ctx, uid(sid))
    }

    /// `(type, host, port, flags)` — bind a UDP socket (`type` = "udp4"|"udp6"; `host` an IP, or
    /// empty for the wildcard; `flags` the libuv `UV_UDP_*` bits). Returns
    /// `{ socketId, address, port, family }` or throws an errno-tagged error.
    #[op(coerce, name = "bind")]
    fn op_udp_bind(
        ctx: &mut Ctx,
        kind: String,
        host: String,
        port: f64,
        flags: Option<f64>,
    ) -> Result<Value, OpError> {
        let kind6 = kind == "udp6";
        let host = if !host.is_empty() {
            host
        } else if kind6 {
            "::".to_string()
        } else {
            "0.0.0.0".to_string()
        };
        let port = port_of(port);
        let flags = flags.map_or(0, uid) as u32;

        let addr = match parse_udp_addr(&host, port, kind6) {
            Ok(a) => a,
            Err(e) => return Err(net_error(&net_err("bind", &e, Some((host, port)))).into()),
        };
        let socket = match bind_udp(addr, flags) {
            Ok(s) => s,
            Err(e) => return Err(net_error(&net_err("bind", &e, Some((host, port)))).into()),
        };
        let local = socket
            .local_addr()
            .map_err(|e| NativeError::runtime(format!("local_addr: {e}")))?;

        let reg = ctx
            .host_mut::<DgramRegistry>()
            .expect("dgram registry installed");
        let id = reg.next;
        reg.next += 1;
        reg.sockets.insert(
            id,
            UdpEntry {
                socket: Arc::new(socket),
                kind6,
                unref: false,
                pending: None,
                io: None,
            },
        );

        let o = Value::Obj(ctx.new_object());
        let _ = ctx.set_member(&o, "socketId", Value::Num(id as f64));
        let _ = ctx.set_member(&o, "address", Value::from_string(local.ip().to_string()));
        let _ = ctx.set_member(&o, "port", Value::Num(local.port() as f64));
        let _ = ctx.set_member(&o, "family", Value::str(family_of(&local)));
        Ok(o)
    }

    /// `(socketId, host, port)` — `connect(2)` the datagram socket to one peer.
    #[op(coerce, name = "connect")]
    fn op_udp_connect(ctx: &mut Ctx, sid: f64, host: String, port: f64) -> Result<(), OpError> {
        let sid = uid(sid);
        let port = port_of(port);
        let found = ctx
            .host_mut::<DgramRegistry>()
            .and_then(|r| r.sockets.get(&sid))
            .map(|e| (e.socket.clone(), e.kind6));
        let Some((socket, kind6)) = found else {
            return Err(NativeError::runtime("dgram: unknown socket").into());
        };
        let result = parse_udp_addr(&host, port, kind6).and_then(|addr| socket.connect(addr));
        result.map_err(|e| OpError::from(net_error(&net_err("connect", &e, Some((host, port))))))
    }

    /// `(socketId)` — dissolve the association made by `connect`.
    #[op(coerce, name = "disconnect")]
    fn op_udp_disconnect(ctx: &mut Ctx, sid: f64) -> Result<(), OpError> {
        with_udp(ctx, uid(sid), "disconnect", |s, _| disconnect_udp(s))
    }

    /// `(socketId)` — the connected peer as `{ address, family, port }`, or `null`.
    #[op(coerce, name = "peer")]
    fn op_udp_peer(ctx: &mut Ctx, sid: f64) -> Value {
        let sid = uid(sid);
        let addr = ctx
            .host_mut::<DgramRegistry>()
            .and_then(|r| r.sockets.get(&sid))
            .and_then(|e| e.socket.peer_addr().ok());
        match addr {
            Some(a) => addr_object(ctx, &a),
            None => Value::Null,
        }
    }

    /// `(socketId, resolve, reject)` — receive one datagram; resolves with
    /// `{ data, address, port, family, size }`. Closing the socket cancels the receive.
    #[op(coerce, name = "recv")]
    fn op_udp_recv(ctx: &mut Ctx, sid: f64, resolve: Value, reject: Value) -> Result<(), OpError> {
        let sid = uid(sid);
        let (resolve, reject) = take_resolve_reject(Some(&resolve), Some(&reject))?;

        let found = ctx.host_mut::<DgramRegistry>().and_then(|r| {
            r.sockets
                .get(&sid)
                .map(|e| (e.socket.clone(), e.unref, e.io.clone()))
        });
        let Some((socket, unref, io)) = found else {
            enqueue(ctx, resolve, vec![Value::Null]);
            return Ok(());
        };
        let source = udp_source(&socket).ok_or_else(no_readiness_source)?;
        let (io, created) = io_for(ctx, io, source, socket.clone())?;

        let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_recv);
        if unref {
            ctx.host_mut::<TaskRegistry>().expect("registry").set_unref(id);
        }
        if let Some(e) = ctx
            .host_mut::<DgramRegistry>()
            .and_then(|r| r.sockets.get_mut(&sid))
        {
            e.pending = Some(id);
            if created {
                e.io = Some(io.clone());
            }
        }
        io.submit(Dir::Read, id, true, recv_step(socket));
        Ok(())
    }

    /// `(socketId, bytes, port, address)` — send one datagram to `address:port` (a connected socket
    /// passes an empty address); returns the byte count or throws an errno-tagged error.
    #[op(coerce, name = "send")]
    fn op_udp_send(
        ctx: &mut Ctx,
        sid: f64,
        data: &[u8],
        port: f64,
        address: String,
    ) -> Result<f64, OpError> {
        let sid = uid(sid);
        let port = port_of(port);
        let found = ctx
            .host_mut::<DgramRegistry>()
            .and_then(|r| r.sockets.get(&sid))
            .map(|e| (e.socket.clone(), e.kind6));
        let Some((socket, kind6)) = found else {
            return Err(NativeError::runtime("dgram: unknown socket").into());
        };
        let sent = if address.is_empty() {
            socket.send(data)
        } else {
            parse_udp_addr(&address, port, kind6).and_then(|addr| socket.send_to(data, addr))
        };
        sent.map(|n| n as f64)
            .map_err(|e| OpError::from(net_error(&net_err("send", &e, None))))
    }

    /// `(socketId)` — close: deregister the socket and drop the handle.
    #[op(coerce, name = "close")]
    fn op_udp_close(ctx: &mut Ctx, sid: f64) {
        let sid = uid(sid);
        let pending = ctx
            .host_mut::<DgramRegistry>()
            .and_then(|reg| reg.sockets.remove(&sid))
            .and_then(|e| e.pending);
        // As with TCP (`op_close`): the in-flight receive is cancelled, so it neither holds the
        // loop open nor runs its callback after the close.
        if let (Some(id), Some(tasks)) = (pending, ctx.host_mut::<TaskRegistry>()) {
            tasks.take(id);
        }
    }

    #[op(coerce, name = "address")]
    fn op_udp_address(ctx: &mut Ctx, sid: f64) -> Value {
        let sid = uid(sid);
        let addr = ctx
            .host_mut::<DgramRegistry>()
            .and_then(|r| r.sockets.get(&sid))
            .and_then(|e| e.socket.local_addr().ok());
        match addr {
            Some(a) => addr_object(ctx, &a),
            None => Value::Null,
        }
    }

    #[op(coerce, name = "setBroadcast")]
    fn op_udp_set_broadcast(ctx: &mut Ctx, sid: f64, on: bool) -> Result<(), OpError> {
        with_udp(ctx, uid(sid), "setBroadcast", |s, _| s.set_broadcast(on))
    }

    #[op(coerce, name = "setTTL")]
    fn op_udp_set_ttl(ctx: &mut Ctx, sid: f64, ttl: Option<f64>) -> Result<(), OpError> {
        let ttl = ttl.unwrap_or(1.0);
        // std's set_ttl sets IP_TTL, which is invalid on an IPv6 socket; Node sets
        // IPV6_UNICAST_HOPS there, so do the same via setsockopt.
        with_udp(ctx, uid(sid), "setTTL", |s, kind6| {
            // libuv rejects anything outside 1..=255 before reaching the socket.
            if !(1.0..=255.0).contains(&ttl) {
                #[cfg(unix)]
                return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                #[cfg(not(unix))]
                return Err(std::io::Error::from_raw_os_error(10022));
            }
            let ttl = ttl as u32;
            if kind6 {
                set_ipv6_unicast_hops(s, ttl as i32)
            } else {
                s.set_ttl(ttl)
            }
        })
    }

    #[op(coerce, name = "setMulticastTTL")]
    fn op_udp_set_multicast_ttl(ctx: &mut Ctx, sid: f64, ttl: Option<f64>) -> Result<(), OpError> {
        let ttl = ttl.unwrap_or(1.0) as u32;
        with_udp(ctx, uid(sid), "setMulticastTTL", |s, kind6| {
            if kind6 {
                set_ipv6_multicast_hops(s, ttl as i32)
            } else {
                s.set_multicast_ttl_v4(ttl)
            }
        })
    }

    #[op(coerce, name = "setMulticastLoopback")]
    fn op_udp_set_multicast_loop(ctx: &mut Ctx, sid: f64, on: bool) -> Result<(), OpError> {
        with_udp(ctx, uid(sid), "setMulticastLoopback", |s, kind6| {
            if kind6 {
                s.set_multicast_loop_v6(on)
            } else {
                s.set_multicast_loop_v4(on)
            }
        })
    }

    #[op(coerce, name = "setMulticastInterface")]
    fn op_udp_set_multicast_interface(
        ctx: &mut Ctx,
        sid: f64,
        interface: String,
    ) -> Result<(), OpError> {
        with_udp(ctx, uid(sid), "setMulticastInterface", |socket, kind6| {
            set_multicast_interface(socket, kind6, &interface)
        })
    }

    /// `(socketId, multicastAddress, interface)` — join a multicast group. For udp4 `interface` is an
    /// IPv4 address (default `0.0.0.0`); for udp6 it is an interface index (default 0).
    #[op(coerce, name = "addMembership")]
    fn op_udp_add_membership(
        ctx: &mut Ctx,
        sid: f64,
        group: String,
        iface: Option<String>,
    ) -> Result<(), OpError> {
        udp_membership(ctx, uid(sid), group, iface, true)
    }

    #[op(coerce, name = "dropMembership")]
    fn op_udp_drop_membership(
        ctx: &mut Ctx,
        sid: f64,
        group: String,
        iface: Option<String>,
    ) -> Result<(), OpError> {
        udp_membership(ctx, uid(sid), group, iface, false)
    }

    #[op(coerce, name = "addSourceMembership")]
    fn op_udp_add_source_membership(
        ctx: &mut Ctx,
        sid: f64,
        source: String,
        group: String,
        iface: Option<String>,
    ) -> Result<(), OpError> {
        udp_source_membership(ctx, uid(sid), source, group, iface, true)
    }

    #[op(coerce, name = "dropSourceMembership")]
    fn op_udp_drop_source_membership(
        ctx: &mut Ctx,
        sid: f64,
        source: String,
        group: String,
        iface: Option<String>,
    ) -> Result<(), OpError> {
        udp_source_membership(ctx, uid(sid), source, group, iface, false)
    }

    /// `(socketId, unref)` — `dgram.ref()`/`unref()`.
    #[op(coerce, name = "udpRef")]
    fn op_udp_ref(ctx: &mut Ctx, sid: f64, unref: bool) {
        let sid = uid(sid);
        let pending = ctx.host_mut::<DgramRegistry>().and_then(|r| {
            r.sockets.get_mut(&sid).map(|e| {
                e.unref = unref;
                e.pending
            })
        });
        if let (Some(Some(id)), Some(reg)) = (pending, ctx.host_mut::<TaskRegistry>()) {
            if unref {
                reg.set_unref(id);
            } else {
                reg.set_ref(id);
            }
        }
    }

    #[op(coerce, name = "getBufferSize")]
    fn op_udp_get_buffer_size(ctx: &mut Ctx, sid: f64, receive: bool) -> Result<f64, OpError> {
        let sid = uid(sid);
        let socket = ctx
            .host_mut::<DgramRegistry>()
            .and_then(|registry| registry.sockets.get(&sid))
            .map(|entry| entry.socket.clone())
            .ok_or_else(|| NativeError::runtime("dgram: unknown socket"))?;
        socket_buffer_size(&socket, receive)
            .map(|size| size as f64)
            .map_err(|error| OpError::from(net_error(&net_err("getsockopt", &error, None))))
    }

    #[op(coerce, name = "setBufferSize")]
    fn op_udp_set_buffer_size(
        ctx: &mut Ctx,
        sid: f64,
        receive: bool,
        size: Option<f64>,
    ) -> Result<(), OpError> {
        let sid = uid(sid);
        let size = size.map_or(0, uid).min(i32::MAX as u64) as i32;
        let socket = ctx
            .host_mut::<DgramRegistry>()
            .and_then(|registry| registry.sockets.get(&sid))
            .map(|entry| entry.socket.clone())
            .ok_or_else(|| NativeError::runtime("dgram: unknown socket"))?;
        set_socket_buffer_size(&socket, receive, size)
            .map_err(|error| OpError::from(net_error(&net_err("setsockopt", &error, None))))
    }
}
