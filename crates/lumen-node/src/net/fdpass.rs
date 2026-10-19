//! Descriptor passing for the `child_process` IPC channel (unix): a socket handle travels to the
//! other process as `SCM_RIGHTS` ancillary data on the channel's Unix socket, as libuv does it, and
//! the receiver adopts the descriptor as a TCP/pipe socket, listener or UDP socket of its own.

use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};

use super::*;

/// Descriptors one `recvmsg` can carry; libuv accepts one per message, so this is ample.
const MAX_FDS: usize = 16;

fn set_cloexec(fd: RawFd) {
    let _ = lumen_os::fdctl::set_inheritable(fd, false);
}

fn sockopt_int(fd: RawFd, level: libc::c_int, name: libc::c_int) -> std::io::Result<libc::c_int> {
    lumen_os::net::getsockopt_int(fd, level, name).map_err(os_error)
}

fn socket_family(fd: RawFd) -> std::io::Result<libc::c_int> {
    Ok(lumen_os::net::getsockname(fd).map_err(os_error)?.family())
}

fn fd_error(syscall: &'static str, error: &std::io::Error) -> NativeError {
    net_error(&net_err(syscall, error, None))
}

fn fd_arg(n: f64) -> Option<RawFd> {
    (n >= 0.0 && n <= i32::MAX as f64 && n.fract() == 0.0).then_some(n as RawFd)
}

fn set_member(ctx: &mut Ctx, o: &Value, key: &str, value: Value) {
    let _ = ctx.set_member(o, key, value);
}

/// `(fd)` — take ownership of an inherited or received socket descriptor. Returns
/// `{ type: 'tcp' | 'pipe' | 'udp', server, ... }`: a connected stream adds `desc` (the
/// `register_stream` descriptor), a listener `serverId` and its address, a UDP socket `socketId`.
pub(super) fn adopt_fd(ctx: &mut Ctx, fd: f64) -> Result<Value, OpError> {
    let Some(fd) = fd_arg(fd) else {
        return Err(NativeError::type_error("adoptFd: fd must be a non-negative integer").into());
    };
    let sock_type = sockopt_int(fd, libc::SOL_SOCKET, libc::SO_TYPE)
        .map_err(|e| OpError::from(fd_error("open", &e)))?;
    let family = socket_family(fd).map_err(|e| OpError::from(fd_error("open", &e)))?;
    let listening = sock_type == libc::SOCK_STREAM
        && sockopt_int(fd, libc::SOL_SOCKET, libc::SO_ACCEPTCONN).unwrap_or(0) != 0;
    set_cloexec(fd);
    let o = Value::Obj(ctx.new_object());
    let kind = match family {
        libc::AF_UNIX => "pipe",
        libc::AF_INET | libc::AF_INET6 if sock_type == libc::SOCK_DGRAM => "udp",
        libc::AF_INET | libc::AF_INET6 if sock_type == libc::SOCK_STREAM => "tcp",
        _ => {
            let e = std::io::Error::from_raw_os_error(libc::EINVAL);
            return Err(fd_error("open", &e).into());
        }
    };
    set_member(ctx, &o, "type", Value::str(kind));
    set_member(ctx, &o, "server", Value::Bool(listening));
    if kind == "udp" {
        // SAFETY: the caller hands this descriptor over; it is a UDP socket (checked above).
        let socket = unsafe { UdpSocket::from_raw_fd(fd) };
        let reg = ctx
            .host_mut::<DgramRegistry>()
            .expect("dgram registry installed");
        let id = reg.next;
        reg.next += 1;
        reg.sockets.insert(
            id,
            UdpEntry {
                socket: Arc::new(socket),
                kind6: family == libc::AF_INET6,
                unref: false,
                pending: None,
                io: None,
            },
        );
        set_member(ctx, &o, "socketId", Value::Num(id as f64));
        return Ok(o);
    }
    if listening {
        let (listener, local_addr) = if kind == "tcp" {
            // SAFETY: a listening TCP socket handed over by the caller.
            let listener = unsafe { TcpListener::from_raw_fd(fd) };
            let addr = listener
                .local_addr()
                .map_err(|e| OpError::from(fd_error("open", &e)))?;
            (NetListener::Tcp(listener), ServerAddress::Tcp(addr))
        } else {
            // SAFETY: a listening Unix socket handed over by the caller.
            let listener = unsafe { UnixListener::from_raw_fd(fd) };
            let path = listener
                .local_addr()
                .ok()
                .and_then(|a| a.as_pathname().map(|p| p.to_string_lossy().into_owned()))
                .unwrap_or_default();
            (NetListener::Unix(listener), ServerAddress::Unix(path))
        };
        listener.set_nonblocking();
        let reg = ctx
            .host_mut::<NetRegistry>()
            .expect("net registry installed");
        let id = reg.next_server;
        reg.next_server += 1;
        reg.servers.insert(
            id,
            ServerEntry {
                listener: Arc::new(listener),
                closed: Arc::new(AtomicBool::new(false)),
                io: None,
                local_addr: local_addr.clone(),
                unref: false,
                pending: None,
                owns_path: false,
            },
        );
        set_member(ctx, &o, "serverId", Value::Num(id as f64));
        match local_addr {
            ServerAddress::Tcp(addr) => {
                set_member(
                    ctx,
                    &o,
                    "address",
                    Value::from_string(addr.ip().to_string()),
                );
                set_member(ctx, &o, "port", Value::Num(addr.port() as f64));
                set_member(ctx, &o, "family", Value::str(family_of(&addr)));
            }
            ServerAddress::Unix(path) => set_member(ctx, &o, "address", Value::from_string(path)),
        }
        return Ok(o);
    }
    let stream = if kind == "tcp" {
        // SAFETY: a connected TCP socket handed over by the caller.
        let stream = unsafe { TcpStream::from_raw_fd(fd) };
        let _ = stream.set_nonblocking(false);
        NetStream::Tcp(stream)
    } else {
        // SAFETY: a connected Unix socket handed over by the caller.
        let stream = unsafe { UnixStream::from_raw_fd(fd) };
        let _ = stream.set_nonblocking(false);
        NetStream::Unix(stream)
    };
    let desc = register_stream(ctx, stream)?;
    let array = ctx.make_array(desc);
    set_member(ctx, &o, "desc", array);
    Ok(o)
}

/// `(socketId)` — the descriptor of a connected socket, or -1.
pub(super) fn socket_fd(ctx: &mut Ctx, sid: u64) -> f64 {
    let fd = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| e.stream.raw_fd())
        .unwrap_or(-1);
    fd as f64
}

/// `(serverId)` — the descriptor of a listener, or -1.
pub(super) fn server_fd(ctx: &mut Ctx, sid: u64) -> f64 {
    let fd = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.servers.get(&sid))
        .map(|e| e.listener.raw_fd())
        .unwrap_or(-1);
    fd as f64
}

/// `(socketId)` — the descriptor of a UDP socket, or -1.
pub(super) fn udp_fd(ctx: &mut Ctx, sid: u64) -> f64 {
    let fd = ctx
        .host_mut::<DgramRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| e.socket.as_raw_fd())
        .unwrap_or(-1);
    fd as f64
}

/// `(socketId)` — close this process's copy of a socket without shutting the connection down
/// (it lives on in the process the descriptor was sent to): its registration and parked reader
/// are dropped and the descriptor closed once nothing else holds it.
pub(super) fn release(ctx: &mut Ctx, sid: u64) {
    let pending = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.sockets.remove(&sid))
        .and_then(|e| {
            if let Some(io) = &e.io {
                io.close(closed_write_error);
            }
            e.pending
        });
    if let (Some(id), Some(tasks)) = (pending, ctx.host_mut::<TaskRegistry>()) {
        tasks.take(id);
    }
}

type MsgResult = Result<(Vec<u8>, Vec<RawFd>), NetErr>;

fn recv_with_fds(fd: RawFd, buf: &mut [u8], fds: &mut Vec<RawFd>) -> std::io::Result<usize> {
    lumen_os::net::recv_fds(fd, buf, libc::MSG_DONTWAIT, MAX_FDS, fds).map_err(os_error)
}

/// `(socketId, resolve, reject)` — read from a channel socket, collecting any descriptors that
/// arrive with the bytes. Resolves `(bytes, fds)`, or `(null, [])` at EOF.
pub(super) fn read_msg(
    ctx: &mut Ctx,
    sid: u64,
    resolve: Value,
    reject: Value,
) -> Result<(), OpError> {
    let (resolve, reject) = take_resolve_reject(Some(&resolve), Some(&reject))?;
    let found = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| (e.stream.clone(), e.unref, e.io.clone()));
    let Some((stream, unref, io)) = found else {
        let empty = ctx.make_array(Vec::new());
        enqueue(ctx, resolve, vec![Value::Null, empty]);
        return Ok(());
    };
    let (io, created) = io_for(ctx, io, Source::Fd(stream.raw_fd()), stream.clone())?;
    let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_msg);
    if unref {
        ctx.host_mut::<TaskRegistry>()
            .expect("registry")
            .set_unref(id);
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
    io.submit(Dir::Read, id, true, msg_step(stream));
    Ok(())
}

/// A queued channel read: `recvmsg` into the `Vec` that becomes the JS `Uint8Array`.
fn msg_step(stream: Arc<NetStream>) -> impl FnMut(Trigger) -> Step + Send + 'static {
    move |trigger| {
        let result: MsgResult = match trigger {
            Trigger::Failed(e) => Err(net_err("read", &e, None)),
            Trigger::Ready => {
                let mut buf = vec![0u8; READ_CHUNK];
                let mut fds = Vec::new();
                loop {
                    match recv_with_fds(stream.raw_fd(), &mut buf, &mut fds) {
                        Ok(n) => {
                            buf.truncate(n);
                            break Ok((trimmed(buf), fds));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            return Step::Again
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(e) => break Err(net_err("read", &e, None)),
                    }
                }
            }
        };
        Step::Done(Box::new(result))
    }
}

fn decode_msg(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload.downcast::<MsgResult>().expect("readMsg payload") {
        Ok((bytes, fds)) => {
            let fds = ctx.make_array(fds.into_iter().map(|fd| Value::Num(fd as f64)).collect());
            if bytes.is_empty() {
                Ok(vec![Value::Null, fds])
            } else {
                Ok(vec![uint8array_owned(ctx, bytes)?, fds])
            }
        }
        Err(e) => Err(net_error_value(ctx, &e)),
    }
}

/// One `sendmsg`, with `pass` (when >= 0) attached as `SCM_RIGHTS`.
fn send_with_fd(fd: RawFd, data: &[u8], pass: RawFd, dontwait: bool) -> std::io::Result<usize> {
    let flags = if dontwait { libc::MSG_DONTWAIT } else { 0 };
    lumen_os::net::send_fd(fd, data, (pass >= 0).then_some(pass), flags).map_err(os_error)
}

fn pass_arg(fd: Option<f64>) -> RawFd {
    match fd {
        Some(n) if n >= 0.0 && n <= i32::MAX as f64 => n as RawFd,
        _ => -1,
    }
}

/// `(socketId, bytes, fd)` — `uv_try_write` for a channel: send what the socket takes right now,
/// with `fd` (-1 for none) riding on the first byte. Returns the byte count; the descriptor went
/// out iff it is positive.
pub(super) fn try_send_msg(ctx: &mut Ctx, sid: u64, data: &[u8], pass: Option<f64>) -> f64 {
    let stream = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| e.stream.clone());
    let Some(stream) = stream else { return 0.0 };
    let pass = pass_arg(pass);
    if data.is_empty() {
        return 0.0;
    }
    let fd = stream.raw_fd();
    let data = sendable(fd, data);
    if data.is_empty() {
        return 0.0;
    }
    send_with_fd(fd, data, pass, true).unwrap_or(0) as f64
}

/// `(socketId, bytes, fd, resolve, reject)` — write all bytes, `fd` (-1 for none) attached to the
/// first chunk; writes on one channel complete in order.
pub(super) fn write_msg(
    ctx: &mut Ctx,
    sid: u64,
    data: Vec<u8>,
    pass: Option<f64>,
    resolve: Value,
    reject: Value,
) -> Result<(), OpError> {
    let pass = pass_arg(pass);
    let (resolve, reject) = take_resolve_reject(Some(&resolve), Some(&reject))?;
    let found = ctx
        .host_mut::<NetRegistry>()
        .and_then(|r| r.sockets.get(&sid))
        .map(|e| (e.stream.clone(), e.io.clone()));
    let Some((stream, io)) = found else {
        let e = std::io::Error::from_raw_os_error(libc::EPIPE);
        let err = net_error_value(ctx, &net_err("write", &e, None));
        enqueue(ctx, reject, vec![err]);
        return Ok(());
    };
    let (io, created) = io_for(ctx, io, Source::Fd(stream.raw_fd()), stream.clone())?;
    let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_write);
    if created {
        if let Some(e) = ctx
            .host_mut::<NetRegistry>()
            .and_then(|r| r.sockets.get_mut(&sid))
        {
            e.io = Some(io.clone());
        }
    }
    io.submit(Dir::Write, id, true, write_msg_step(stream, data, pass));
    Ok(())
}

fn write_msg_step(
    stream: Arc<NetStream>,
    data: Vec<u8>,
    mut pass: RawFd,
) -> impl FnMut(Trigger) -> Step + Send + 'static {
    let mut offset = 0;
    move |trigger| {
        let fail = |e: &std::io::Error| Step::Done(Box::new(Err::<(), NetErr>(net_err("write", e, None))));
        if let Trigger::Failed(e) = &trigger {
            return fail(e);
        }
        let fd = stream.raw_fd();
        while offset < data.len() {
            let window = sendable(fd, &data[offset..]);
            if window.is_empty() {
                return Step::Again;
            }
            match send_with_fd(fd, window, pass, true) {
                Ok(n) => {
                    offset += n;
                    pass = -1;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Step::Again,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return fail(&e),
            }
        }
        Step::Done(Box::new(Ok::<(), NetErr>(())))
    }
}

/// `(fd)` — libuv's `uv_guess_handle`: "TCP", "TTY", "UDP", "FILE", "PIPE" or "UNKNOWN".
pub(super) fn guess_handle(fd: Option<f64>) -> &'static str {
    let Some(fd) = fd.and_then(fd_arg) else {
        return "UNKNOWN";
    };
    // SAFETY: isatty/fstat on an arbitrary descriptor number only report on it.
    if unsafe { libc::isatty(fd) } == 1 {
        return "TTY";
    }
    // SAFETY: a zeroed stat is a valid out-buffer.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::fstat(fd, &mut st) } < 0 {
        return "UNKNOWN";
    }
    match st.st_mode & libc::S_IFMT {
        libc::S_IFREG | libc::S_IFCHR => "FILE",
        libc::S_IFIFO => "PIPE",
        libc::S_IFSOCK => {
            let ty = sockopt_int(fd, libc::SOL_SOCKET, libc::SO_TYPE).unwrap_or(-1);
            match (ty, socket_family(fd).unwrap_or(-1)) {
                (libc::SOCK_STREAM, libc::AF_INET | libc::AF_INET6) => "TCP",
                (libc::SOCK_STREAM, libc::AF_UNIX) => "PIPE",
                (libc::SOCK_DGRAM, libc::AF_INET | libc::AF_INET6) => "UDP",
                _ => "UNKNOWN",
            }
        }
        _ => "UNKNOWN",
    }
}

/// `(fd)` — a close-on-exec duplicate, so a queued handle write keeps its descriptor even if the
/// handle is closed before the write goes out. -1 on failure.
pub(super) fn dup_fd(fd: Option<f64>) -> f64 {
    let fd = pass_arg(fd);
    if fd < 0 {
        return -1.0;
    }
    lumen_os::net::dup(fd).unwrap_or(-1) as f64
}

/// `(fd)` — close a raw descriptor this glue owns (a `dupFd` copy, an unclaimed received one).
pub(super) fn close_fd(fd: Option<f64>) {
    let fd = pass_arg(fd);
    if fd >= 0 {
        let _ = lumen_os::net::close(fd);
    }
}
