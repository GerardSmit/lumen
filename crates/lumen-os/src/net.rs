//! BSD sockets and name resolution: socket syscalls on raw descriptors, socket addresses,
//! `getaddrinfo`/`getnameinfo` and the service/protocol databases. Every socket made here is
//! close-on-exec.

use crate::errno::FsError;
use std::net::{SocketAddrV4, SocketAddrV6};

pub type R<T> = Result<T, FsError>;

/// Cancellation shared by blocking TCP clients. A cloned socket interrupts reads,
/// writes and TLS I/O without waiting for the client's ordinary timeout.
#[derive(Clone, Default)]
pub struct TcpCancellation(std::sync::Arc<std::sync::Mutex<TcpCancellationState>>);

#[derive(Default)]
struct TcpCancellationState {
    cancelled: bool,
    socket: Option<std::net::TcpStream>,
}

impl TcpCancellation {
    pub fn is_cancelled(&self) -> bool {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).cancelled
    }

    /// Attach before protocol I/O. Cancellation racing connection establishment
    /// closes the new socket rather than admitting a request after its abort.
    pub fn attach(&self, socket: &std::net::TcpStream) -> std::io::Result<()> {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        if state.cancelled {
            let _ = socket.shutdown(std::net::Shutdown::Both);
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        state.socket = Some(socket.try_clone()?);
        Ok(())
    }

    pub fn cancel(&self) {
        let mut state = self.0.lock().unwrap_or_else(|error| error.into_inner());
        state.cancelled = true;
        if let Some(socket) = state.socket.take() {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }

    /// Release the cancellation clone after protocol completion.
    pub fn detach(&self) {
        self.0.lock().unwrap_or_else(|error| error.into_inner()).socket = None;
    }
}

/// Resolve and connect within one deadline, observing cancellation during DNS
/// and connection establishment. A system resolver call cannot be interrupted,
/// but its detached result never admits a socket after the request is canceled.
pub fn connect_cancellable(
    host: &str,
    port: u16,
    timeout: std::time::Duration,
    cancellation: &TcpCancellation,
) -> std::io::Result<std::net::TcpStream> {
    use std::net::ToSocketAddrs;
    use std::time::{Duration, Instant};
    let interrupted = || std::io::Error::from(std::io::ErrorKind::Interrupted);
    if cancellation.is_cancelled() { return Err(interrupted()); }
    let deadline = Instant::now().checked_add(timeout)
        .ok_or_else(|| std::io::Error::other("connection timeout exceeds clock range"))?;
    let addresses = if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        vec![std::net::SocketAddr::new(ip, port)]
    } else {
    let host = host.to_owned();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new().name("lumen-resolver".into()).spawn(move || {
        let result = (host.as_str(), port).to_socket_addrs().map(|addresses| addresses.collect::<Vec<_>>());
        let _ = sender.send(result);
    })?;
    let addresses = loop {
        if cancellation.is_cancelled() { return Err(interrupted()); }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() { return Err(std::io::ErrorKind::TimedOut.into()); }
        match receiver.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(result) => break result?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {},
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err(std::io::Error::other("resolver stopped without a result")),
        }
    };
    addresses
    };
    if addresses.is_empty() { return Err(std::io::Error::other("DNS returned no addresses")); }
    let mut last_error = std::io::Error::from(std::io::ErrorKind::TimedOut);
    for address in &addresses {
            if cancellation.is_cancelled() { return Err(interrupted()); }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() { return Err(last_error); }
            match connect_socket_addr_cancellable(*address, remaining, cancellation) {
                Ok(stream) => { cancellation.attach(&stream)?; return Ok(stream); }
                Err(error) => {
                    if cancellation.is_cancelled() { return Err(interrupted()); }
                    last_error = error;
                }
            }
    }
    Err(last_error)
}

/// Connect to an already-resolved socket address with the same timeout and cancellation
/// behavior as [`connect_cancellable`]. Callers that route a logical host to a test or service
/// endpoint can keep URL/DNS identity separate from the transport address.
pub fn connect_socket_addr_cancellable(
    address: std::net::SocketAddr,
    timeout: std::time::Duration,
    cancellation: &TcpCancellation,
) -> std::io::Result<std::net::TcpStream> {
    if cancellation.is_cancelled() {
        return Err(std::io::ErrorKind::Interrupted.into());
    }
    connect_address_cancellable(&address, timeout, cancellation)
}

#[cfg(unix)]
fn connect_address_cancellable(address: &std::net::SocketAddr, timeout: std::time::Duration, cancellation: &TcpCancellation) -> std::io::Result<std::net::TcpStream> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let address = SockAddr::from(*address);
    let fd = socket(address.family(), libc::SOCK_STREAM, 0).map_err(std::io::Error::other)?;
    // SAFETY: `socket` just allocated this owned descriptor; this TcpStream is
    // its sole owner, including on every failure/cancellation path below.
    let stream = unsafe { std::net::TcpStream::from_raw_fd(fd) };
    stream.set_nonblocking(true)?;
    cancellation.attach(&stream)?;
    match connect(fd, &address) {
        Ok(()) => {},
        Err(error) if matches!(error.0, "EINPROGRESS" | "EALREADY" | "EWOULDBLOCK" | "EAGAIN") => {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                if cancellation.is_cancelled() { return Err(std::io::ErrorKind::Interrupted.into()); }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() { return Err(std::io::ErrorKind::TimedOut.into()); }
                let mut fds = [crate::poll::PollFd::new(stream.as_raw_fd(), crate::poll::POLLOUT)];
                match crate::poll::poll(&mut fds, remaining.as_millis().clamp(1, 50) as i32) {
                    Ok(0) => continue,
                    Ok(_) => { if let Some(error) = stream.take_error()? { return Err(error); } break; }
                    Err(error) if error.0 == "EINTR" => continue,
                    Err(error) => return Err(std::io::Error::other(error)),
                }
            }
        }
        Err(error) => return Err(std::io::Error::other(error)),
    }
    if cancellation.is_cancelled() { return Err(std::io::ErrorKind::Interrupted.into()); }
    stream.set_nonblocking(false)?;
    Ok(stream)
}

#[cfg(not(unix))]
fn connect_address_cancellable(address: &std::net::SocketAddr, timeout: std::time::Duration, cancellation: &TcpCancellation) -> std::io::Result<std::net::TcpStream> {
    let address = *address;
    let worker_cancellation = cancellation.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::Builder::new().name("lumen-connect".into()).spawn(move || {
        let result = std::net::TcpStream::connect_timeout(&address, timeout).and_then(|stream| {
            worker_cancellation.attach(&stream)?;
            Ok(stream)
        });
        let _ = sender.send(result);
    })?;
    loop {
        if cancellation.is_cancelled() { return Err(std::io::ErrorKind::Interrupted.into()); }
        match receiver.recv_timeout(std::time::Duration::from_millis(50)) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {},
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err(std::io::Error::other("connection worker stopped")),
        }
    }
}

/// A socket address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SockAddr {
    V4(SocketAddrV4),
    V6(SocketAddrV6),
    /// A Unix-domain path (Linux: a leading NUL marks the abstract namespace); empty when unnamed.
    Unix(Vec<u8>),
    /// An address of another family, reported by its family number only.
    Other(i32),
}

impl From<std::net::SocketAddr> for SockAddr {
    fn from(addr: std::net::SocketAddr) -> SockAddr {
        match addr {
            std::net::SocketAddr::V4(a) => SockAddr::V4(a),
            std::net::SocketAddr::V6(a) => SockAddr::V6(a),
        }
    }
}

/// One `getaddrinfo` result.
#[derive(Clone, Debug)]
pub struct AddrInfo {
    pub family: i32,
    pub socktype: i32,
    pub proto: i32,
    pub canonname: String,
    pub addr: SockAddr,
}

/// A `getaddrinfo`/`getnameinfo` failure: the `EAI_*` code, plus `errno` for `EAI_SYSTEM`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaiError {
    pub code: i32,
    pub errno: i32,
}

#[cfg(unix)]
mod imp {
    use super::*;
    use std::ffi::{CStr, CString};
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn check(rc: libc::c_int) -> R<libc::c_int> {
        if rc < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(rc)
        }
    }

    fn check_size(rc: isize) -> R<usize> {
        if rc < 0 {
            Err(std::io::Error::last_os_error().into())
        } else {
            Ok(rc as usize)
        }
    }

    fn cloexec(fd: i32) -> R<i32> {
        if let Err(e) = crate::fdctl::set_inheritable(fd, false) {
            // SAFETY: closing the descriptor this module just created.
            unsafe { libc::close(fd) };
            return Err(e);
        }
        Ok(fd)
    }

    impl SockAddr {
        pub fn family(&self) -> i32 {
            match self {
                SockAddr::V4(_) => libc::AF_INET,
                SockAddr::V6(_) => libc::AF_INET6,
                SockAddr::Unix(_) => libc::AF_UNIX,
                SockAddr::Other(f) => *f,
            }
        }

        /// The C form; `EINVAL` for a Unix path that does not fit `sun_path`.
        pub fn to_raw(&self) -> R<(libc::sockaddr_storage, libc::socklen_t)> {
            // SAFETY: an all-zero sockaddr_storage is valid and every family struct written into
            // it below is no larger than the storage.
            let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
            let at = (&mut storage as *mut libc::sockaddr_storage).cast::<u8>();
            let len = match self {
                SockAddr::V4(a) => {
                    let sin = libc::sockaddr_in {
                        #[cfg(target_vendor = "apple")]
                        sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
                        sin_family: libc::AF_INET as libc::sa_family_t,
                        sin_port: a.port().to_be(),
                        sin_addr: libc::in_addr {
                            s_addr: u32::from_ne_bytes(a.ip().octets()),
                        },
                        sin_zero: [0; 8],
                    };
                    // SAFETY: see above.
                    unsafe { std::ptr::write(at.cast(), sin) };
                    std::mem::size_of::<libc::sockaddr_in>()
                }
                SockAddr::V6(a) => {
                    let sin6 = libc::sockaddr_in6 {
                        #[cfg(target_vendor = "apple")]
                        sin6_len: std::mem::size_of::<libc::sockaddr_in6>() as u8,
                        sin6_family: libc::AF_INET6 as libc::sa_family_t,
                        sin6_port: a.port().to_be(),
                        sin6_flowinfo: a.flowinfo(),
                        sin6_addr: libc::in6_addr {
                            s6_addr: a.ip().octets(),
                        },
                        sin6_scope_id: a.scope_id(),
                    };
                    // SAFETY: see above.
                    unsafe { std::ptr::write(at.cast(), sin6) };
                    std::mem::size_of::<libc::sockaddr_in6>()
                }
                SockAddr::Unix(path) => {
                    // SAFETY: see above.
                    let sun = unsafe { &mut *at.cast::<libc::sockaddr_un>() };
                    if path.len() >= sun.sun_path.len() {
                        return Err(FsError("EINVAL"));
                    }
                    sun.sun_family = libc::AF_UNIX as libc::sa_family_t;
                    for (d, &b) in sun.sun_path.iter_mut().zip(path) {
                        *d = b as libc::c_char;
                    }
                    let base = std::mem::offset_of!(libc::sockaddr_un, sun_path);
                    // Linux: an empty path autobinds and a leading NUL names the abstract
                    // namespace; neither carries a terminating NUL.
                    let abstract_name = cfg!(any(target_os = "linux", target_os = "android"))
                        && path.first().is_none_or(|&b| b == 0);
                    let len = base + path.len() + usize::from(!abstract_name);
                    #[cfg(target_vendor = "apple")]
                    {
                        sun.sun_len = len as u8;
                    }
                    len
                }
                SockAddr::Other(_) => return Err(FsError("EAFNOSUPPORT")),
            };
            Ok((storage, len as libc::socklen_t))
        }

        /// Reads the C form the kernel wrote (`len` bytes).
        pub fn from_raw(storage: &libc::sockaddr_storage, len: libc::socklen_t) -> SockAddr {
            let at = (storage as *const libc::sockaddr_storage).cast::<u8>();
            match storage.ss_family as i32 {
                libc::AF_INET => {
                    // SAFETY: the family says the storage holds a sockaddr_in.
                    let sin = unsafe { &*at.cast::<libc::sockaddr_in>() };
                    let ip = Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes());
                    SockAddr::V4(SocketAddrV4::new(ip, u16::from_be(sin.sin_port)))
                }
                libc::AF_INET6 => {
                    // SAFETY: the family says the storage holds a sockaddr_in6.
                    let sin6 = unsafe { &*at.cast::<libc::sockaddr_in6>() };
                    let ip = Ipv6Addr::from(sin6.sin6_addr.s6_addr);
                    SockAddr::V6(SocketAddrV6::new(
                        ip,
                        u16::from_be(sin6.sin6_port),
                        sin6.sin6_flowinfo,
                        sin6.sin6_scope_id,
                    ))
                }
                libc::AF_UNIX => {
                    // SAFETY: the family says the storage holds a sockaddr_un.
                    let sun = unsafe { &*at.cast::<libc::sockaddr_un>() };
                    let base = std::mem::offset_of!(libc::sockaddr_un, sun_path);
                    let n = (len as usize).saturating_sub(base).min(sun.sun_path.len());
                    let raw: Vec<u8> = sun.sun_path[..n].iter().map(|&c| c as u8).collect();
                    let path = if raw.first() == Some(&0) {
                        raw
                    } else {
                        raw.iter()
                            .position(|&b| b == 0)
                            .map_or(raw.clone(), |end| raw[..end].to_vec())
                    };
                    SockAddr::Unix(path)
                }
                f => SockAddr::Other(f),
            }
        }
    }

    pub fn socket(family: i32, socktype: i32, proto: i32) -> R<i32> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: plain syscall.
        return check(unsafe { libc::socket(family, socktype | libc::SOCK_CLOEXEC, proto) });
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        // SAFETY: plain syscall.
        cloexec(check(unsafe { libc::socket(family, socktype, proto) })?)
    }

    pub fn socketpair(family: i32, socktype: i32, proto: i32) -> R<(i32, i32)> {
        let mut fds = [0 as libc::c_int; 2];
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let socktype = socktype | libc::SOCK_CLOEXEC;
        // SAFETY: `fds` has room for both descriptors.
        check(unsafe { libc::socketpair(family, socktype, proto, fds.as_mut_ptr()) })?;
        if let Err(e) = crate::fdctl::set_inheritable(fds[0], false)
            .and(crate::fdctl::set_inheritable(fds[1], false))
        {
            // SAFETY: closing the pair just created.
            unsafe {
                libc::close(fds[0]);
                libc::close(fds[1]);
            }
            return Err(e);
        }
        Ok((fds[0], fds[1]))
    }

    /// The address family, type and protocol of an existing socket.
    pub fn socket_info(fd: i32) -> R<(i32, i32, i32)> {
        let socktype = getsockopt_int(fd, libc::SOL_SOCKET, libc::SO_TYPE)?;
        let family = getsockname(fd).map(|a| a.family())?;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let proto = getsockopt_int(fd, libc::SOL_SOCKET, libc::SO_PROTOCOL).unwrap_or(0);
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let proto = 0;
        Ok((family, socktype, proto))
    }

    pub fn bind(fd: i32, addr: &SockAddr) -> R<()> {
        let (s, len) = addr.to_raw()?;
        // SAFETY: `s` holds a valid sockaddr of `len` bytes.
        check(unsafe { libc::bind(fd, (&s as *const libc::sockaddr_storage).cast(), len) })
            .map(drop)
    }

    /// `connect(2)`; a non-blocking socket reports `EINPROGRESS` as an error.
    pub fn connect(fd: i32, addr: &SockAddr) -> R<()> {
        let (s, len) = addr.to_raw()?;
        // SAFETY: `s` holds a valid sockaddr of `len` bytes.
        check(unsafe { libc::connect(fd, (&s as *const libc::sockaddr_storage).cast(), len) })
            .map(drop)
    }

    /// Dissolve a datagram socket's peer (`connect` to an `AF_UNSPEC` address).
    pub fn disconnect(fd: i32) -> R<()> {
        // SAFETY: an all-zero sockaddr_storage with family AF_UNSPEC is the documented way to
        // dissolve a datagram socket's peer.
        let mut s: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        s.ss_family = libc::AF_UNSPEC as libc::sa_family_t;
        #[cfg(target_vendor = "apple")]
        {
            s.ss_len = std::mem::size_of::<libc::sockaddr>() as u8;
        }
        let len = std::mem::size_of::<libc::sockaddr>() as libc::socklen_t;
        // SAFETY: `s` is a live sockaddr of the length passed.
        match check(unsafe { libc::connect(fd, (&s as *const libc::sockaddr_storage).cast(), len) })
        {
            Ok(_) => Ok(()),
            // Darwin reports the dissolved association as EAFNOSUPPORT on some releases while
            // still having applied it.
            Err(e) if e.code() == "EAFNOSUPPORT" && getpeername(fd).is_err() => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn listen(fd: i32, backlog: i32) -> R<()> {
        // SAFETY: plain syscall.
        check(unsafe { libc::listen(fd, backlog) }).map(drop)
    }

    pub fn accept(fd: i32) -> R<(i32, SockAddr)> {
        // SAFETY: zeroed storage is valid; the kernel writes at most `len` bytes.
        let mut s: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let sp = (&mut s as *mut libc::sockaddr_storage).cast();
        #[cfg(any(target_os = "linux", target_os = "android"))]
        // SAFETY: as above.
        let nfd = check(unsafe { libc::accept4(fd, sp, &mut len, libc::SOCK_CLOEXEC) })?;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        // SAFETY: as above.
        let nfd = cloexec(check(unsafe { libc::accept(fd, sp, &mut len) })?)?;
        Ok((nfd, SockAddr::from_raw(&s, len)))
    }

    pub fn shutdown(fd: i32, how: i32) -> R<()> {
        // SAFETY: plain syscall.
        check(unsafe { libc::shutdown(fd, how) }).map(drop)
    }

    fn name_of(fd: i32, peer: bool) -> R<SockAddr> {
        // SAFETY: zeroed storage is valid; the kernel writes at most `len` bytes.
        let mut s: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let sp = (&mut s as *mut libc::sockaddr_storage).cast();
        // SAFETY: as above.
        check(unsafe {
            if peer {
                libc::getpeername(fd, sp, &mut len)
            } else {
                libc::getsockname(fd, sp, &mut len)
            }
        })?;
        Ok(SockAddr::from_raw(&s, len))
    }

    pub fn getsockname(fd: i32) -> R<SockAddr> {
        name_of(fd, false)
    }

    pub fn getpeername(fd: i32) -> R<SockAddr> {
        name_of(fd, true)
    }

    pub fn send(fd: i32, buf: &[u8], flags: i32) -> R<usize> {
        // SAFETY: `buf` is a live slice.
        check_size(unsafe { libc::send(fd, buf.as_ptr().cast(), buf.len(), flags) })
    }

    pub fn sendto(fd: i32, buf: &[u8], flags: i32, addr: &SockAddr) -> R<usize> {
        let (s, len) = addr.to_raw()?;
        // SAFETY: `buf` is a live slice and `s` a valid sockaddr of `len` bytes.
        check_size(unsafe {
            libc::sendto(
                fd,
                buf.as_ptr().cast(),
                buf.len(),
                flags,
                (&s as *const libc::sockaddr_storage).cast(),
                len,
            )
        })
    }

    pub fn recv(fd: i32, buf: &mut [u8], flags: i32) -> R<usize> {
        // SAFETY: `buf` is a live, writable slice.
        check_size(unsafe { libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), flags) })
    }

    /// `recvmsg(2)` of up to `buf.len()` bytes, appending the descriptors that arrive as
    /// `SCM_RIGHTS` (at most `max_fds`, each made close-on-exec) to `fds`.
    pub fn recv_fds(
        fd: i32,
        buf: &mut [u8],
        flags: i32,
        max_fds: usize,
        fds: &mut Vec<i32>,
    ) -> R<usize> {
        let mut iov = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        // SAFETY: CMSG_SPACE is a pure size computation.
        let space =
            unsafe { libc::CMSG_SPACE((max_fds * std::mem::size_of::<i32>()) as u32) } as usize;
        let mut control = vec![0u8; space];
        // SAFETY: a zeroed msghdr is valid; the pointers set below outlive the call.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let flags = flags | libc::MSG_CMSG_CLOEXEC;
        // SAFETY: `msg` describes live buffers.
        let n = check_size(unsafe { libc::recvmsg(fd, &mut msg, flags) })?;
        // SAFETY: walking the control buffer recvmsg just filled, with the libc CMSG helpers.
        unsafe {
            let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
            while !cmsg.is_null() {
                if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(cmsg) as *const i32;
                    let header = libc::CMSG_LEN(0) as usize;
                    let count = ((*cmsg).cmsg_len as usize).saturating_sub(header)
                        / std::mem::size_of::<i32>();
                    for i in 0..count {
                        let received = std::ptr::read_unaligned(data.add(i));
                        let _ = crate::fdctl::set_inheritable(received, false);
                        fds.push(received);
                    }
                }
                cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
            }
        }
        Ok(n)
    }

    /// `sendmsg(2)` of `data`, with `pass` attached as `SCM_RIGHTS`; never raises `SIGPIPE`
    /// where the platform has `MSG_NOSIGNAL`.
    pub fn send_fd(fd: i32, data: &[u8], pass: Option<i32>, flags: i32) -> R<usize> {
        let mut iov = libc::iovec {
            iov_base: data.as_ptr() as *mut _,
            iov_len: data.len(),
        };
        // SAFETY: CMSG_SPACE is a pure size computation.
        let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
        let mut control = vec![0u8; space];
        // SAFETY: a zeroed msghdr is valid; the pointers set below outlive the call.
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        if let Some(pass) = pass {
            msg.msg_control = control.as_mut_ptr().cast();
            msg.msg_controllen = space as _;
            // SAFETY: the control buffer has room for one header carrying one descriptor.
            unsafe {
                let cmsg = libc::CMSG_FIRSTHDR(&msg);
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as _;
                std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut i32, pass);
            }
        }
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let flags = flags | libc::MSG_NOSIGNAL;
        // SAFETY: `msg` describes live buffers.
        check_size(unsafe { libc::sendmsg(fd, &msg, flags) })
    }

    /// `recvfrom(2)`; the sender is `None` when the kernel reports no address (a connected
    /// stream socket).
    pub fn recvfrom(fd: i32, buf: &mut [u8], flags: i32) -> R<(usize, Option<SockAddr>)> {
        // SAFETY: zeroed storage is valid; the kernel writes at most `len` bytes.
        let mut s: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        // SAFETY: `buf` is a live, writable slice; the address out-parameters are valid.
        let n = check_size(unsafe {
            libc::recvfrom(
                fd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                flags,
                (&mut s as *mut libc::sockaddr_storage).cast(),
                &mut len,
            )
        })?;
        Ok((n, (len > 0).then(|| SockAddr::from_raw(&s, len))))
    }

    pub fn getsockopt(fd: i32, level: i32, opt: i32, buflen: usize) -> R<Vec<u8>> {
        let mut buf = vec![0u8; buflen];
        let mut len = buflen as libc::socklen_t;
        // SAFETY: `buf` has `len` writable bytes.
        check(unsafe { libc::getsockopt(fd, level, opt, buf.as_mut_ptr().cast(), &mut len) })?;
        buf.truncate(len as usize);
        Ok(buf)
    }

    pub fn getsockopt_int(fd: i32, level: i32, opt: i32) -> R<i32> {
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: `v` is a writable int of `len` bytes.
        check(unsafe {
            libc::getsockopt(
                fd,
                level,
                opt,
                (&mut v as *mut libc::c_int).cast(),
                &mut len,
            )
        })?;
        Ok(v)
    }

    pub fn setsockopt(fd: i32, level: i32, opt: i32, value: &[u8]) -> R<()> {
        // SAFETY: `value` is a live slice of the given length.
        check(unsafe {
            libc::setsockopt(
                fd,
                level,
                opt,
                value.as_ptr().cast(),
                value.len() as libc::socklen_t,
            )
        })
        .map(drop)
    }

    pub fn setsockopt_int(fd: i32, level: i32, opt: i32, value: i32) -> R<()> {
        setsockopt(fd, level, opt, &value.to_ne_bytes())
    }

    /// `setsockopt` with a NULL value of `optlen` bytes.
    pub fn setsockopt_null(fd: i32, level: i32, opt: i32, optlen: u32) -> R<()> {
        // SAFETY: a NULL option value is passed through for the kernel to validate.
        check(unsafe {
            libc::setsockopt(fd, level, opt, std::ptr::null(), optlen as libc::socklen_t)
        })
        .map(drop)
    }

    pub fn close(fd: i32) -> R<()> {
        // SAFETY: plain syscall.
        check(unsafe { libc::close(fd) }).map(drop)
    }

    /// A close-on-exec duplicate of `fd`.
    pub fn dup(fd: i32) -> R<i32> {
        // SAFETY: plain syscall.
        check(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) })
    }

    pub fn gethostname() -> R<String> {
        let mut buf = [0 as libc::c_char; 1024];
        // SAFETY: `buf` has the declared length.
        check(unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) })?;
        // SAFETY: gethostname NUL-terminates within the buffer.
        Ok(unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned())
    }

    fn cstr_opt(s: Option<&str>) -> Result<Option<CString>, GaiError> {
        s.map(|s| {
            CString::new(s).map_err(|_| GaiError {
                code: libc::EAI_NONAME,
                errno: 0,
            })
        })
        .transpose()
    }

    pub fn getaddrinfo(
        host: Option<&str>,
        port: Option<&str>,
        family: i32,
        socktype: i32,
        proto: i32,
        flags: i32,
    ) -> Result<Vec<AddrInfo>, GaiError> {
        let host = cstr_opt(host)?;
        let port = cstr_opt(port)?;
        // SAFETY: an all-zero addrinfo is the documented "no hints" starting point.
        let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
        hints.ai_flags = flags;
        hints.ai_family = family;
        hints.ai_socktype = socktype;
        hints.ai_protocol = proto;
        let mut res: *mut libc::addrinfo = std::ptr::null_mut();
        let hp = host.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        let pp = port.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());
        // SAFETY: the strings are NUL-terminated or NULL, `hints` is initialised and `res`
        // receives the list.
        let rc = unsafe { libc::getaddrinfo(hp, pp, &hints, &mut res) };
        if rc != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return Err(GaiError { code: rc, errno });
        }
        let mut out = Vec::new();
        let mut cur = res;
        while !cur.is_null() {
            // SAFETY: `cur` walks the list getaddrinfo returned, alive until freeaddrinfo.
            let info = unsafe { &*cur };
            let addr = if info.ai_addr.is_null() {
                SockAddr::Other(info.ai_family)
            } else {
                // SAFETY: zeroed storage is valid and `ai_addrlen` bytes fit in it.
                let mut s: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
                let n =
                    (info.ai_addrlen as usize).min(std::mem::size_of::<libc::sockaddr_storage>());
                // SAFETY: copying `n` bytes from the resolver's sockaddr into the storage.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        info.ai_addr.cast::<u8>(),
                        (&mut s as *mut libc::sockaddr_storage).cast::<u8>(),
                        n,
                    )
                };
                SockAddr::from_raw(&s, n as libc::socklen_t)
            };
            let canonname = if info.ai_canonname.is_null() {
                String::new()
            } else {
                // SAFETY: a NUL-terminated string owned by the list.
                unsafe { CStr::from_ptr(info.ai_canonname) }
                    .to_string_lossy()
                    .into_owned()
            };
            out.push(AddrInfo {
                family: info.ai_family,
                socktype: info.ai_socktype,
                proto: info.ai_protocol,
                canonname,
                addr,
            });
            cur = info.ai_next;
        }
        // SAFETY: `res` came from a successful getaddrinfo.
        unsafe { libc::freeaddrinfo(res) };
        Ok(out)
    }

    pub fn getnameinfo(addr: &SockAddr, flags: i32) -> Result<(String, String), GaiError> {
        let (s, len) = addr.to_raw().map_err(|_| GaiError {
            code: libc::EAI_FAMILY,
            errno: 0,
        })?;
        let mut host = [0 as libc::c_char; 1025];
        let mut serv = [0 as libc::c_char; 32];
        // SAFETY: `s` holds a valid sockaddr of `len` bytes; the output buffers are sized as
        // declared.
        let rc = unsafe {
            libc::getnameinfo(
                (&s as *const libc::sockaddr_storage).cast(),
                len,
                host.as_mut_ptr(),
                host.len() as _,
                serv.as_mut_ptr(),
                serv.len() as _,
                flags,
            )
        };
        if rc != 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            return Err(GaiError { code: rc, errno });
        }
        // SAFETY: getnameinfo wrote NUL-terminated strings.
        let text = |buf: &[libc::c_char]| {
            unsafe { CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        };
        Ok((text(&host), text(&serv)))
    }

    /// The message of an `EAI_*` code.
    pub fn gai_strerror(code: i32) -> String {
        // SAFETY: gai_strerror returns a static NUL-terminated string.
        unsafe { CStr::from_ptr(libc::gai_strerror(code)) }
            .to_string_lossy()
            .into_owned()
    }

    /// The libuv-style name of an `EAI_*` code (`"EAI_NONAME"`).
    pub fn gai_code_name(code: i32) -> &'static str {
        match code {
            libc::EAI_AGAIN => "EAI_AGAIN",
            libc::EAI_BADFLAGS => "EAI_BADFLAGS",
            libc::EAI_FAMILY => "EAI_FAMILY",
            libc::EAI_MEMORY => "EAI_MEMORY",
            libc::EAI_NONAME => "EAI_NONAME",
            libc::EAI_SERVICE => "EAI_SERVICE",
            libc::EAI_SOCKTYPE => "EAI_SOCKTYPE",
            _ => "EAI_FAIL",
        }
    }

    pub fn getservbyname(name: &str, proto: Option<&str>) -> Option<u16> {
        let name = CString::new(name).ok()?;
        let proto = proto.map(CString::new).transpose().ok()?;
        // SAFETY: NUL-terminated arguments; the result points into libc's static storage and is
        // read before any other database call.
        let ent = unsafe {
            libc::getservbyname(
                name.as_ptr(),
                proto.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            )
        };
        // SAFETY: as above.
        (!ent.is_null()).then(|| u16::from_be(unsafe { (*ent).s_port } as u16))
    }

    pub fn getservbyport(port: u16, proto: Option<&str>) -> Option<String> {
        let proto = proto.map(CString::new).transpose().ok()?;
        // SAFETY: as in `getservbyname`.
        let ent = unsafe {
            libc::getservbyport(
                i32::from(port.to_be()),
                proto.as_ref().map_or(std::ptr::null(), |p| p.as_ptr()),
            )
        };
        // SAFETY: as above.
        (!ent.is_null()).then(|| {
            unsafe { CStr::from_ptr((*ent).s_name) }
                .to_string_lossy()
                .into_owned()
        })
    }

    pub fn getprotobyname(name: &str) -> Option<i32> {
        let name = CString::new(name).ok()?;
        // SAFETY: as in `getservbyname`.
        let ent = unsafe { libc::getprotobyname(name.as_ptr()) };
        // SAFETY: as above.
        (!ent.is_null()).then(|| unsafe { (*ent).p_proto })
    }

    /// `inet_aton(3)` (accepts the short forms such as `"127.1"`).
    pub fn aton(s: &str) -> Option<[u8; 4]> {
        let c = CString::new(s).ok()?;
        let mut a = libc::in_addr { s_addr: 0 };
        // SAFETY: NUL-terminated input and a valid out-parameter.
        (unsafe { inet_aton(c.as_ptr(), &mut a) } != 0).then(|| a.s_addr.to_ne_bytes())
    }

    /// `inet_pton(3)` for `AF_INET` (4 bytes) or `AF_INET6` (16 bytes); `None` when invalid.
    pub fn pton(family: i32, s: &str) -> R<Option<Vec<u8>>> {
        let c = CString::new(s).map_err(|_| FsError("EINVAL"))?;
        let mut buf = [0u8; 16];
        // SAFETY: NUL-terminated input; `buf` is large enough for either family.
        let rc = unsafe { inet_pton(family, c.as_ptr(), buf.as_mut_ptr().cast()) };
        match rc {
            1 => Ok(Some(
                buf[..if family == libc::AF_INET { 4 } else { 16 }].to_vec(),
            )),
            0 => Ok(None),
            _ => Err(std::io::Error::last_os_error().into()),
        }
    }

    /// `inet_ntop(3)` of a 4- or 16-byte address.
    pub fn ntop(family: i32, packed: &[u8]) -> R<String> {
        let mut buf = [0 as libc::c_char; 64];
        // SAFETY: `packed` is the address of `family` (checked by the caller's length), `buf`
        // has the declared size.
        let p = unsafe {
            inet_ntop(
                family,
                packed.as_ptr().cast(),
                buf.as_mut_ptr(),
                buf.len() as libc::socklen_t,
            )
        };
        if p.is_null() {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: inet_ntop NUL-terminated the buffer.
        Ok(unsafe { CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned())
    }

    extern "C" {
        fn inet_aton(cp: *const libc::c_char, inp: *mut libc::in_addr) -> libc::c_int;
        fn inet_pton(
            af: libc::c_int,
            src: *const libc::c_char,
            dst: *mut libc::c_void,
        ) -> libc::c_int;
        fn inet_ntop(
            af: libc::c_int,
            src: *const libc::c_void,
            dst: *mut libc::c_char,
            size: libc::socklen_t,
        ) -> *const libc::c_char;
    }

    pub const AF_UNSPEC: i32 = libc::AF_UNSPEC;
    pub const AF_INET: i32 = libc::AF_INET;
    pub const AF_INET6: i32 = libc::AF_INET6;
    pub const AF_UNIX: i32 = libc::AF_UNIX;
    pub const SOCK_STREAM: i32 = libc::SOCK_STREAM;
    pub const SOCK_DGRAM: i32 = libc::SOCK_DGRAM;
    pub const SOMAXCONN: i32 = libc::SOMAXCONN;
    pub const AI_PASSIVE: i32 = libc::AI_PASSIVE;
    pub const AI_CANONNAME: i32 = libc::AI_CANONNAME;
    pub const AI_NUMERICHOST: i32 = libc::AI_NUMERICHOST;
    pub const NI_NAMEREQD: i32 = libc::NI_NAMEREQD;
    pub const EAI_SYSTEM: i32 = libc::EAI_SYSTEM;
    /// `send` flag suppressing `SIGPIPE` where the platform has one (0 elsewhere).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub const MSG_NOSIGNAL: i32 = libc::MSG_NOSIGNAL;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub const MSG_NOSIGNAL: i32 = 0;
    /// Type flags `socket(2)` accepts on Linux (0 elsewhere).
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub const SOCK_NONBLOCK: i32 = libc::SOCK_NONBLOCK;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub const SOCK_NONBLOCK: i32 = 0;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub const SOCK_CLOEXEC: i32 = libc::SOCK_CLOEXEC;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    pub const SOCK_CLOEXEC: i32 = 0;
    /// The longest Unix-domain path (excluding the terminating NUL).
    pub const UNIX_PATH_MAX: usize = {
        // SAFETY: only the size of a field of a zeroed value is taken.
        let sun: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        sun.sun_path.len()
    };

    /// Whether `errno` means "would block" (`EAGAIN`/`EWOULDBLOCK`).
    pub fn would_block(errno: i32) -> bool {
        errno == libc::EAGAIN || errno == libc::EWOULDBLOCK
    }

    /// Whether `errno` from `connect` means the connection continues asynchronously.
    pub fn in_progress(errno: i32) -> bool {
        errno == libc::EINPROGRESS
    }

    /// The pending error of a socket (`SO_ERROR`), cleared by reading it; `EISCONN` counts as
    /// success.
    pub fn take_error(fd: i32) -> R<i32> {
        let e = getsockopt_int(fd, libc::SOL_SOCKET, libc::SO_ERROR)?;
        Ok(if e == libc::EISCONN { 0 } else { e })
    }

    // `NAME = value` covers constants the libc crate leaves out on some platforms.
    macro_rules! const_table {
        ($($(#[$m:meta])* $name:ident $(= $value:expr)?),* $(,)?) => {
            &[$($(#[$m])* (stringify!($name), const_value!($name $(, $value)?)),)*]
        };
    }
    macro_rules! const_value {
        ($name:ident) => {
            libc::$name as i64
        };
        ($name:ident, $value:expr) => {
            $value as i64
        };
    }

    /// The socket constants of this platform by C name.
    pub fn constants() -> &'static [(&'static str, i64)] {
        const_table!(
            AF_UNSPEC,
            AF_INET,
            AF_INET6,
            AF_UNIX,
            AF_ROUTE,
            AF_APPLETALK,
            AF_SNA,
            AF_DECnet,
            AF_IPX,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            AF_NETLINK,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            AF_PACKET,
            #[cfg(target_vendor = "apple")]
            AF_SYSTEM,
            SOCK_STREAM,
            SOCK_DGRAM,
            SOCK_RAW,
            SOCK_SEQPACKET,
            SOCK_RDM,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            SOCK_NONBLOCK,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            SOCK_CLOEXEC,
            SOL_SOCKET,
            SO_DEBUG,
            SO_ACCEPTCONN,
            SO_REUSEADDR,
            SO_REUSEPORT,
            SO_KEEPALIVE,
            SO_DONTROUTE,
            SO_BROADCAST,
            SO_LINGER,
            SO_OOBINLINE,
            SO_SNDBUF,
            SO_RCVBUF,
            SO_SNDLOWAT,
            SO_RCVLOWAT,
            SO_SNDTIMEO,
            SO_RCVTIMEO,
            SO_ERROR,
            SO_TYPE,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            SO_PROTOCOL,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            SO_DOMAIN,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            SO_PASSCRED,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            SO_PEERCRED,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            SO_BINDTODEVICE,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            SO_PRIORITY,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            SO_MARK,
            #[cfg(target_vendor = "apple")]
            SO_USELOOPBACK,
            SOMAXCONN,
            SCM_RIGHTS,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            SCM_CREDENTIALS,
            MSG_OOB,
            MSG_PEEK,
            MSG_DONTROUTE,
            MSG_DONTWAIT,
            MSG_EOR,
            MSG_TRUNC,
            MSG_CTRUNC,
            MSG_WAITALL,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            MSG_NOSIGNAL,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            MSG_CMSG_CLOEXEC,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            MSG_CONFIRM,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            MSG_ERRQUEUE,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            MSG_MORE,
            #[cfg(target_vendor = "apple")]
            MSG_EOF,
            SHUT_RD,
            SHUT_WR,
            SHUT_RDWR,
            IPPROTO_IP,
            IPPROTO_ICMP,
            IPPROTO_IGMP,
            IPPROTO_TCP,
            IPPROTO_UDP,
            IPPROTO_IPV6,
            IPPROTO_RAW,
            IPPROTO_ICMPV6,
            IPPROTO_SCTP,
            IPPROTO_GRE,
            IPPROTO_ESP,
            IPPROTO_AH,
            IPPROTO_PIM,
            IPPROTO_HOPOPTS,
            IPPROTO_ROUTING,
            IPPROTO_FRAGMENT,
            IPPROTO_NONE,
            IPPROTO_DSTOPTS,
            IPPROTO_EGP,
            IPPROTO_PUP,
            IPPROTO_IDP,
            IPPROTO_TP,
            IPPROTO_RSVP,
            IPPROTO_IPIP,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            IPPROTO_UDPLITE,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            IPPROTO_MPTCP,
            #[cfg(target_vendor = "apple")]
            IPPROTO_GGP,
            #[cfg(target_vendor = "apple")]
            IPPROTO_HELLO,
            #[cfg(target_vendor = "apple")]
            IPPROTO_ND,
            #[cfg(target_vendor = "apple")]
            IPPROTO_EON,
            #[cfg(target_vendor = "apple")]
            IPPROTO_XTP,
            INADDR_ANY,
            INADDR_BROADCAST,
            INADDR_LOOPBACK,
            INADDR_NONE,
            IP_TOS,
            IP_TTL,
            IP_HDRINCL,
            IP_MULTICAST_IF,
            IP_MULTICAST_TTL,
            IP_MULTICAST_LOOP,
            IP_ADD_MEMBERSHIP,
            IP_DROP_MEMBERSHIP,
            IP_RECVTOS,
            IP_PKTINFO,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            IP_TRANSPARENT,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            IP_BIND_ADDRESS_NO_PORT,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            IP_DEFAULT_MULTICAST_TTL,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            IP_DEFAULT_MULTICAST_LOOP,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            IP_MAX_MEMBERSHIPS = 20,
            #[cfg(target_vendor = "apple")]
            IP_RECVDSTADDR,
            IPV6_V6ONLY,
            #[cfg(target_vendor = "apple")]
            IPV6_JOIN_GROUP,
            #[cfg(target_vendor = "apple")]
            IPV6_LEAVE_GROUP,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            IPV6_JOIN_GROUP = libc::IPV6_ADD_MEMBERSHIP,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            IPV6_LEAVE_GROUP = libc::IPV6_DROP_MEMBERSHIP,
            IPV6_MULTICAST_HOPS,
            IPV6_MULTICAST_IF,
            IPV6_MULTICAST_LOOP,
            IPV6_UNICAST_HOPS,
            IPV6_CHECKSUM,
            IPV6_RECVTCLASS,
            IPV6_TCLASS,
            IPV6_RECVPKTINFO,
            IPV6_PKTINFO,
            IPV6_HOPLIMIT,
            IPV6_RECVHOPLIMIT,
            IPV6_DONTFRAG,
            TCP_NODELAY,
            TCP_MAXSEG,
            TCP_KEEPINTVL,
            TCP_KEEPCNT,
            TCP_FASTOPEN,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            TCP_KEEPIDLE,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            TCP_CORK,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            TCP_DEFER_ACCEPT,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            TCP_INFO,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            TCP_LINGER2,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            TCP_QUICKACK,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            TCP_SYNCNT,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            TCP_USER_TIMEOUT,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            TCP_WINDOW_CLAMP,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            TCP_CONGESTION,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            TCP_NOTSENT_LOWAT,
            #[cfg(target_vendor = "apple")]
            TCP_KEEPALIVE,
            AI_PASSIVE,
            AI_CANONNAME,
            AI_NUMERICHOST,
            AI_NUMERICSERV,
            AI_V4MAPPED,
            AI_ALL,
            AI_ADDRCONFIG,
            #[cfg(target_vendor = "apple")]
            AI_DEFAULT,
            #[cfg(target_vendor = "apple")]
            AI_MASK,
            #[cfg(target_vendor = "apple")]
            AI_V4MAPPED_CFG,
            NI_NUMERICHOST,
            NI_NUMERICSERV,
            NI_NOFQDN,
            NI_NAMEREQD,
            NI_DGRAM,
            NI_MAXHOST,
            #[cfg(target_vendor = "apple")]
            NI_MAXSERV,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            NI_MAXSERV = 32,
            EAI_AGAIN,
            EAI_BADFLAGS,
            EAI_FAIL,
            EAI_FAMILY,
            EAI_MEMORY,
            EAI_NONAME,
            EAI_SERVICE,
            EAI_SOCKTYPE,
            EAI_SYSTEM,
            EAI_OVERFLOW,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            EAI_NODATA,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            EAI_ADDRFAMILY = -9,
            #[cfg(target_vendor = "apple")]
            EAI_NODATA,
            #[cfg(target_vendor = "apple")]
            LOCAL_PEERCRED,
        )
    }
}

#[cfg(unix)]
pub use imp::*;
