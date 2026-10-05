//! `select` on `lumen_os::poll`: `select()` and poll objects. Waits run in short slices so
//! signal handlers and interrupts are serviced while blocked.

use crate::object::*;
use crate::vm::Interp;
use lumen_os::FsError;

const SLICE_MS: i64 = 20;

/// Repeats `wait_once(slice_ms)` (which reports whether anything is ready) until something is
/// ready or `timeout_ms` passes (`None`: forever), servicing signals between slices.
pub(crate) fn wait<T: Send>(
    it: &mut Interp,
    timeout_ms: Option<i64>,
    mut wait_once: impl FnMut(i32) -> Result<(bool, T), FsError> + Send,
) -> R<T> {
    it.flush_out();
    let start = it.platform.borrow().monotonic_ns();
    loop {
        let left = match timeout_ms {
            Some(ms) => {
                let spent =
                    (it.platform.borrow().monotonic_ns().saturating_sub(start) / 1_000_000) as i64;
                (ms - spent).max(0)
            }
            None => SLICE_MS,
        };
        let slice = left.min(SLICE_MS) as i32;
        match it.unlocked(|| wait_once(slice)) {
            Ok((false, _)) if timeout_ms.is_none() || left > i64::from(slice) => {}
            Ok((_, r)) => return Ok(r),
            Err(e) if e.errno() == 4 => {}
            Err(e) => return Err(it.os_error_errno(e.errno(), None, None)),
        }
        it.poll()?;
    }
}

/// This module supports asynchronous I/O on multiple file descriptors.
///
/// *** IMPORTANT NOTICE ***
/// On Windows, only sockets are supported; on Unix, all file descriptors.
#[lumen_bind::module(name = "select")]
pub mod select {
    use crate::bind::{opaque_instance, type_object, Py, This};
    use crate::builtins::posixm::as_file_descriptor;
    use crate::object::*;
    use crate::vm::{dict_del_str, dict_set_str, Interp};
    use lumen_os::event::Kevent;
    use lumen_os::poll::{PollFd, POLLIN, POLLOUT, POLLPRI};

    fn fd_list(it: &mut Interp, seq: &Value) -> R<Vec<(Value, i32)>> {
        let items = it.iterate_to_vec(seq)?;
        let mut out = Vec::with_capacity(items.len());
        for v in items {
            let fd = as_file_descriptor(it, &v)?;
            out.push((v, fd));
        }
        Ok(out)
    }

    /// Wait until one or more file descriptors are ready for some kind of I/O.
    ///
    /// The first three arguments are iterables of file descriptors to be waited for:
    /// rlist -- wait until ready for reading
    /// wlist -- wait until ready for writing
    /// xlist -- wait for an "exceptional condition"
    /// If only one kind of condition is required, pass [] for the other lists.
    ///
    /// A file descriptor is either a socket or file object, or a small integer
    /// gotten from a fileno() method call on one of those.
    ///
    /// The optional 4th argument specifies a timeout in seconds; it may be
    /// a floating point number to specify fractions of seconds.  If it is absent
    /// or None, the call will never time out.
    ///
    /// The return value is a tuple of three lists corresponding to the first three
    /// arguments; each contains the subset of the corresponding file descriptors
    /// that are ready.
    ///
    /// *** IMPORTANT NOTICE ***
    /// On Windows, only sockets are supported; on Unix, all file
    /// descriptors can be used.
    #[op]
    fn select(
        it: &mut Interp,
        rlist: &Value,
        wlist: &Value,
        xlist: &Value,
        timeout: Option<&Value>,
    ) -> R<Value> {
        let timeout_ms = match timeout {
            None | Some(Value::None) => None,
            Some(v) => {
                let secs = match v {
                    Value::Float(f) => *f,
                    v if v.is_int_like() => it.index_of(v)? as f64,
                    v => {
                        let name = it.type_name_of(v);
                        return Err(it.type_error(&format!(
                            "'{name}' object cannot be interpreted as an integer"
                        )));
                    }
                };
                if secs.is_nan() {
                    return Err(it.value_error("Invalid value NaN (not a number)"));
                }
                if secs < 0.0 {
                    return Err(it.value_error("timeout must be non-negative"));
                }
                Some((secs * 1000.0).ceil().min(i64::MAX as f64) as i64)
            }
        };
        let lists = [
            fd_list(it, rlist)?,
            fd_list(it, wlist)?,
            fd_list(it, xlist)?,
        ];
        let fds: Vec<Vec<i32>> = lists
            .iter()
            .map(|l| l.iter().map(|e| e.1).collect())
            .collect();
        if fds
            .iter()
            .flatten()
            .any(|&fd| fd as usize >= lumen_os::poll::FD_SETSIZE)
        {
            return Err(it.value_error("filedescriptor out of range in select()"));
        }
        let ready = super::wait(it, timeout_ms, |ms| {
            let flags = lumen_os::poll::select([&fds[0], &fds[1], &fds[2]], ms)?;
            Ok((flags.iter().flatten().any(|&f| f), flags))
        })?;
        let out = lists
            .iter()
            .zip(&ready)
            .map(|(list, flags)| {
                Value::list(
                    list.iter()
                        .zip(flags)
                        .filter(|e| *e.1)
                        .map(|e| e.0 .0.clone())
                        .collect(),
                )
            })
            .collect();
        Ok(Value::tuple(out))
    }

    #[class(name = "poll", module = "select", skip(py))]
    pub struct Poll {
        fds: Vec<(i32, i16)>,
        polling: bool,
    }

    fn event_mask(it: &mut Interp, v: Option<&Value>, default: i16) -> R<i16> {
        let Some(v) = v else { return Ok(default) };
        let n = it.index_of(v)?;
        if n < 0 {
            return Err(it.overflow_err("can't convert negative int to unsigned"));
        }
        if n > u16::MAX as i64 {
            return Err(it.overflow_err("Python int too large for C unsigned short"));
        }
        Ok(n as u16 as i16)
    }

    #[methods]
    impl Poll {
        /// Register a file descriptor with the polling object.
        ///
        ///   fd
        ///     either an integer, or an object with a fileno() method returning an int
        ///   eventmask
        ///     an optional bitmask describing the type of events to check for
        fn register(
            slf: This<Py<Self>>,
            it: &mut Interp,
            fd: &Value,
            eventmask: Option<&Value>,
        ) -> R<()> {
            let fd = as_file_descriptor(it, fd)?;
            let mask = event_mask(it, eventmask, POLLIN | POLLPRI | POLLOUT)?;
            slf.0
                .with(it, |s| match s.fds.iter_mut().find(|e| e.0 == fd) {
                    Some(e) => e.1 = mask,
                    None => s.fds.push((fd, mask)),
                })
        }

        /// Modify an already registered file descriptor.
        ///
        ///   fd
        ///     either an integer, or an object with a fileno() method returning
        ///     an int
        ///   eventmask
        ///     a bitmask describing the type of events to check for
        fn modify(slf: This<Py<Self>>, it: &mut Interp, fd: &Value, eventmask: &Value) -> R<()> {
            let fd = as_file_descriptor(it, fd)?;
            let mask = event_mask(it, Some(eventmask), 0)?;
            let found = slf
                .0
                .with(it, |s| match s.fds.iter_mut().find(|e| e.0 == fd) {
                    Some(e) => {
                        e.1 = mask;
                        true
                    }
                    None => false,
                })?;
            if !found {
                return Err(it.os_error_errno(2, None, None));
            }
            Ok(())
        }

        /// Remove a file descriptor being tracked by the polling object.
        fn unregister(slf: This<Py<Self>>, it: &mut Interp, fd: &Value) -> R<()> {
            let n = as_file_descriptor(it, fd)?;
            let found = slf.0.with(it, |s| {
                let before = s.fds.len();
                s.fds.retain(|e| e.0 != n);
                s.fds.len() != before
            })?;
            if !found {
                return Err(it.new_exc_val("KeyError", Value::Int(n as i64)));
            }
            Ok(())
        }

        /// Polls the set of registered file descriptors.
        ///
        ///   timeout
        ///     The maximum time to wait in milliseconds, or else None (or a negative
        ///     value) to wait indefinitely.
        ///
        /// Returns a list containing any descriptors that have events or errors to
        /// report, as a list of (fd, event) 2-tuples.
        fn poll(slf: This<Py<Self>>, it: &mut Interp, timeout: Option<&Value>) -> R<Value> {
            let timeout_ms = match timeout {
                None | Some(Value::None) => None,
                Some(Value::Float(f)) if f.is_nan() => {
                    return Err(it.value_error("Invalid value NaN (not a number)"));
                }
                Some(Value::Float(f)) => Some(f.ceil() as i64),
                Some(v) if v.is_int_like() => Some(it.index_of(v)?),
                Some(_) => return Err(it.type_error("timeout must be an integer or None")),
            };
            let timeout_ms = timeout_ms.filter(|&ms| ms >= 0);
            let busy = slf
                .0
                .with(it, |s| std::mem::replace(&mut s.polling, true))?;
            if busy {
                return Err(it.runtime_error("concurrent poll() invocation"));
            }
            let mut fds: Vec<PollFd> = slf.0.with(it, |s| {
                s.fds.iter().map(|&(fd, ev)| PollFd::new(fd, ev)).collect()
            })?;
            let r = super::wait(it, timeout_ms, |ms| {
                lumen_os::poll::poll(&mut fds, ms).map(|n| (n > 0, ()))
            });
            slf.0.with(it, |s| s.polling = false)?;
            r?;
            let out = fds
                .iter()
                .filter(|f| f.revents != 0)
                .map(|f| {
                    Value::tuple(vec![
                        Value::Int(f.fd as i64),
                        Value::Int(f.revents as u16 as i64),
                    ])
                })
                .collect();
            Ok(Value::list(out))
        }
    }

    /// Returns a polling object.
    ///
    /// This object supports registering and unregistering file descriptors, and then
    /// polling them for I/O events.
    #[op(name = "poll")]
    fn new_poll(it: &mut Interp) -> Value {
        let cls = type_object::<Poll>(it);
        opaque_instance(
            &cls,
            Poll {
                fds: Vec::new(),
                polling: false,
            },
        )
    }

    const FD_SETSIZE: i64 = 1024;

    fn closed_err(it: &mut Interp, what: &str) -> Obj {
        it.value_error(&format!("I/O operation on closed {what} object"))
    }

    fn seconds_to_ms(it: &mut Interp, v: Option<&Value>) -> R<Option<i64>> {
        match v {
            None | Some(Value::None) => Ok(None),
            Some(Value::Float(f)) if f.is_nan() => {
                Err(it.value_error("Invalid value NaN (not a number)"))
            }
            Some(Value::Float(f)) => {
                Ok(Some((f * 1000.0).ceil().min(i64::MAX as f64) as i64).filter(|&ms| ms >= 0))
            }
            Some(v) if v.is_int_like() => {
                Ok(Some(it.index_of(v)?.saturating_mul(1000)).filter(|&ms| ms >= 0))
            }
            Some(v) => {
                let name = it.type_name_of(v);
                Err(it.type_error(&format!(
                    "'{name}' object cannot be interpreted as an integer"
                )))
            }
        }
    }

    fn event_bits(it: &mut Interp, v: Option<&Value>, default: u32) -> R<u32> {
        let Some(v) = v else { return Ok(default) };
        let n = it.index_of(v)?;
        if n < 0 {
            return Err(it.overflow_err("can't convert negative int to unsigned"));
        }
        u32::try_from(n).map_err(|_| it.overflow_err("Python int too large for C unsigned int"))
    }

    /// Epoll: Linux edge-triggered and level-triggered I/O event notification.
    #[class(name = "epoll", module = "select")]
    pub struct Epoll {
        fd: i32,
    }

    impl Drop for Epoll {
        fn drop(&mut self) {
            if self.fd >= 0 {
                let _ = lumen_os::fs::close(self.fd);
            }
        }
    }

    impl Epoll {
        fn live(&self, it: &mut Interp) -> R<i32> {
            if self.fd < 0 {
                return Err(closed_err(it, "epoll"));
            }
            Ok(self.fd)
        }

        fn ctl(&self, it: &mut Interp, op: i32, fd: &Value, events: u32) -> R<()> {
            let epfd = self.live(it)?;
            let fd = as_file_descriptor(it, fd)?;
            lumen_os::event::epoll_ctl(epfd, op, fd, events)
                .map_err(|e| it.os_error_errno(e.errno(), None, None))
        }
    }

    #[methods]
    impl Epoll {
        /// Returns an epolling object.
        ///
        ///   sizehint
        ///     The expected number of events to be registered.  It must be positive,
        ///     or -1 to use the default.  It is only used on older systems where
        ///     epoll_create1() is not available; otherwise it has no effect (though its
        ///     value is still checked).
        ///   flags
        ///     Deprecated and completely ignored.  However, when supplied, its value
        ///     must be 0 or select.EPOLL_CLOEXEC, otherwise OSError is raised.
        #[constructor]
        fn new(
            it: &mut Interp,
            #[kw]
            #[default(-1)]
            sizehint: i32,
            #[kw]
            #[default(0)]
            flags: i32,
        ) -> R<Epoll> {
            if sizehint != -1 && sizehint <= 0 {
                return Err(it.value_error("negative sizehint"));
            }
            if flags != 0 && flags != 0o2000000 {
                return Err(it.os_error_errno(22, None, None));
            }
            let fd = lumen_os::event::epoll_create()
                .map_err(|e| it.os_error_errno(e.errno(), None, None))?;
            Ok(Epoll { fd })
        }

        /// Close the epoll control file descriptor.
        ///
        /// Further operations on the epoll object will raise an exception.
        fn close(&mut self, it: &mut Interp) -> R<()> {
            if self.fd >= 0 {
                let fd = std::mem::replace(&mut self.fd, -1);
                lumen_os::fs::close(fd).map_err(|e| it.os_error_errno(e.errno(), None, None))?;
            }
            Ok(())
        }

        /// True if the epoll handler is closed
        #[getter]
        fn closed(&self) -> bool {
            self.fd < 0
        }

        /// Return the epoll control file descriptor.
        fn fileno(&self, it: &mut Interp) -> R<i32> {
            self.live(it)
        }

        /// Create an epoll object from a given control fd.
        #[classmethod]
        fn fromfd(cls: This<Value>, it: &mut Interp, fd: i32) -> Value {
            let _ = cls;
            let ty = type_object::<Epoll>(it);
            opaque_instance(&ty, Epoll { fd })
        }

        /// Registers a new fd or raises an OSError if the fd is already registered.
        ///
        ///   fd
        ///     the target file descriptor of the operation
        ///   eventmask
        ///     a bit set composed of the various EPOLL constants
        ///
        /// The epoll interface supports all file descriptors that support poll.
        fn register(
            &self,
            it: &mut Interp,
            #[kw] fd: &Value,
            #[kw] eventmask: Option<&Value>,
        ) -> R<()> {
            let mask = event_bits(it, eventmask, 1 | 2 | 4)?;
            self.ctl(it, 1, fd, mask)
        }

        /// Modify event mask for a registered file descriptor.
        ///
        ///   fd
        ///     the target file descriptor of the operation
        ///   eventmask
        ///     a bit set composed of the various EPOLL constants
        fn modify(&self, it: &mut Interp, #[kw] fd: &Value, #[kw] eventmask: &Value) -> R<()> {
            let mask = event_bits(it, Some(eventmask), 0)?;
            self.ctl(it, 3, fd, mask)
        }

        /// Remove a registered file descriptor from the epoll object.
        ///
        ///   fd
        ///     the target file descriptor of the operation
        fn unregister(&self, it: &mut Interp, #[kw] fd: &Value) -> R<()> {
            self.ctl(it, 2, fd, 0)
        }

        /// Wait for events on the epoll file descriptor.
        ///
        ///   timeout
        ///     the maximum time to wait in seconds (as float);
        ///     a timeout of None or -1 makes poll wait indefinitely
        ///   maxevents
        ///     the maximum number of events returned; -1 means no limit
        ///
        /// Returns a list containing any descriptors that have events to report,
        /// as a list of (fd, events) 2-tuples.
        fn poll(
            slf: This<Py<Self>>,
            it: &mut Interp,
            #[kw] timeout: Option<&Value>,
            #[kw]
            #[default(-1)]
            maxevents: i64,
        ) -> R<Value> {
            let timeout_ms = seconds_to_ms(it, timeout)?;
            let max = if maxevents == -1 {
                FD_SETSIZE - 1
            } else if maxevents < 1 {
                return Err(it.value_error(&format!(
                    "maxevents must be greater than 0, got {maxevents}"
                )));
            } else {
                maxevents
            };
            let fd = slf.0.borrow(it)?.live(it)?;
            let events = super::wait(it, timeout_ms, |ms| {
                lumen_os::event::epoll_wait(fd, max as usize, ms).map(|v| (!v.is_empty(), v))
            })?;
            Ok(Value::list(
                events
                    .into_iter()
                    .map(|(fd, ev)| {
                        Value::tuple(vec![Value::Int(fd as i64), Value::Int(ev as i64)])
                    })
                    .collect(),
            ))
        }

        #[proto(enter)]
        fn enter(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
            slf.0.borrow(it)?.live(it)?;
            Ok(slf.0.value().clone())
        }

        #[proto(exit)]
        fn exit(&mut self, it: &mut Interp, #[varargs] args: &[Value]) -> R<()> {
            let _ = args;
            self.close(it)
        }
    }

    /// kevent(ident, filter=KQ_FILTER_READ, flags=KQ_EV_ADD, fflags=0, data=0, udata=0)
    ///
    /// This object is the equivalent of the struct kevent for the C API.
    #[class(name = "kevent", module = "select")]
    pub struct KeventObj {
        k: Kevent,
    }

    fn kevent_key(k: &Kevent) -> (usize, i16, u16, u32, isize, usize) {
        (k.ident, k.filter, k.flags, k.fflags, k.data, k.udata)
    }

    fn kevent_cmp(it: &mut Interp, a: &Kevent, other: &Value) -> Option<std::cmp::Ordering> {
        let p = Py::<KeventObj>::from_value(it, other)?;
        let b = p.borrow(it).ok()?.k;
        Some(kevent_key(a).cmp(&kevent_key(&b)))
    }

    #[methods]
    impl KeventObj {
        #[constructor]
        fn new(
            it: &mut Interp,
            #[kw] ident: &Value,
            #[kw]
            #[default(-1)]
            filter: i16,
            #[kw]
            #[default(1)]
            flags: u16,
            #[kw]
            #[default(0)]
            fflags: u32,
            #[kw]
            #[default(0)]
            data: i64,
            #[kw]
            #[default(0)]
            udata: usize,
        ) -> R<KeventObj> {
            let ident = as_file_descriptor(it, ident)? as usize;
            Ok(KeventObj {
                k: Kevent {
                    ident,
                    filter,
                    flags,
                    fflags,
                    data: data as isize,
                    udata,
                },
            })
        }

        #[getter]
        fn ident(&self) -> i64 {
            self.k.ident as i64
        }
        #[setter(name = "ident")]
        fn set_ident(&mut self, it: &mut Interp, v: &Value) -> R<()> {
            self.k.ident = as_file_descriptor(it, v)? as usize;
            Ok(())
        }

        #[getter]
        fn filter(&self) -> i64 {
            self.k.filter as i64
        }
        #[setter(name = "filter")]
        fn set_filter(&mut self, v: i16) {
            self.k.filter = v;
        }

        #[getter]
        fn flags(&self) -> i64 {
            self.k.flags as i64
        }
        #[setter(name = "flags")]
        fn set_flags(&mut self, v: u16) {
            self.k.flags = v;
        }

        #[getter]
        fn fflags(&self) -> i64 {
            self.k.fflags as i64
        }
        #[setter(name = "fflags")]
        fn set_fflags(&mut self, v: u32) {
            self.k.fflags = v;
        }

        #[getter]
        fn data(&self) -> i64 {
            self.k.data as i64
        }
        #[setter(name = "data")]
        fn set_data(&mut self, v: i64) {
            self.k.data = v as isize;
        }

        #[getter]
        fn udata(&self) -> i64 {
            self.k.udata as i64
        }
        #[setter(name = "udata")]
        fn set_udata(&mut self, v: usize) {
            self.k.udata = v;
        }

        #[proto(repr)]
        fn repr(&self) -> String {
            let k = &self.k;
            format!(
                "<select.kevent ident={} filter={} flags=0x{:x} fflags=0x{:x} data=0x{:x} udata=0x{:x}>",
                k.ident, k.filter, k.flags, k.fflags, k.data as i64 as u64, k.udata
            )
        }

        #[proto(eq)]
        fn eq(&self, it: &mut Interp, other: &Value) -> Value {
            kevent_cmp(it, &self.k, other).map_or(Value::NotImplemented, |o| Value::Bool(o.is_eq()))
        }
        #[proto(ne)]
        fn ne(&self, it: &mut Interp, other: &Value) -> Value {
            kevent_cmp(it, &self.k, other).map_or(Value::NotImplemented, |o| Value::Bool(o.is_ne()))
        }
        #[proto(lt)]
        fn lt(&self, it: &mut Interp, other: &Value) -> Value {
            kevent_cmp(it, &self.k, other).map_or(Value::NotImplemented, |o| Value::Bool(o.is_lt()))
        }
        #[proto(le)]
        fn le(&self, it: &mut Interp, other: &Value) -> Value {
            kevent_cmp(it, &self.k, other).map_or(Value::NotImplemented, |o| Value::Bool(o.is_le()))
        }
        #[proto(gt)]
        fn gt(&self, it: &mut Interp, other: &Value) -> Value {
            kevent_cmp(it, &self.k, other).map_or(Value::NotImplemented, |o| Value::Bool(o.is_gt()))
        }
        #[proto(ge)]
        fn ge(&self, it: &mut Interp, other: &Value) -> Value {
            kevent_cmp(it, &self.k, other).map_or(Value::NotImplemented, |o| Value::Bool(o.is_ge()))
        }
    }

    /// Kqueue syscall wrapper.
    ///
    /// For example, to start watching a socket for input:
    /// >>> kq = kqueue()
    /// >>> sock = socket()
    /// >>> sock.connect((host, port))
    /// >>> kq.control([kevent(sock, KQ_FILTER_WRITE, KQ_EV_ADD)], 0)
    ///
    /// To wait one second for it to become writeable:
    /// >>> kq.control(None, 1, 1000)
    ///
    /// To stop listening:
    /// >>> kq.control([kevent(sock, KQ_FILTER_WRITE, KQ_EV_DELETE)], 0)
    #[class(name = "kqueue", module = "select")]
    pub struct Kqueue {
        fd: i32,
    }

    impl Drop for Kqueue {
        fn drop(&mut self) {
            if self.fd >= 0 {
                let _ = lumen_os::fs::close(self.fd);
            }
        }
    }

    impl Kqueue {
        fn live(&self, it: &mut Interp) -> R<i32> {
            if self.fd < 0 {
                return Err(closed_err(it, "kqueue"));
            }
            Ok(self.fd)
        }
    }

    #[methods]
    impl Kqueue {
        #[constructor]
        fn new(it: &mut Interp) -> R<Kqueue> {
            let fd =
                lumen_os::event::kqueue().map_err(|e| it.os_error_errno(e.errno(), None, None))?;
            Ok(Kqueue { fd })
        }

        /// Close the kqueue control file descriptor.
        ///
        /// Further operations on the kqueue object will raise an exception.
        fn close(&mut self, it: &mut Interp) -> R<()> {
            if self.fd >= 0 {
                let fd = std::mem::replace(&mut self.fd, -1);
                lumen_os::fs::close(fd).map_err(|e| it.os_error_errno(e.errno(), None, None))?;
            }
            Ok(())
        }

        /// True if the kqueue handler is closed
        #[getter]
        fn closed(&self) -> bool {
            self.fd < 0
        }

        /// Return the kqueue control file descriptor.
        fn fileno(&self, it: &mut Interp) -> R<i32> {
            self.live(it)
        }

        /// Create a kqueue object from a given control fd.
        #[classmethod]
        fn fromfd(cls: This<Value>, it: &mut Interp, fd: i32) -> Value {
            let _ = cls;
            let ty = type_object::<Kqueue>(it);
            opaque_instance(&ty, Kqueue { fd })
        }

        /// Calls the kernel kevent function.
        ///
        ///   changelist
        ///     Must be an iterable of kevent objects describing the changes to be made
        ///     to the kernel's watch list or None.
        ///   maxevents
        ///     The maximum number of events that the kernel will return.
        ///   timeout
        ///     The maximum time to wait in seconds, or else None to wait forever.
        ///     This accepts floats for smaller timeouts, too.
        fn control(
            slf: This<Py<Self>>,
            it: &mut Interp,
            changelist: &Value,
            maxevents: i64,
            timeout: Option<&Value>,
        ) -> R<Value> {
            let fd = slf.0.borrow(it)?.live(it)?;
            if maxevents < 0 {
                return Err(it.value_error(&format!(
                    "Length of eventlist must be 0 or positive, got {maxevents}"
                )));
            }
            let timeout_ms = match timeout {
                None | Some(Value::None) => None,
                Some(v) => {
                    let secs = match v {
                        Value::Float(f) => *f,
                        v if v.is_int_like() => it.index_of(v)? as f64,
                        v => {
                            let name = it.type_name_of(v);
                            return Err(it.type_error(&format!(
                                "'{name}' object cannot be interpreted as an integer"
                            )));
                        }
                    };
                    if secs.is_nan() {
                        return Err(it.value_error("Invalid value NaN (not a number)"));
                    }
                    if secs < 0.0 {
                        return Err(it.value_error("timeout must be positive or None"));
                    }
                    Some((secs * 1000.0).ceil().min(i64::MAX as f64) as i64)
                }
            };
            let mut changes = Vec::new();
            if !matches!(changelist, Value::None) {
                let items = match it.iterate_to_vec(changelist) {
                    Ok(items) => items,
                    Err(_) => return Err(it.type_error("changelist is not iterable")),
                };
                for item in items {
                    let Some(p) = Py::<KeventObj>::from_value(it, &item) else {
                        return Err(it.type_error(
                            "changelist must be an iterable of select.kevent objects",
                        ));
                    };
                    changes.push(p.borrow(it)?.k);
                }
            }
            if !changes.is_empty() {
                lumen_os::event::kevent(fd, &changes, 0, Some((0, 0)))
                    .map_err(|e| it.os_error_errno(e.errno(), None, None))?;
            }
            if maxevents == 0 {
                return Ok(Value::list(Vec::new()));
            }
            let events = super::wait(it, timeout_ms, |ms| {
                let r = lumen_os::event::kevent(
                    fd,
                    &[],
                    maxevents as usize,
                    Some((i64::from(ms) / 1000, i64::from(ms) % 1000 * 1_000_000)),
                )?;
                Ok((!r.is_empty(), r))
            })?;
            let out = events
                .into_iter()
                .map(|k| Py::new(it, KeventObj { k }).value().clone())
                .collect();
            Ok(Value::list(out))
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        dict_set_str(&d, "error", Value::Obj(it.exc_type("OSError")));
        for &(name, v) in lumen_os::poll::constants() {
            dict_set_str(&d, name, Value::Int(v as i64));
        }
        if lumen_os::event::HAVE_EPOLL {
            for (name, v) in lumen_os::event::epoll_constants() {
                dict_set_str(&d, name, Value::Int(v));
            }
        } else {
            dict_del_str(&d, "epoll");
        }
        if lumen_os::event::HAVE_KQUEUE {
            for (name, v) in lumen_os::event::kqueue_constants() {
                dict_set_str(&d, name, Value::Int(v));
            }
        } else {
            dict_del_str(&d, "kqueue");
            dict_del_str(&d, "kevent");
        }
    }
}
