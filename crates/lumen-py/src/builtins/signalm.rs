//! `_signal` on `lumen_os::signal`: the OS handler only records the signal; the interpreter runs
//! the Python handler at its next poll (`Interp::poll`), on the thread that owns signals.
//! Handlers are installed with `SA_RESTART` (unlike CPython, which retries `EINTR` at every
//! call site): blocking calls that must notice signals wait in slices instead.

use crate::object::*;
use crate::vm::Interp;
use std::sync::OnceLock;
use std::thread::ThreadId;

/// The thread whose interpreter runs Python signal handlers (CPython's main thread).
static OWNER: OnceLock<ThreadId> = OnceLock::new();

/// Makes this interpreter the one that handles signals, if its thread owns them.
pub fn claim(it: &mut Interp) -> bool {
    let me = std::thread::current().id();
    let owns = *OWNER.get_or_init(|| me) == me;
    if owns {
        it.handles_signals = true;
    }
    owns
}

/// CPython's startup handlers: Ctrl-C raises `KeyboardInterrupt` in the script.
pub fn install_default_handlers(it: &mut Interp) {
    use lumen_os::signal::{disposition, set_disposition, Disposition};
    const SIGINT: i32 = 2;
    if claim(it) && disposition(SIGINT) == Ok(Disposition::Default) {
        let _ = set_disposition(SIGINT, Disposition::Catch, true);
    }
}

/// `PyErr_CheckSignals`: runs the handlers of any signals caught so far.
pub fn check(it: &mut Interp) -> R<()> {
    if it.handles_signals && lumen_os::signal::any_pending() {
        return run_pending(it);
    }
    Ok(())
}

/// Runs the Python handlers of the signals caught since the last call.
pub fn run_pending(it: &mut Interp) -> R<()> {
    let sigs = lumen_os::signal::take_pending();
    for (i, &sig) in sigs.iter().enumerate() {
        let r = run_handler(it, sig);
        if let Err(e) = r {
            for &s in &sigs[i + 1..] {
                lumen_os::signal::repend(s);
            }
            return Err(e);
        }
    }
    Ok(())
}

/// A signal as if it had arrived (`_thread.interrupt_main`): ignored when its handler is
/// `SIG_DFL` or `SIG_IGN`, otherwise handled at the next poll.
pub fn simulate(it: &mut Interp, sig: i64) -> R<()> {
    if !(1..lumen_os::signal::NSIG as i64).contains(&sig) {
        return Err(it.value_error("signal number out of range"));
    }
    let handler = it.native_state::<_signal::State>().handlers.get(sig as usize).cloned();
    if matches!(handler, Some(Value::Int(_))) {
        return Ok(());
    }
    if it.handles_signals {
        lumen_os::signal::repend(sig as i32);
        return Ok(());
    }
    run_handler(it, sig as i32)
}

fn run_handler(it: &mut Interp, sig: i32) -> R<()> {
    let handler = it.native_state::<_signal::State>().handlers.get(sig as usize).cloned();
    match handler {
        None if sig == 2 => Err(it.interrupt_exc()),
        Some(h) if !matches!(h, Value::Int(_) | Value::None) => {
            let frame = match it.frames.len() {
                0 => Value::None,
                n => it.frame_object(n - 1),
            };
            it.call(&h, vec![Value::Int(sig as i64), frame], Vec::new()).map(|_| ())
        }
        _ => Ok(()),
    }
}

/// This module provides mechanisms to use signal handlers in Python.
///
/// Functions:
///
/// alarm() -- cause SIGALRM after a specified time [Unix only]
/// setitimer() -- cause a signal (described below) after a specified
///                float time and the timer may restart then [Unix only]
/// getitimer() -- get current value of timer [Unix only]
/// signal() -- set the action for a given signal
/// getsignal() -- get the signal action for a given signal
/// pause() -- wait until a signal arrives [Unix only]
/// default_int_handler() -- default SIGINT handler
///
/// signal constants:
/// SIG_DFL -- used to refer to the system default handler
/// SIG_IGN -- used to ignore the signal
/// NSIG -- number of defined signals
/// SIGINT, SIGTERM, etc. -- signal numbers
///
/// itimer constants:
/// ITIMER_REAL -- decrements in real time, and delivers SIGALRM upon
///                expiration
/// ITIMER_VIRTUAL -- decrements only when the process is executing,
///                and delivers SIGVTALRM upon expiration
/// ITIMER_PROF -- decrements both when the process is executing and
///                when the system is executing on behalf of the process.
///                Coupled with ITIMER_VIRTUAL, this timer is usually
///                used to profile the time spent by the application
///                in user and kernel space. SIGPROF is delivered upon
///                expiration.
///
///
/// *** IMPORTANT NOTICE ***
/// A signal handler function is called with two arguments:
/// the first is the signal number, the second is the interrupted stack frame.
#[lumen_bind::module(name = "_signal")]
pub mod _signal {
    use crate::bind::KwArgs;
    use crate::object::*;
    use crate::vm::{dict_get_str, dict_set_str, Interp};
    use lumen_os::signal::{self as os, Disposition, NSIG};

    const SIG_DFL: i64 = 0;
    const SIG_IGN: i64 = 1;

    /// The Python handler of each signal (`SIG_DFL`/`SIG_IGN` as ints, `None` when unknown);
    /// empty until the module is imported.
    #[derive(Default)]
    pub struct State {
        pub handlers: Vec<Value>,
        itimer_error: Option<Obj>,
    }

    fn os_error(it: &mut Interp, e: lumen_os::FsError) -> Obj {
        it.os_error_errno(e.errno(), None, None)
    }

    fn check_signum(it: &mut Interp, sig: i64) -> R<i32> {
        if !(1..NSIG as i64).contains(&sig) {
            return Err(it.value_error("signal number out of range"));
        }
        Ok(sig as i32)
    }

    fn signal_set(it: &mut Interp, v: &Value) -> R<Vec<i32>> {
        let items = it.iterate_to_vec(v)?;
        let mut out = Vec::with_capacity(items.len());
        for i in items {
            let n = it.index_of(&i)?;
            if !(1..NSIG as i64).contains(&n) {
                return Err(it.value_error(&format!("signal number {n} out of range [1; {}]", NSIG - 1)));
            }
            out.push(n as i32);
        }
        Ok(out)
    }

    fn to_set(it: &mut Interp, sigs: Vec<i32>) -> R<Value> {
        let list = Value::list(sigs.into_iter().map(|s| Value::Int(s as i64)).collect());
        let set = Value::Obj(it.types.set.clone());
        it.call(&set, vec![list], Vec::new())
    }

    fn main_thread_only(it: &mut Interp, what: &str) -> R<()> {
        if super::claim(it) {
            return Ok(());
        }
        Err(it.value_error(&format!("{what} only works in main thread of the main interpreter")))
    }

    /// The default handler for SIGINT installed by Python.
    ///
    /// It raises KeyboardInterrupt.
    #[op]
    fn default_int_handler(it: &mut Interp, signalnum: i64, frame: &Value) -> R<()> {
        let _ = (signalnum, frame);
        Err(it.interrupt_exc())
    }

    /// Arrange for SIGALRM to arrive after the given number of seconds.
    #[op]
    fn alarm(seconds: i64) -> i64 {
        os::alarm(seconds.clamp(0, u32::MAX as i64) as u32) as i64
    }

    /// Wait until a signal arrives.
    #[op]
    fn pause(it: &mut Interp) -> R<()> {
        it.flush_out();
        while !os::any_pending() {
            it.poll()?;
            it.platform.borrow_mut().sleep(0.02);
        }
        super::run_pending(it)
    }

    /// Send a signal to the executing process.
    #[op]
    fn raise_signal(it: &mut Interp, signalnum: i64) -> R<()> {
        let sig = check_signum(it, signalnum)?;
        os::raise(sig).map_err(|e| os_error(it, e))?;
        if it.handles_signals {
            super::run_pending(it)?;
        }
        Ok(())
    }

    /// Set the action for the given signal.
    ///
    /// The action can be SIG_DFL, SIG_IGN, or a callable Python object.
    /// The previous action is returned.  See getsignal() for possible return values.
    ///
    /// *** IMPORTANT NOTICE ***
    /// A signal handler function is called with two arguments:
    /// the first is the signal number, the second is the interrupted stack frame.
    #[op]
    fn signal(it: &mut Interp, signalnum: i64, handler: &Value) -> R<Value> {
        main_thread_only(it, "signal")?;
        let sig = check_signum(it, signalnum)?;
        let disposition = if it.is_callable(handler) {
            Disposition::Catch
        } else if is_int(handler, SIG_IGN) {
            Disposition::Ignore
        } else if is_int(handler, SIG_DFL) {
            Disposition::Default
        } else {
            return Err(it.type_error("signal handler must be signal.SIG_IGN, signal.SIG_DFL, or a callable object"));
        };
        if os::any_pending() {
            super::run_pending(it)?;
        }
        if !os::catchable(sig) {
            return Err(it.os_error_errno(22, None, None));
        }
        os::set_disposition(sig, disposition, true).map_err(|e| os_error(it, e))?;
        let st = it.native_state::<State>();
        let old = std::mem::replace(&mut st.handlers[sig as usize], handler.clone());
        Ok(old)
    }

    fn is_int(v: &Value, n: i64) -> bool {
        matches!(v, Value::Int(i) if *i == n)
    }

    /// Return the current action for the given signal.
    ///
    /// The return value can be:
    ///   SIG_IGN -- if the signal is being ignored
    ///   SIG_DFL -- if the default action for the signal is in effect
    ///   None    -- if an unknown handler is in effect
    ///   anything else -- the callable Python object used as a handler
    #[op]
    fn getsignal(it: &mut Interp, signalnum: i64) -> R<Value> {
        let sig = check_signum(it, signalnum)?;
        Ok(it.native_state::<State>().handlers.get(sig as usize).cloned().unwrap_or(Value::None))
    }

    /// Return the system description of the given signal.
    ///
    /// Returns the description of signal *signalnum*, such as "Interrupt"
    /// for :const:`SIGINT`. Returns :const:`None` if *signalnum* has no
    /// description. Raises :exc:`ValueError` if *signalnum* is invalid.
    #[op]
    fn strsignal(it: &mut Interp, signalnum: i64) -> R<Option<String>> {
        let sig = check_signum(it, signalnum)?;
        Ok(os::strsignal(sig).filter(|s| !s.contains("Unknown signal")))
    }

    /// Change system call restart behaviour.
    ///
    /// If flag is False, system calls will be restarted when interrupted by
    /// signal sig, else system calls will be interrupted.
    #[op]
    fn siginterrupt(it: &mut Interp, signalnum: i64, flag: i64) -> R<()> {
        let sig = check_signum(it, signalnum)?;
        os::set_restart(sig, flag == 0).map_err(|e| os_error(it, e))
    }

    /// set_wakeup_fd(fd, *, warn_on_full_buffer=True) -> fd
    ///
    /// Sets the fd to be written to (with the signal number) when a signal
    /// comes in.  A library can use this to wakeup select or poll.
    /// The previous fd or -1 is returned.
    ///
    /// The fd must be non-blocking.
    #[op(hint(py(text_signature = "")))]
    fn set_wakeup_fd(it: &mut Interp, fd: i64, #[varkw] kw: KwArgs) -> R<i64> {
        for (k, _) in kw.iter() {
            if k != "warn_on_full_buffer" {
                return Err(it.type_error(&format!("'{k}' is an invalid keyword argument for set_wakeup_fd()")));
            }
        }
        main_thread_only(it, "set_wakeup_fd")?;
        let fd = fd as i32;
        if fd != -1 {
            if lumen_os::fdctl::get_inheritable(fd).is_err() {
                return Err(it.os_error_errno(9, None, None));
            }
            if lumen_os::fdctl::get_blocking(fd).unwrap_or(true) {
                return Err(it.value_error(&format!("the fd {fd} must be in non-blocking mode")));
            }
        }
        Ok(os::set_wakeup_fd(os::WAKE_PYTHON, fd) as i64)
    }

    /// Sets given itimer (one of ITIMER_REAL, ITIMER_VIRTUAL or ITIMER_PROF).
    ///
    /// The timer will fire after value seconds and after that every interval seconds.
    /// The itimer can be cleared by setting seconds to zero.
    ///
    /// Returns old values as a tuple: (delay, interval).
    #[op]
    fn setitimer(it: &mut Interp, which: i64, seconds: &Value, interval: Option<&Value>) -> R<(f64, f64)> {
        let value = it.float_arg(seconds)?;
        let interval = match interval {
            Some(v) => it.float_arg(v)?,
            None => 0.0,
        };
        os::setitimer(which as i32, value, interval).map_err(|e| itimer_error(it, e))
    }

    /// Returns current value of given itimer.
    #[op]
    fn getitimer(it: &mut Interp, which: i64) -> R<(f64, f64)> {
        os::getitimer(which as i32).map_err(|e| itimer_error(it, e))
    }

    fn itimer_error(it: &mut Interp, e: lumen_os::FsError) -> Obj {
        let cls = it.native_state::<State>().itimer_error.clone();
        let errno = e.errno();
        match cls {
            Some(c) => {
                let msg = Value::string(lumen_os::errno::strerror(errno));
                it.os_error_of(&c, vec![Value::Int(errno as i64), msg])
            }
            None => it.os_error_errno(errno, None, None),
        }
    }

    /// Fetch and/or change the signal mask of the calling thread.
    #[op]
    fn pthread_sigmask(it: &mut Interp, how: i64, mask: &Value) -> R<Value> {
        let sigs = signal_set(it, mask)?;
        let old = os::sigmask(how as i32, &sigs).map_err(|e| os_error(it, e))?;
        if it.handles_signals && os::any_pending() {
            super::run_pending(it)?;
        }
        to_set(it, old)
    }

    /// Send a signal to a thread.
    #[op]
    fn pthread_kill(it: &mut Interp, thread_id: i64, signalnum: i64) -> R<()> {
        let sig = signalnum as i32;
        if thread_id != crate::builtins::threadm::_thread::MAIN_THREAD {
            return Err(it.os_error_errno(3, None, None));
        }
        os::raise(sig).map_err(|e| os_error(it, e))?;
        if it.handles_signals {
            super::run_pending(it)?;
        }
        Ok(())
    }

    /// Examine pending signals.
    ///
    /// Returns a set of signal numbers that are pending for delivery to
    /// the calling thread.
    #[op]
    fn sigpending(it: &mut Interp) -> R<Value> {
        let sigs = os::sigpending().map_err(|e| os_error(it, e))?;
        to_set(it, sigs)
    }

    /// Wait for a signal.
    ///
    /// Suspend execution of the calling thread until the delivery of one of the
    /// signals specified in the signal set sigset.  The function accepts the signal
    /// and returns the signal number.
    #[op]
    fn sigwait(it: &mut Interp, sigset: &Value) -> R<i64> {
        let sigs = signal_set(it, sigset)?;
        it.flush_out();
        os::sigwait(&sigs).map(i64::from).map_err(|e| os_error(it, e))
    }

    /// Return a set of valid signal numbers on this platform.
    ///
    /// The signal numbers returned by this function can be safely passed to
    /// functions like `pthread_sigmask`.
    #[op]
    fn valid_signals(it: &mut Interp) -> R<Value> {
        to_set(it, os::valid_signals())
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        for (name, v) in [("SIG_DFL", SIG_DFL), ("SIG_IGN", SIG_IGN), ("NSIG", NSIG as i64)] {
            dict_set_str(&d, name, Value::Int(v));
        }
        for &(name, v) in os::names().iter().chain(os::constants()) {
            dict_set_str(&d, name, Value::Int(v as i64));
        }
        let os_error = it.exc_type("OSError");
        let itimer = crate::builtins::native::new_type(it, "signal", "itimer_error", Some(&os_error), Layout::Exception);
        dict_set_str(&d, "ItimerError", Value::Obj(itimer.clone()));
        let int_handler = dict_get_str(&d, "default_int_handler").unwrap_or(Value::None);
        let mut handlers = vec![Value::None; NSIG as usize];
        for (sig, slot) in handlers.iter_mut().enumerate().skip(1) {
            *slot = match os::disposition(sig as i32) {
                Ok(Disposition::Default) => Value::Int(SIG_DFL),
                Ok(Disposition::Ignore) => Value::Int(SIG_IGN),
                Ok(Disposition::Catch) if sig == 2 => int_handler.clone(),
                _ => Value::None,
            };
        }
        let st = it.native_state::<State>();
        st.handlers = handlers;
        st.itimer_error = Some(itimer);
    }
}
