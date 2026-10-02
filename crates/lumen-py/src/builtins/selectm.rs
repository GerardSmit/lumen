//! `select` on `lumen_os::poll`: `select()` and poll objects. Waits run in short slices so
//! signal handlers and interrupts are serviced while blocked.

use crate::object::*;
use crate::vm::Interp;
use lumen_os::FsError;

const SLICE_MS: i64 = 20;

/// Repeats `wait_once(slice_ms)` (which reports whether anything is ready) until something is
/// ready or `timeout_ms` passes (`None`: forever), servicing signals between slices.
fn wait<T>(it: &mut Interp, timeout_ms: Option<i64>, mut wait_once: impl FnMut(i32) -> Result<(bool, T), FsError>) -> R<T> {
    it.flush_out();
    let start = it.platform.borrow().monotonic_ns();
    loop {
        let left = match timeout_ms {
            Some(ms) => {
                let spent = (it.platform.borrow().monotonic_ns().saturating_sub(start) / 1_000_000) as i64;
                (ms - spent).max(0)
            }
            None => SLICE_MS,
        };
        let slice = left.min(SLICE_MS) as i32;
        match wait_once(slice) {
            Ok((false, _)) if left > i64::from(slice) => {}
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
    use crate::bind::{type_object, opaque_instance, Py, This};
    use crate::builtins::posixm::as_file_descriptor;
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
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
    fn select(it: &mut Interp, rlist: &Value, wlist: &Value, xlist: &Value, timeout: Option<&Value>) -> R<Value> {
        let timeout_ms = match timeout {
            None | Some(Value::None) => None,
            Some(v) => {
                let secs = match v {
                    Value::Float(f) => *f,
                    v if v.is_int_like() => it.index_of(v)? as f64,
                    v => {
                        let name = it.type_name_of(v);
                        return Err(it.type_error(&format!("'{name}' object cannot be interpreted as an integer")));
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
        let lists = [fd_list(it, rlist)?, fd_list(it, wlist)?, fd_list(it, xlist)?];
        let fds: Vec<Vec<i32>> = lists.iter().map(|l| l.iter().map(|e| e.1).collect()).collect();
        if fds.iter().flatten().any(|&fd| fd as usize >= lumen_os::poll::FD_SETSIZE) {
            return Err(it.value_error("filedescriptor out of range in select()"));
        }
        let ready = super::wait(it, timeout_ms, |ms| {
            let flags = lumen_os::poll::select([&fds[0], &fds[1], &fds[2]], ms)?;
            Ok((flags.iter().flatten().any(|&f| f), flags))
        })?;
        let out = lists
            .iter()
            .zip(&ready)
            .map(|(list, flags)| Value::list(list.iter().zip(flags).filter(|e| *e.1).map(|e| e.0 .0.clone()).collect()))
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
        fn register(slf: This<Py<Self>>, it: &mut Interp, fd: &Value, eventmask: Option<&Value>) -> R<()> {
            let fd = as_file_descriptor(it, fd)?;
            let mask = event_mask(it, eventmask, POLLIN | POLLPRI | POLLOUT)?;
            slf.0.with(it, |s| match s.fds.iter_mut().find(|e| e.0 == fd) {
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
            let found = slf.0.with(it, |s| match s.fds.iter_mut().find(|e| e.0 == fd) {
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
                Some(Value::Float(f)) if f.is_nan() => return Err(it.value_error("Invalid value NaN (not a number)")),
                Some(Value::Float(f)) => Some(f.ceil() as i64),
                Some(v) if v.is_int_like() => Some(it.index_of(v)?),
                Some(_) => return Err(it.type_error("timeout must be an integer or None")),
            };
            let timeout_ms = timeout_ms.filter(|&ms| ms >= 0);
            let busy = slf.0.with(it, |s| std::mem::replace(&mut s.polling, true))?;
            if busy {
                return Err(it.runtime_error("concurrent poll() invocation"));
            }
            let mut fds: Vec<PollFd> = slf.0.with(it, |s| s.fds.iter().map(|&(fd, ev)| PollFd::new(fd, ev)).collect())?;
            let r = super::wait(it, timeout_ms, |ms| lumen_os::poll::poll(&mut fds, ms).map(|n| (n > 0, ())));
            slf.0.with(it, |s| s.polling = false)?;
            r?;
            let out = fds
                .iter()
                .filter(|f| f.revents != 0)
                .map(|f| Value::tuple(vec![Value::Int(f.fd as i64), Value::Int(f.revents as u16 as i64)]))
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
        opaque_instance(&cls, Poll { fds: Vec::new(), polling: false })
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        dict_set_str(&d, "error", Value::Obj(it.exc_type("OSError")));
        for &(name, v) in lumen_os::poll::constants() {
            dict_set_str(&d, name, Value::Int(v as i64));
        }
    }
}
