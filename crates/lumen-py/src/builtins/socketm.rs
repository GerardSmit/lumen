//! `_socket` on `lumen_os::net`: Python address and timeout handling over the shared socket
//! syscalls. Blocking and timed operations wait for readiness in slices (`selectm::wait`), so
//! signal handlers and interrupts run while a socket waits.

/// Implementation module for socket operations.
///
/// See the socket module for documentation.
#[lumen_bind::module(name = "_socket")]
pub mod _socket {
    #![allow(clippy::new_ret_no_self)]

    use crate::bind::{opaque_instance, type_object, KwArgs, Py, This};
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_os::net::{self as net, SockAddr};
    use lumen_os::poll::{PollFd, POLLIN, POLLOUT};
    use lumen_os::FsError;
    use std::net::{Ipv4Addr, Ipv6Addr, SocketAddrV4, SocketAddrV6};

    use lumen_os::net::{AF_INET, AF_INET6, AF_UNIX, SOCK_STREAM};

    const EINTR: i32 = 4;
    const EBADF: i32 = 9;

    #[derive(Default)]
    pub struct State {
        default_timeout: Option<f64>,
        gaierror: Option<Obj>,
        herror: Option<Obj>,
    }

    fn os_err(it: &mut Interp, e: FsError) -> Obj {
        it.os_error_errno(e.errno(), None, None)
    }

    fn plain_os_error(it: &mut Interp, msg: &str) -> Obj {
        it.new_exc_str("OSError", msg)
    }

    fn gai_error(it: &mut Interp, e: net::GaiError) -> Obj {
        if e.code == net::EAI_SYSTEM {
            return it.os_error_errno(e.errno, None, None);
        }
        let cls = it.native_state::<State>().gaierror.clone().expect("_socket initialised");
        let msg = Value::string(net::gai_strerror(e.code));
        it.os_error_of(&cls, vec![Value::Int(e.code as i64), msg])
    }

    fn timeout_error(it: &mut Interp) -> Obj {
        it.new_exc_str("TimeoutError", "timed out")
    }

    /// The contents of a `bytes` or `bytearray`, else `None`.
    fn bytes_value(it: &mut Interp, v: &Value) -> R<Option<Vec<u8>>> {
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Bytes(_) | Kind::ByteArray(_)) => it.bytes_of(v).map(Some),
            _ => Ok(None),
        }
    }

    fn tuple_items(v: &Value) -> Option<Vec<Value>> {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Tuple(items) => Some(items.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// `idna_converter`: a host name as text.
    fn host_text(it: &mut Interp, v: &Value) -> R<String> {
        let raw = if let Some(s) = v.as_str() {
            if s.is_ascii() {
                s.as_bytes().to_vec()
            } else {
                let r = it.call_method(v, "encode", vec![Value::string("idna".to_string())]);
                match r {
                    Ok(b) => bytes_value(it, &b)?.unwrap_or_default(),
                    Err(_) => return Err(it.type_error("encoding of hostname failed")),
                }
            }
        } else if let Some(b) = bytes_value(it, v)? {
            b
        } else {
            let n = it.tp_name_of(v);
            return Err(it.type_error(&format!("str, bytes or bytearray expected, not {n}")));
        };
        if raw.contains(&0) {
            return Err(it.type_error("host name must not contain null character"));
        }
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }

    /// `setipaddr`: resolves `name` for `family` (`AF_INET`, `AF_INET6` or `AF_UNSPEC`).
    fn resolve_ip(it: &mut Interp, name: &str, family: i32) -> R<std::net::IpAddr> {
        if name.is_empty() {
            let list = net::getaddrinfo(None, Some("0"), family, net::SOCK_DGRAM, 0, net::AI_PASSIVE).map_err(|e| gai_error(it, e))?;
            if list.len() > 1 {
                return Err(plain_os_error(it, "wildcard resolved to multiple address"));
            }
            return match list.first().map(|a| &a.addr) {
                Some(SockAddr::V4(a)) => Ok((*a.ip()).into()),
                Some(SockAddr::V6(a)) => Ok((*a.ip()).into()),
                _ => Err(plain_os_error(it, "unsupported address family")),
            };
        }
        if name == "255.255.255.255" || name == "<broadcast>" {
            if family != AF_INET && family != 0 {
                return Err(plain_os_error(it, "address family mismatched"));
            }
            return Ok(Ipv4Addr::BROADCAST.into());
        }
        if family == 0 || family == AF_INET {
            if let Ok(Some(b)) = net::pton(AF_INET, name) {
                return Ok(Ipv4Addr::new(b[0], b[1], b[2], b[3]).into());
            }
        }
        if (family == 0 || family == AF_INET6) && !name.contains('%') {
            if let Ok(Some(b)) = net::pton(AF_INET6, name) {
                let a: [u8; 16] = b.try_into().unwrap_or([0; 16]);
                return Ok(Ipv6Addr::from(a).into());
            }
        }
        let list = net::getaddrinfo(Some(name), None, family, 0, 0, 0).map_err(|e| gai_error(it, e))?;
        match list.first().map(|a| &a.addr) {
            Some(SockAddr::V4(a)) => Ok((*a.ip()).into()),
            Some(SockAddr::V6(a)) => Ok((*a.ip()).into()),
            _ => Err(plain_os_error(it, "unknown address family")),
        }
    }

    fn int_arg(it: &mut Interp, v: &Value) -> R<i64> {
        if !v.is_int_like() {
            let n = it.tp_name_of(v);
            return Err(it.type_error(&format!("'{n}' object cannot be interpreted as an integer")));
        }
        it.index_of(v)
    }

    /// `getsockaddrarg`: a Python address for a socket of `family`.
    fn sockaddr_arg(it: &mut Interp, family: i32, v: &Value, caller: &str) -> R<SockAddr> {
        match family {
            AF_UNIX => {
                let path = if v.as_str().is_some() {
                    let args = vec![Value::string("utf-8".into()), Value::string("surrogateescape".into())];
                    let b = it.call_method(v, "encode", args)?;
                    it.bytes_of(&b)?
                } else {
                    it.bytes_of(v)?
                };
                if path.len() >= net::UNIX_PATH_MAX {
                    return Err(plain_os_error(it, "AF_UNIX path too long"));
                }
                Ok(SockAddr::Unix(path))
            }
            AF_INET | AF_INET6 => {
                let fam_name = if family == AF_INET { "AF_INET" } else { "AF_INET6" };
                let Some(items) = tuple_items(v) else {
                    let n = it.tp_name_of(v);
                    return Err(it.type_error(&format!("{caller}(): {fam_name} address must be tuple, not {n}")));
                };
                let ok_len = if family == AF_INET { items.len() == 2 } else { (2..=4).contains(&items.len()) };
                if !ok_len {
                    let msg = if family == AF_INET {
                        "AF_INET address must be a pair (host, port)"
                    } else {
                        "AF_INET6 address must be a tuple (host, port[, flowinfo[, scopeid]])"
                    };
                    return Err(it.type_error(msg));
                }
                let host = host_text(it, &items[0])?;
                let port = int_arg(it, &items[1])?;
                let ip = resolve_ip(it, &host, family)?;
                if !(0..=0xffff).contains(&port) {
                    return Err(it.overflow_err(&format!("{caller}(): port must be 0-65535.")));
                }
                match ip {
                    std::net::IpAddr::V4(ip) => Ok(SockAddr::V4(SocketAddrV4::new(ip, port as u16))),
                    std::net::IpAddr::V6(ip) => {
                        let flowinfo = match items.get(2) {
                            Some(f) => int_arg(it, f)?,
                            None => 0,
                        };
                        let scope_id = match items.get(3) {
                            Some(s) => int_arg(it, s)?,
                            None => 0,
                        };
                        if !(0..=0xfffff).contains(&flowinfo) {
                            return Err(it.overflow_err(&format!("{caller}(): flowinfo must be 0-1048575.")));
                        }
                        Ok(SockAddr::V6(SocketAddrV6::new(ip, port as u16, flowinfo as u32, scope_id as u32)))
                    }
                }
            }
            _ => Err(plain_os_error(it, &format!("{caller}(): bad family"))),
        }
    }

    /// `makesockaddr`.
    fn sockaddr_value(it: &mut Interp, a: &SockAddr) -> Value {
        let _ = it;
        match a {
            SockAddr::V4(a) => Value::tuple(vec![Value::string(a.ip().to_string()), Value::Int(a.port() as i64)]),
            SockAddr::V6(a) => {
                let ip = net::ntop(AF_INET6, &a.ip().octets()).unwrap_or_else(|_| a.ip().to_string());
                Value::tuple(vec![Value::string(ip), Value::Int(a.port() as i64), Value::Int(a.flowinfo() as i64), Value::Int(a.scope_id() as i64)])
            }
            SockAddr::Unix(p) if p.first() == Some(&0) => Value::bytes(p.clone()),
            SockAddr::Unix(p) => Value::string(crate::bind::path::bytes_path(p)),
            SockAddr::Other(f) => Value::tuple(vec![Value::Int(*f as i64), Value::bytes(Vec::new())]),
        }
    }

    /// `socket_parse_timeout`: `None` (blocking) or non-negative seconds.
    fn parse_timeout(it: &mut Interp, v: &Value) -> R<Option<f64>> {
        let secs = match v {
            Value::None => return Ok(None),
            Value::Float(f) => *f,
            v => int_arg(it, v)? as f64,
        };
        if secs.is_nan() {
            return Err(it.value_error("Invalid value NaN (not a number)"));
        }
        if secs < 0.0 {
            return Err(it.value_error("Timeout value out of range"));
        }
        Ok(Some(secs))
    }

    pub(crate) fn now(it: &mut Interp) -> f64 {
        it.platform.borrow().monotonic_ns() as f64 / 1e9
    }

    /// Waits until `fd` is readable or writable or `deadline` (monotonic seconds) passes;
    /// returns whether it became ready.
    pub(crate) fn wait_ready(it: &mut Interp, fd: i32, writing: bool, deadline: Option<f64>) -> R<bool> {
        let left = deadline.map(|d| ((d - now(it)) * 1000.0).ceil().max(0.0) as i64);
        let events = if writing { POLLOUT } else { POLLIN };
        crate::builtins::selectm::wait(it, left, |ms| {
            let mut p = [PollFd::new(fd, events)];
            lumen_os::poll::poll(&mut p, ms).map(|n| (n > 0, n > 0))
        })
    }

    /// Waits until `fd` is ready (unless the socket is non-blocking) and runs `op`, retrying on
    /// `EINTR` and on spurious readiness, as CPython's `sock_call_ex`.
    fn sock_call<T>(it: &mut Interp, fd: i32, writing: bool, timeout: Option<f64>, mut op: impl FnMut() -> lumen_os::net::R<T>) -> R<T> {
        if fd < 0 {
            return Err(it.os_error_errno(EBADF, None, None));
        }
        let deadline = timeout.filter(|&t| t > 0.0).map(|t| now(it) + t);
        loop {
            if timeout != Some(0.0) && !wait_ready(it, fd, writing, deadline)? {
                return Err(timeout_error(it));
            }
            let err = loop {
                match op() {
                    Ok(v) => return Ok(v),
                    Err(e) if e.errno() == EINTR => crate::builtins::signalm::check(it)?,
                    Err(e) => break e,
                }
            };
            if timeout == Some(0.0) || !net::would_block(err.errno()) {
                return Err(os_err(it, err));
            }
        }
    }

    /// socket(family=AF_INET, type=SOCK_STREAM, proto=0) -> socket object
    /// socket(family=-1, type=-1, proto=-1, fileno=None) -> socket object
    ///
    /// Open a socket of the given type.  The family argument specifies the
    /// address family; it defaults to AF_INET.  The type argument specifies
    /// whether this is a stream (SOCK_STREAM, this is the default)
    /// or datagram (SOCK_DGRAM) socket.  The protocol argument defaults to 0,
    /// specifying the default protocol.  Keyword arguments are accepted.
    /// The socket is created as non-inheritable.
    ///
    /// When a fileno is passed in, family, type and proto are auto-detected,
    /// unless they are explicitly set.
    ///
    /// A socket object represents one endpoint of a network connection.
    #[class(name = "socket", module = "_socket")]
    pub struct Sock {
        fd: i32,
        family: i32,
        ty: i32,
        proto: i32,
        timeout: Option<f64>,
    }

    impl Drop for Sock {
        fn drop(&mut self) {
            if self.fd >= 0 {
                let _ = net::close(self.fd);
            }
        }
    }

    fn fields(it: &mut Interp, s: &Py<Sock>) -> R<(i32, i32, Option<f64>)> {
        s.with(it, |s| (s.fd, s.family, s.timeout))
    }

    /// The descriptor and timeout of `v` when it is a `socket.socket` (or a subclass instance).
    pub(crate) fn fd_and_timeout(it: &mut Interp, v: &Value) -> Option<(i32, Option<f64>)> {
        let sock = Py::<Sock>::from_value(it, v)?;
        sock.with(it, |s| (s.fd, s.timeout)).ok()
    }

    fn set_blocking_fd(fd: i32, timeout: Option<f64>) -> lumen_os::net::R<()> {
        if fd < 0 {
            return Ok(());
        }
        lumen_os::fdctl::set_blocking(fd, timeout.is_none())
    }

    /// `init_sockobject`: the socket state for `fd`, in the module's default timeout mode.
    // SOCK_NONBLOCK and SOCK_CLOEXEC are 0 off Linux.
    #[allow(clippy::bad_bit_mask)]
    fn make_sock(it: &mut Interp, fd: i32, family: i32, ty: i32, proto: i32) -> R<Sock> {
        let mut timeout = it.native_state::<State>().default_timeout;
        if ty & net::SOCK_NONBLOCK != 0 {
            timeout = Some(0.0);
        }
        let ty = ty & !(net::SOCK_NONBLOCK | net::SOCK_CLOEXEC);
        let sock = Sock { fd, family, ty, proto, timeout };
        if timeout.is_some() {
            set_blocking_fd(fd, timeout).map_err(|e| os_err(it, e))?;
        }
        Ok(sock)
    }

    fn new_socket(it: &mut Interp, fd: i32, family: i32, ty: i32, proto: i32) -> R<Value> {
        let sock = make_sock(it, fd, family, ty, proto)?;
        Ok(Py::new(it, sock).value().clone())
    }

    #[methods]
    impl Sock {
        #[constructor]
        fn new(cls: This<Value>, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> Value {
            let _ = (args, kw);
            let Value::Obj(cls) = &cls.0 else { unreachable!() };
            opaque_instance(cls, Sock { fd: -1, family: AF_INET, ty: SOCK_STREAM, proto: 0, timeout: None })
        }

        /// Initialize self.  See help(type(self)) for accurate signature.
        #[method(name = "__init__", hint(py(text_signature = "($self, /, *args, **kwargs)")))]
        fn init(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[kw]
            #[default(-1)]
            family: i32,
            #[kw]
            #[default(-1)]
            r#type: i32,
            #[kw]
            #[default(-1)]
            proto: i32,
            #[kw] fileno: Option<&Value>,
        ) -> R<()> {
            let fdobj = fileno.filter(|v| !matches!(v, Value::None));
            let (fd, family, ty, proto) = if let Some(fdobj) = fdobj {
                let fd = int_arg(it, fdobj)?;
                if fd < 0 {
                    return Err(it.value_error("negative file descriptor"));
                }
                let fd = fd as i32;
                let (f, t, p) = match net::socket_info(fd) {
                    Ok(info) => info,
                    Err(e) => return Err(os_err(it, e)),
                };
                (fd, if family == -1 { f } else { family }, if r#type == -1 { t } else { r#type }, if proto == -1 { p } else { proto })
            } else {
                let family = if family == -1 { AF_INET } else { family };
                let ty = if r#type == -1 { SOCK_STREAM } else { r#type };
                let proto = if proto == -1 { 0 } else { proto };
                let fd = net::socket(family, ty, proto).map_err(|e| os_err(it, e))?;
                (fd, family, ty, proto)
            };
            let sock = make_sock(it, fd, family, ty, proto)?;
            slf.0.with(it, |s| *s = sock)
        }

        /// Return repr(self).
        #[method(name = "__repr__")]
        fn repr(&self) -> String {
            format!("<socket object, fd={}, family={}, type={}, proto={}>", self.fd, self.family, self.ty, self.proto)
        }

        /// the socket family
        #[getter]
        fn family(&self) -> i32 {
            self.family
        }

        /// the socket type
        #[getter]
        fn r#type(&self) -> i32 {
            self.ty
        }

        /// the socket protocol
        #[getter]
        fn proto(&self) -> i32 {
            self.proto
        }

        /// the socket timeout
        #[getter]
        fn timeout(&self) -> Option<f64> {
            self.timeout
        }

        /// _accept() -> (integer, address info)
        ///
        /// Wait for an incoming connection.  Return a new socket file descriptor
        /// representing the connection, and the address of the client.
        /// For IP sockets, the address info is a pair (hostaddr, port).
        #[method(name = "_accept")]
        fn accept(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let (fd, _, timeout) = fields(it, &slf.0)?;
            let (nfd, addr) = sock_call(it, fd, false, timeout, || net::accept(fd))?;
            let addr = sockaddr_value(it, &addr);
            Ok(Value::tuple(vec![Value::Int(nfd as i64), addr]))
        }

        /// bind(address)
        ///
        /// Bind the socket to a local address.  For IP sockets, the address is a
        /// pair (host, port); the host must refer to the local host. For raw packet
        /// sockets the address is a tuple (ifname, proto [,pkttype [,hatype [,addr]]])
        fn bind(slf: This<Py<Self>>, it: &mut Interp, address: &Value) -> R<()> {
            let (fd, family, _) = fields(it, &slf.0)?;
            let addr = sockaddr_arg(it, family, address, "bind")?;
            net::bind(fd, &addr).map_err(|e| os_err(it, e))
        }

        /// close()
        ///
        /// Close the socket.  It cannot be used after this call.
        fn close(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
            let fd = slf.0.with(it, |s| std::mem::replace(&mut s.fd, -1))?;
            if fd >= 0 {
                if let Err(e) = net::close(fd) {
                    if e.0 != "ECONNRESET" {
                        return Err(os_err(it, e));
                    }
                }
            }
            Ok(())
        }

        /// detach()
        ///
        /// Close the socket object without closing the underlying file descriptor.
        /// The object cannot be used after this call, but the file descriptor
        /// can be reused for other purposes.  The file descriptor is returned.
        fn detach(slf: This<Py<Self>>, it: &mut Interp) -> R<i32> {
            slf.0.with(it, |s| std::mem::replace(&mut s.fd, -1))
        }

        /// connect(address)
        ///
        /// Connect the socket to a remote address.  For IP sockets, the address
        /// is a pair (host, port).
        fn connect(slf: This<Py<Self>>, it: &mut Interp, address: &Value) -> R<()> {
            match connect_impl(it, &slf.0, address, "connect")? {
                0 => Ok(()),
                e => Err(it.os_error_errno(e, None, None)),
            }
        }

        /// connect_ex(address) -> errno
        ///
        /// This is like connect(address), but returns an error code (the errno value)
        /// instead of raising an exception when an error occurs.
        fn connect_ex(slf: This<Py<Self>>, it: &mut Interp, address: &Value) -> R<i32> {
            connect_impl(it, &slf.0, address, "connect_ex")
        }

        /// fileno() -> integer
        ///
        /// Return the integer file descriptor of the socket.
        fn fileno(&self) -> i32 {
            self.fd
        }

        /// getsockname() -> address info
        ///
        /// Return the address of the local endpoint. The format depends on the
        /// address family. For IPv4 sockets, the address info is a pair
        /// (hostaddr, port). For IPv6 sockets, the address info is a 4-tuple
        /// (hostaddr, port, flowinfo, scope_id).
        fn getsockname(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let (fd, _, _) = fields(it, &slf.0)?;
            let a = net::getsockname(fd).map_err(|e| os_err(it, e))?;
            Ok(sockaddr_value(it, &a))
        }

        /// getpeername() -> address info
        ///
        /// Return the address of the remote endpoint.  For IP sockets, the address
        /// info is a pair (hostaddr, port).
        fn getpeername(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            let (fd, _, _) = fields(it, &slf.0)?;
            let a = net::getpeername(fd).map_err(|e| os_err(it, e))?;
            Ok(sockaddr_value(it, &a))
        }

        /// getsockopt(level, option[, buffersize]) -> value
        ///
        /// Get a socket option.  See the Unix manual for level and option.
        /// If a nonzero buffersize argument is given, the return value is a
        /// string of that length; otherwise it is an integer.
        fn getsockopt(slf: This<Py<Self>>, it: &mut Interp, level: i32, option: i32, buflen: Option<i64>) -> R<Value> {
            let (fd, _, _) = fields(it, &slf.0)?;
            match buflen {
                None => net::getsockopt_int(fd, level, option).map(|v| Value::Int(v as i64)).map_err(|e| os_err(it, e)),
                Some(n) if n <= 0 || n > 1024 => Err(plain_os_error(it, "getsockopt buflen out of range")),
                Some(n) => net::getsockopt(fd, level, option, n as usize).map(Value::bytes).map_err(|e| os_err(it, e)),
            }
        }

        /// setsockopt(level, option, value: int)
        /// setsockopt(level, option, value: buffer)
        /// setsockopt(level, option, None, optlen: int)
        ///
        /// Set a socket option.  See the Unix manual for level and option.
        /// The value argument can either be an integer, a string buffer, or
        /// None, optlen.
        fn setsockopt(slf: This<Py<Self>>, it: &mut Interp, level: i32, option: i32, value: &Value, optlen: Option<u32>) -> R<()> {
            let (fd, _, _) = fields(it, &slf.0)?;
            let r = match (value, optlen) {
                (Value::None, Some(len)) => net::setsockopt_null(fd, level, option, len),
                (v, None) if v.is_int_like() => {
                    let n = it.index_of(v)?;
                    net::setsockopt_int(fd, level, option, n as i32)
                }
                (v, None) => {
                    let b = it.bytes_of(v)?;
                    net::setsockopt(fd, level, option, &b)
                }
                (_, Some(_)) => return Err(it.type_error("setsockopt() takes 3 or 4 arguments")),
            };
            r.map_err(|e| os_err(it, e))
        }

        /// gettimeout() -> timeout
        ///
        /// Returns the timeout in seconds (float) associated with socket
        /// operations. A timeout of None indicates that timeouts on socket
        /// operations are disabled.
        fn gettimeout(&self) -> Option<f64> {
            self.timeout
        }

        /// settimeout(timeout)
        ///
        /// Set a timeout on socket operations.  'timeout' can be a float,
        /// giving in seconds, or None.  Setting a timeout of None disables
        /// the timeout feature and is equivalent to setblocking(1).
        /// Setting a timeout of zero is the same as setblocking(0).
        fn settimeout(slf: This<Py<Self>>, it: &mut Interp, timeout: &Value) -> R<()> {
            let t = parse_timeout(it, timeout)?;
            let fd = slf.0.with(it, |s| {
                s.timeout = t;
                s.fd
            })?;
            set_blocking_fd(fd, t).map_err(|e| os_err(it, e))
        }

        /// setblocking(flag)
        ///
        /// Set the socket to blocking (flag is true) or non-blocking (false).
        /// setblocking(True) is equivalent to settimeout(None);
        /// setblocking(False) is equivalent to settimeout(0.0).
        fn setblocking(slf: This<Py<Self>>, it: &mut Interp, flag: &Value) -> R<()> {
            let block = it.truthy(flag)?;
            let t = if block { None } else { Some(0.0) };
            let fd = slf.0.with(it, |s| {
                s.timeout = t;
                s.fd
            })?;
            set_blocking_fd(fd, t).map_err(|e| os_err(it, e))
        }

        /// getblocking()
        ///
        /// Returns True if socket is in blocking mode, or False if it
        /// is in non-blocking mode.
        fn getblocking(&self) -> bool {
            self.timeout != Some(0.0)
        }

        /// listen([backlog])
        ///
        /// Enable a server to accept connections.  If backlog is specified, it must be
        /// at least 0 (if it is lower, it is set to 0); it specifies the number of
        /// unaccepted connections that the system will allow before refusing new
        /// connections. If not specified, a default reasonable value is chosen.
        fn listen(slf: This<Py<Self>>, it: &mut Interp, backlog: Option<i32>) -> R<()> {
            let (fd, _, _) = fields(it, &slf.0)?;
            let backlog = backlog.unwrap_or(128.min(net::SOMAXCONN)).max(0);
            net::listen(fd, backlog).map_err(|e| os_err(it, e))
        }

        /// recv(buffersize[, flags]) -> data
        ///
        /// Receive up to buffersize bytes from the socket.  For the optional flags
        /// argument, see the Unix manual.  When no data is available, block until
        /// at least one byte is available or until the remote end is closed.  When
        /// the remote end is closed and all data is read, return the empty string.
        fn recv(slf: This<Py<Self>>, it: &mut Interp, bufsize: i64, #[default(0)] flags: i32) -> R<Value> {
            if bufsize < 0 {
                return Err(it.value_error("negative buffersize in recv"));
            }
            let (fd, _, timeout) = fields(it, &slf.0)?;
            let mut buf = vec![0u8; bufsize as usize];
            let n = sock_call(it, fd, false, timeout, || net::recv(fd, &mut buf, flags))?;
            buf.truncate(n);
            Ok(Value::bytes(buf))
        }

        /// recv_into(buffer, [nbytes[, flags]]) -> nbytes_read
        ///
        /// A version of recv() that stores its data into a buffer rather than creating
        /// a new string.  Receive up to buffersize bytes from the socket.  If buffersize
        /// is not specified (or 0), receive up to the size available in the given buffer.
        ///
        /// See recv() for documentation about the flags.
        fn recv_into(slf: This<Py<Self>>, it: &mut Interp, buffer: &mut [u8], #[default(0)] nbytes: i64, #[default(0)] flags: i32) -> R<usize> {
            if nbytes < 0 {
                return Err(it.value_error("negative buffersize in recv_into"));
            }
            let n = if nbytes == 0 { buffer.len() } else { nbytes as usize };
            if n > buffer.len() {
                return Err(it.value_error("buffer too small for requested bytes"));
            }
            let (fd, _, timeout) = fields(it, &slf.0)?;
            sock_call(it, fd, false, timeout, || net::recv(fd, &mut buffer[..n], flags))
        }

        /// recvfrom(buffersize[, flags]) -> (data, address info)
        ///
        /// Like recv(buffersize, flags) but also return the sender's address info.
        fn recvfrom(slf: This<Py<Self>>, it: &mut Interp, bufsize: i64, #[default(0)] flags: i32) -> R<Value> {
            if bufsize < 0 {
                return Err(it.value_error("negative buffersize in recvfrom"));
            }
            let (fd, _, timeout) = fields(it, &slf.0)?;
            let mut buf = vec![0u8; bufsize as usize];
            let (n, addr) = sock_call(it, fd, false, timeout, || net::recvfrom(fd, &mut buf, flags))?;
            buf.truncate(n);
            let addr = addr.map_or(Value::None, |a| sockaddr_value(it, &a));
            Ok(Value::tuple(vec![Value::bytes(buf), addr]))
        }

        /// recvfrom_into(buffer[, nbytes[, flags]]) -> (nbytes, address info)
        ///
        /// Like recv_into(buffer[, nbytes[, flags]]) but also return the sender's address info.
        fn recvfrom_into(slf: This<Py<Self>>, it: &mut Interp, buffer: &mut [u8], #[default(0)] nbytes: i64, #[default(0)] flags: i32) -> R<Value> {
            if nbytes < 0 {
                return Err(it.value_error("negative buffersize in recvfrom_into"));
            }
            let n = if nbytes == 0 { buffer.len() } else { nbytes as usize };
            if n > buffer.len() {
                return Err(it.value_error("nbytes is greater than the length of the buffer"));
            }
            let (fd, _, timeout) = fields(it, &slf.0)?;
            let (got, addr) = sock_call(it, fd, false, timeout, || net::recvfrom(fd, &mut buffer[..n], flags))?;
            let addr = addr.map_or(Value::None, |a| sockaddr_value(it, &a));
            Ok(Value::tuple(vec![Value::Int(got as i64), addr]))
        }

        /// send(data[, flags]) -> count
        ///
        /// Send a data string to the socket.  For the optional flags
        /// argument, see the Unix manual.  Return the number of bytes
        /// sent; this may be less than len(data) if the network is busy.
        fn send(slf: This<Py<Self>>, it: &mut Interp, data: &[u8], #[default(0)] flags: i32) -> R<usize> {
            let (fd, _, timeout) = fields(it, &slf.0)?;
            sock_call(it, fd, true, timeout, || net::send(fd, data, flags | nosignal()))
        }

        /// sendall(data[, flags])
        ///
        /// Send a data string to the socket.  For the optional flags
        /// argument, see the Unix manual.  This calls send() repeatedly
        /// until all data is sent.  If an error occurs, it's impossible
        /// to tell how much data has been sent.
        fn sendall(slf: This<Py<Self>>, it: &mut Interp, data: &[u8], #[default(0)] flags: i32) -> R<()> {
            let (fd, _, timeout) = fields(it, &slf.0)?;
            let deadline = timeout.filter(|&t| t > 0.0).map(|t| now(it) + t);
            let mut at = 0;
            while at < data.len() {
                let left = match deadline {
                    Some(d) => {
                        let l = d - now(it);
                        if l <= 0.0 {
                            return Err(timeout_error(it));
                        }
                        Some(l)
                    }
                    None => timeout,
                };
                at += sock_call(it, fd, true, left, || net::send(fd, &data[at..], flags | nosignal()))?;
                crate::builtins::signalm::check(it)?;
            }
            Ok(())
        }

        /// sendto(data[, flags], address) -> count
        ///
        /// Like send(data, flags) but allows specifying the destination address.
        /// For IP sockets, the address is a pair (hostaddr, port).
        fn sendto(slf: This<Py<Self>>, it: &mut Interp, data: &[u8], flags_or_address: &Value, address: Option<&Value>) -> R<usize> {
            let (flags, address) = match address {
                Some(a) => (int_arg(it, flags_or_address)? as i32, a),
                None => (0, flags_or_address),
            };
            let (fd, family, timeout) = fields(it, &slf.0)?;
            let addr = sockaddr_arg(it, family, address, "sendto")?;
            sock_call(it, fd, true, timeout, || net::sendto(fd, data, flags | nosignal(), &addr))
        }

        /// shutdown(flag)
        ///
        /// Shut down the reading side of the socket (flag == SHUT_RD), the writing side
        /// of the socket (flag == SHUT_WR), or both ends (flag == SHUT_RDWR).
        fn shutdown(slf: This<Py<Self>>, it: &mut Interp, how: i32) -> R<()> {
            let (fd, _, _) = fields(it, &slf.0)?;
            net::shutdown(fd, how).map_err(|e| os_err(it, e))
        }
    }

    pub(crate) fn nosignal() -> i32 {
        net::MSG_NOSIGNAL
    }

    /// `internal_connect`: 0 or the errno of the failure.
    fn connect_impl(it: &mut Interp, s: &Py<Sock>, address: &Value, caller: &str) -> R<i32> {
        let (fd, family, timeout) = fields(it, s)?;
        if fd < 0 {
            return Ok(EBADF);
        }
        let addr = sockaddr_arg(it, family, address, caller)?;
        let err = match net::connect(fd, &addr) {
            Ok(()) => return Ok(0),
            Err(e) => e.errno(),
        };
        let wait = if err == EINTR {
            crate::builtins::signalm::check(it)?;
            timeout != Some(0.0)
        } else {
            timeout.is_some_and(|t| t > 0.0) && net::in_progress(err)
        };
        if !wait {
            return Ok(err);
        }
        let deadline = timeout.filter(|&t| t > 0.0).map(|t| now(it) + t);
        if !wait_ready(it, fd, true, deadline)? {
            return Err(timeout_error(it));
        }
        Ok(net::take_error(fd).unwrap_or_else(|e| e.errno()))
    }

    /// gethostname() -> string
    ///
    /// Return the current host name.
    #[op]
    fn gethostname(it: &mut Interp) -> R<String> {
        net::gethostname().map_err(|e| os_err(it, e))
    }

    /// gethostbyname(host) -> address
    ///
    /// Return the IP address (a string of the form '255.255.255.255') for a host.
    #[op]
    fn gethostbyname(it: &mut Interp, hostname: &Value) -> R<String> {
        let name = host_text(it, hostname)?;
        let ip = resolve_ip(it, &name, AF_INET)?;
        Ok(ip.to_string())
    }

    /// gethostbyname_ex(host) -> (name, aliaslist, addresslist)
    ///
    /// Return the true host name, a list of aliases, and a list of IP addresses,
    /// for a host.  The host argument is a string giving a host name or IP number.
    #[op]
    fn gethostbyname_ex(it: &mut Interp, hostname: &Value) -> R<Value> {
        let name = host_text(it, hostname)?;
        let list = net::getaddrinfo(Some(&name), None, AF_INET, SOCK_STREAM, 0, net::AI_CANONNAME).map_err(|e| gai_error(it, e))?;
        let canon = list.iter().find(|a| !a.canonname.is_empty()).map_or(name.clone(), |a| a.canonname.clone());
        let mut addrs: Vec<String> = Vec::new();
        for a in &list {
            if let SockAddr::V4(v) = &a.addr {
                let s = v.ip().to_string();
                if !addrs.contains(&s) {
                    addrs.push(s);
                }
            }
        }
        Ok(Value::tuple(vec![
            Value::string(canon),
            Value::list(Vec::new()),
            Value::list(addrs.into_iter().map(Value::string).collect()),
        ]))
    }

    /// gethostbyaddr(host) -> (name, aliaslist, addresslist)
    ///
    /// Return the true host name, a list of aliases, and a list of IP addresses,
    /// for a host.  The host argument is a string giving a host name or IP number.
    #[op]
    fn gethostbyaddr(it: &mut Interp, ip_address: &Value) -> R<Value> {
        let name = host_text(it, ip_address)?;
        let ip = resolve_ip(it, &name, 0)?;
        let addr = match ip {
            std::net::IpAddr::V4(ip) => SockAddr::V4(SocketAddrV4::new(ip, 0)),
            std::net::IpAddr::V6(ip) => SockAddr::V6(SocketAddrV6::new(ip, 0, 0, 0)),
        };
        match net::getnameinfo(&addr, net::NI_NAMEREQD) {
            Ok((host, _)) => Ok(Value::tuple(vec![
                Value::string(host),
                Value::list(Vec::new()),
                Value::list(vec![Value::string(ip.to_string())]),
            ])),
            Err(_) => {
                let cls = it.native_state::<State>().herror.clone().expect("_socket initialised");
                Err(it.os_error_of(&cls, vec![Value::Int(1), Value::string("Unknown host".to_string())]))
            }
        }
    }

    fn proto_text(it: &mut Interp, v: Option<&Value>) -> R<Option<String>> {
        match v {
            None => Ok(None),
            Some(v) => match v.as_str() {
                Some(s) => Ok(Some(s.to_string())),
                None => {
                    let n = it.tp_name_of(v);
                    Err(it.type_error(&format!("argument 2 must be str, not {n}")))
                }
            },
        }
    }

    /// getservbyname(servicename[, protocolname]) -> integer
    ///
    /// Return a port number from a service name and protocol name.
    /// The optional protocol name, if given, should be 'tcp' or 'udp',
    /// otherwise any protocol will match.
    #[op]
    fn getservbyname(it: &mut Interp, servicename: &str, protocolname: Option<&Value>) -> R<i64> {
        let proto = proto_text(it, protocolname)?;
        match net::getservbyname(servicename, proto.as_deref()) {
            Some(p) => Ok(p as i64),
            None => Err(plain_os_error(it, "service/proto not found")),
        }
    }

    /// getservbyport(port[, protocolname]) -> string
    ///
    /// Return the service name from a port number and protocol name.
    /// The optional protocol name, if given, should be 'tcp' or 'udp',
    /// otherwise any protocol will match.
    #[op]
    fn getservbyport(it: &mut Interp, port: i64, protocolname: Option<&Value>) -> R<String> {
        if !(0..=0xffff).contains(&port) {
            return Err(it.overflow_err("getservbyport: port must be 0-65535."));
        }
        let proto = proto_text(it, protocolname)?;
        match net::getservbyport(port as u16, proto.as_deref()) {
            Some(s) => Ok(s),
            None => Err(plain_os_error(it, "port/proto not found")),
        }
    }

    /// getprotobyname(name) -> integer
    ///
    /// Return the protocol number for the named protocol.  (Rarely used.)
    #[op]
    fn getprotobyname(it: &mut Interp, protocolname: &str) -> R<i64> {
        match net::getprotobyname(protocolname) {
            Some(p) => Ok(p as i64),
            None => Err(plain_os_error(it, "protocol not found")),
        }
    }

    /// close(integer) -> None
    ///
    /// Close an integer socket file descriptor.  This is like os.close(), but for
    /// sockets; on some platforms os.close() won't work for socket file descriptors.
    #[op]
    fn close(it: &mut Interp, fd: i32) -> R<()> {
        match net::close(fd) {
            Err(e) if e.0 != "ECONNRESET" => Err(os_err(it, e)),
            _ => Ok(()),
        }
    }

    /// dup(integer) -> integer
    ///
    /// Duplicate an integer socket file descriptor.  This is like os.dup(), but for
    /// sockets; on some platforms os.dup() won't work for socket file descriptors.
    #[op]
    fn dup(it: &mut Interp, fd: i32) -> R<i32> {
        net::dup(fd).map_err(|e| os_err(it, e))
    }

    /// socketpair([family[, type [, proto]]]) -> (socket object, socket object)
    ///
    /// Create a pair of socket objects from the sockets returned by the platform
    /// socketpair() function.
    /// The arguments are the same as for socket() except the default family is
    /// AF_UNIX if defined on the platform; otherwise, the default is AF_INET.
    #[op]
    fn socketpair(it: &mut Interp, family: Option<i32>, r#type: Option<i32>, #[default(0)] proto: i32) -> R<Value> {
        let family = family.unwrap_or(AF_UNIX);
        let ty = r#type.unwrap_or(SOCK_STREAM);
        let (a, b) = net::socketpair(family, ty, proto).map_err(|e| os_err(it, e))?;
        let sa = new_socket(it, a, family, ty, proto)?;
        let sb = new_socket(it, b, family, ty, proto)?;
        Ok(Value::tuple(vec![sa, sb]))
    }

    fn u16_arg(it: &mut Interp, x: &Value, name: &str) -> R<u16> {
        let n = int_arg(it, x)?;
        if n < 0 {
            return Err(it.overflow_err(&format!("{name}: can't convert negative Python int to C 16-bit unsigned integer")));
        }
        if n > 0xffff {
            return Err(it.overflow_err(&format!("{name}: Python int too large to convert to C 16-bit unsigned integer")));
        }
        Ok(n as u16)
    }

    fn u32_arg(it: &mut Interp, x: &Value) -> R<u32> {
        let n = int_arg(it, x)?;
        if n < 0 {
            return Err(it.overflow_err("can't convert negative value to unsigned int"));
        }
        u32::try_from(n).map_err(|_| it.overflow_err("int larger than 32 bits"))
    }

    /// ntohs(integer) -> integer
    ///
    /// Convert a 16-bit unsigned integer from network to host byte order.
    #[op]
    fn ntohs(it: &mut Interp, x: &Value) -> R<i64> {
        Ok(u16::from_be(u16_arg(it, x, "ntohs")?) as i64)
    }

    /// htons(integer) -> integer
    ///
    /// Convert a 16-bit unsigned integer from host to network byte order.
    #[op]
    fn htons(it: &mut Interp, x: &Value) -> R<i64> {
        Ok(u16_arg(it, x, "htons")?.to_be() as i64)
    }

    /// ntohl(integer) -> integer
    ///
    /// Convert a 32-bit integer from network to host byte order.
    #[op]
    fn ntohl(it: &mut Interp, x: &Value) -> R<i64> {
        Ok(u32::from_be(u32_arg(it, x)?) as i64)
    }

    /// htonl(integer) -> integer
    ///
    /// Convert a 32-bit integer from host to network byte order.
    #[op]
    fn htonl(it: &mut Interp, x: &Value) -> R<i64> {
        Ok(u32_arg(it, x)?.to_be() as i64)
    }

    /// inet_aton(string) -> bytes giving packed 32-bit IP representation
    ///
    /// Convert an IP address in string format (123.45.67.89) to the 32-bit packed
    /// binary format used in low-level network functions.
    #[op]
    fn inet_aton(it: &mut Interp, ip_addr: &str) -> R<Value> {
        match net::aton(ip_addr) {
            Some(b) => Ok(Value::bytes(b.to_vec())),
            None => Err(plain_os_error(it, "illegal IP address string passed to inet_aton")),
        }
    }

    /// inet_ntoa(packed_ip) -> ip_address_string
    ///
    /// Convert an IP address from 32-bit packed binary format to string format
    #[op]
    fn inet_ntoa(it: &mut Interp, packed_ip: &[u8]) -> R<String> {
        if packed_ip.len() != 4 {
            return Err(plain_os_error(it, "packed IP wrong length for inet_ntoa"));
        }
        Ok(Ipv4Addr::new(packed_ip[0], packed_ip[1], packed_ip[2], packed_ip[3]).to_string())
    }

    /// inet_pton(af, ip) -> packed IP address string
    ///
    /// Convert an IP address from string format to a packed string suitable
    /// for use with low-level network functions.
    #[op]
    fn inet_pton(it: &mut Interp, address_family: i32, ip_string: &str) -> R<Value> {
        match net::pton(address_family, ip_string) {
            Ok(Some(b)) => Ok(Value::bytes(b)),
            Ok(None) => Err(plain_os_error(it, "illegal IP address string passed to inet_pton")),
            Err(e) => Err(os_err(it, e)),
        }
    }

    /// inet_ntop(af, packed_ip) -> string formatted IP address
    ///
    /// Convert a packed IP address of the given family to string format.
    #[op]
    fn inet_ntop(it: &mut Interp, address_family: i32, packed_ip: &[u8]) -> R<String> {
        let want = match address_family {
            AF_INET => 4,
            AF_INET6 => 16,
            f => return Err(it.value_error(&format!("unknown address family {f}"))),
        };
        if packed_ip.len() != want {
            return Err(it.value_error("invalid length of packed IP address string"));
        }
        net::ntop(address_family, packed_ip).map_err(|e| os_err(it, e))
    }

    /// getaddrinfo(host, port [, family, type, proto, flags])
    ///     -> list of (family, type, proto, canonname, sockaddr)
    ///
    /// Resolve host and port into addrinfo struct.
    #[op]
    fn getaddrinfo(
        it: &mut Interp,
        #[kw] host: &Value,
        #[kw] port: &Value,
        #[kw]
        #[default(0)]
        family: i32,
        #[kw]
        #[default(0)]
        r#type: i32,
        #[kw]
        #[default(0)]
        proto: i32,
        #[kw]
        #[default(0)]
        flags: i32,
    ) -> R<Value> {
        let host = match host {
            Value::None => None,
            v => Some(host_text(it, v)?),
        };
        let port = match port {
            Value::None => None,
            v if v.is_int_like() && !matches!(v, Value::Bool(_)) => Some(it.index_of(v)?.to_string()),
            v if v.as_str().is_some() => Some(v.as_str().unwrap_or_default().to_string()),
            v => match bytes_value(it, v)? {
                Some(b) => Some(String::from_utf8_lossy(&b).into_owned()),
                None => return Err(plain_os_error(it, "Int or String expected")),
            },
        };
        let list = net::getaddrinfo(host.as_deref(), port.as_deref(), family, r#type, proto, flags).map_err(|e| gai_error(it, e))?;
        let mut out = Vec::with_capacity(list.len());
        for ai in list {
            let addr = sockaddr_value(it, &ai.addr);
            out.push(Value::tuple(vec![
                Value::Int(ai.family as i64),
                Value::Int(ai.socktype as i64),
                Value::Int(ai.proto as i64),
                Value::string(ai.canonname),
                addr,
            ]));
        }
        Ok(Value::list(out))
    }

    /// getnameinfo(sockaddr, flags) --> (host, port)
    ///
    /// Get host and port for a sockaddr.
    #[op]
    fn getnameinfo(it: &mut Interp, sockaddr: &Value, flags: i32) -> R<Value> {
        let Some(items) = tuple_items(sockaddr) else {
            return Err(it.type_error("getnameinfo() argument 1 must be a tuple"));
        };
        if !(2..=4).contains(&items.len()) {
            return Err(it.type_error("getnameinfo(): illegal sockaddr argument"));
        }
        let host = match items[0].as_str() {
            Some(s) => s.to_string(),
            None => return Err(it.type_error("getnameinfo(): illegal sockaddr argument")),
        };
        let port = int_arg(it, &items[1])?;
        let flowinfo = match items.get(2) {
            Some(v) => int_arg(it, v)?,
            None => 0,
        };
        let scope_id = match items.get(3) {
            Some(v) => int_arg(it, v)?,
            None => 0,
        };
        if !(0..=0xfffff).contains(&flowinfo) {
            return Err(it.overflow_err("getnameinfo(): flowinfo must be 0-1048575."));
        }
        let list = net::getaddrinfo(Some(&host), Some(&port.to_string()), net::AF_UNSPEC, net::SOCK_DGRAM, 0, net::AI_NUMERICHOST).map_err(|e| gai_error(it, e))?;
        if list.len() != 1 {
            return Err(plain_os_error(it, "sockaddr resolved to multiple addresses"));
        }
        let addr = match &list[0].addr {
            SockAddr::V4(a) => {
                if items.len() != 2 {
                    return Err(plain_os_error(it, "IPv4 sockaddr must be 2 tuple"));
                }
                SockAddr::V4(*a)
            }
            SockAddr::V6(a) => SockAddr::V6(SocketAddrV6::new(*a.ip(), a.port(), flowinfo as u32, scope_id as u32)),
            _ => return Err(plain_os_error(it, "unknown family")),
        };
        let (h, s) = net::getnameinfo(&addr, flags).map_err(|e| gai_error(it, e))?;
        Ok(Value::tuple(vec![Value::string(h), Value::string(s)]))
    }

    /// getdefaulttimeout() -> timeout
    ///
    /// Returns the default timeout in seconds (float) for new socket objects.
    /// A value of None indicates that new socket objects have no timeout.
    /// When the socket module is first imported, the default is None.
    #[op]
    fn getdefaulttimeout(it: &mut Interp) -> Option<f64> {
        it.native_state::<State>().default_timeout
    }

    /// setdefaulttimeout(timeout)
    ///
    /// Set the default timeout in seconds (float) for new socket objects.
    /// A value of None indicates that new socket objects have no timeout.
    /// When the socket module is first imported, the default is None.
    #[op]
    fn setdefaulttimeout(it: &mut Interp, timeout: &Value) -> R<()> {
        let t = parse_timeout(it, timeout)?;
        it.native_state::<State>().default_timeout = t;
        Ok(())
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let os_error = it.exc_type("OSError");
        dict_set_str(&d, "error", Value::Obj(os_error.clone()));
        dict_set_str(&d, "timeout", Value::Obj(it.exc_type("TimeoutError")));
        let herror = crate::builtins::native::new_type(it, "socket", "herror", Some(&os_error), Layout::Exception);
        let gaierror = crate::builtins::native::new_type(it, "socket", "gaierror", Some(&os_error), Layout::Exception);
        dict_set_str(&d, "herror", Value::Obj(herror.clone()));
        dict_set_str(&d, "gaierror", Value::Obj(gaierror.clone()));
        let sock = type_object::<Sock>(it);
        dict_set_str(&d, "SocketType", Value::Obj(sock));
        dict_set_str(&d, "has_ipv6", Value::Bool(true));
        for &(name, v) in net::constants() {
            dict_set_str(&d, name, Value::Int(v));
        }
        let st = it.native_state::<State>();
        st.herror = Some(herror);
        st.gaierror = Some(gaierror);
    }
}
