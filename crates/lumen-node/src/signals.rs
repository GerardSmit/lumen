//! Process signals as Node's `process.on('SIGUSR2', ...)` sees them: a listener installs the
//! shared `lumen_os::signal` handler, which writes the signal number to this runtime's
//! self-pipe; one watcher thread (started on the first listener) reads it and wakes every realm
//! listening for that signal through its event loop. Nothing is installed until a program
//! listens for a signal.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use lumen_host::{CompletionSender, Ctx, TaskId, TaskRegistry, Value};

pub(crate) use lumen_os::consts::signal_number as number;

/// `(name)` — the platform's number for signal `name`, or `undefined` if it is not one.
pub(crate) fn op_signal_number(_ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    Ok(match args.first() {
        Some(Value::Str(s)) => number(&s.to_string()).map_or(Value::Undefined, |n| Value::Num(n as f64)),
        _ => Value::Undefined,
    })
}

#[cfg(all(unix, not(target_arch = "wasm32")))]
mod imp {
    use super::*;
    use std::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Mutex, Once};

    static WRITE_FD: AtomicI32 = AtomicI32::new(-1);
    static START: Once = Once::new();
    static NEXT_KEY: AtomicU64 = AtomicU64::new(1);

    struct Listener {
        sig: i32,
        key: u64,
        sender: CompletionSender,
        task: TaskId,
    }

    /// Every realm's signal listeners, process-wide (a worker's realm has its own loop).
    static LISTENERS: Mutex<Vec<Listener>> = Mutex::new(Vec::new());

    struct Target {
        key: u64,
        raised: Arc<AtomicBool>,
        fired: Arc<AtomicBool>,
    }

    /// SIGINT watchdog state, as Node's SigintWatchdogHelper keeps it: while `depth > 0` a SIGINT
    /// breaks the registered runs instead of reaching `process.on('SIGINT')` listeners.
    struct Watch {
        depth: usize,
        pending: bool,
        targets: Vec<Target>,
    }

    static WATCH: Mutex<Watch> = Mutex::new(Watch { depth: 0, pending: false, targets: Vec::new() });
    static WATCH_DEPTH: AtomicUsize = AtomicUsize::new(0);

    fn start_watcher() -> bool {
        START.call_once(|| {
            use std::os::fd::IntoRawFd;
            let Ok((read, write)) = lumen_os::fdctl::os_pipe() else {
                return;
            };
            // The handler's write end must never block; both ends live for the process.
            let (read_fd, write_fd) = (read.into_raw_fd(), write.into_raw_fd());
            let _ = lumen_os::fdctl::set_blocking(write_fd, false);
            let spawned = std::thread::Builder::new()
                .name("lumen-signals".into())
                .stack_size(64 * 1024)
                .spawn(move || {
                    let mut buf = [0u8; 64];
                    loop {
                        // SAFETY: reading into a local buffer from the pipe this thread owns.
                        let n = unsafe {
                            libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
                        };
                        if n < 0 {
                            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                                continue;
                            }
                            return;
                        }
                        if n == 0 {
                            return;
                        }
                        for &sig in &buf[..n as usize] {
                            if sig as i32 == libc::SIGINT && break_runs() {
                                continue;
                            }
                            let listeners = LISTENERS.lock().unwrap();
                            for l in listeners.iter().filter(|l| l.sig == sig as i32) {
                                l.sender.send(l.task, Box::new(sig as i32));
                            }
                        }
                    }
                });
            if spawned.is_ok() {
                WRITE_FD.store(write_fd, Ordering::Relaxed);
                lumen_os::signal::set_wakeup_fd(lumen_os::signal::WAKE_NODE, write_fd);
            }
        });
        WRITE_FD.load(Ordering::Relaxed) >= 0
    }

    /// Break every registered run when the watchdog is active; whether it was.
    fn break_runs() -> bool {
        let mut watch = WATCH.lock().unwrap();
        if watch.depth == 0 {
            return false;
        }
        watch.pending = true;
        for t in &watch.targets {
            t.fired.store(true, Ordering::SeqCst);
            t.raised.store(true, Ordering::SeqCst);
        }
        true
    }

    fn set_handler(sig: i32, install: bool) -> bool {
        use lumen_os::signal::{set_disposition, Disposition};
        set_disposition(sig, if install { Disposition::Catch } else { Disposition::Default }, true).is_ok()
    }

    pub(super) fn catchable(sig: i32) -> bool {
        lumen_os::signal::catchable(sig)
    }

    /// Start delivering `sig` to `task` on `sender`'s loop; the key identifies the watch.
    pub(super) fn watch(sig: i32, sender: CompletionSender, task: TaskId) -> Option<u64> {
        if !start_watcher() {
            return None;
        }
        let mut listeners = LISTENERS.lock().unwrap();
        if !listeners.iter().any(|l| l.sig == sig) && !set_handler(sig, true) {
            return None;
        }
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        listeners.push(Listener { sig, key, sender, task });
        Some(key)
    }

    pub(super) fn watchdog_start() -> bool {
        if !start_watcher() {
            return false;
        }
        let mut watch = WATCH.lock().unwrap();
        watch.depth += 1;
        WATCH_DEPTH.store(watch.depth, Ordering::SeqCst);
        if watch.depth == 1 {
            set_handler(libc::SIGINT, true);
        }
        true
    }

    /// Leave one watchdog level; whether a SIGINT arrived since the last stop.
    pub(super) fn watchdog_stop() -> bool {
        let mut watch = WATCH.lock().unwrap();
        let had = watch.pending;
        watch.pending = false;
        if watch.depth > 0 {
            watch.depth -= 1;
            WATCH_DEPTH.store(watch.depth, Ordering::SeqCst);
            if watch.depth == 0 && !LISTENERS.lock().unwrap().iter().any(|l| l.sig == libc::SIGINT) {
                set_handler(libc::SIGINT, false);
            }
        }
        had
    }

    pub(super) fn watchdog_pending() -> bool {
        WATCH.lock().unwrap().pending
    }

    pub(super) fn break_register(raised: Arc<AtomicBool>, fired: Arc<AtomicBool>) -> u64 {
        let key = NEXT_KEY.fetch_add(1, Ordering::Relaxed);
        WATCH.lock().unwrap().targets.push(Target { key, raised, fired });
        key
    }

    pub(super) fn break_unregister(key: u64) {
        WATCH.lock().unwrap().targets.retain(|t| t.key != key);
    }

    /// Raise `sig` on the calling thread when this process handles it: the handler then runs
    /// before the call returns, so signals a program sends itself are neither delayed nor merged
    /// with an identical pending one (as a process-directed `kill` landing on another thread
    /// may be).
    pub(super) fn raise_watched(sig: i32) -> bool {
        let watchdog = sig == libc::SIGINT && WATCH_DEPTH.load(Ordering::SeqCst) > 0;
        if !watchdog && !LISTENERS.lock().unwrap().iter().any(|l| l.sig == sig) {
            return false;
        }
        // SAFETY: raise(3) takes no pointers.
        unsafe { libc::raise(sig) == 0 }
    }

    /// Stop a watch, restoring the default disposition once nothing listens for its signal.
    /// Returns the watch's task.
    pub(super) fn unwatch(key: u64) -> Option<TaskId> {
        let mut listeners = LISTENERS.lock().unwrap();
        let i = listeners.iter().position(|l| l.key == key)?;
        let l = listeners.remove(i);
        let watched = l.sig == libc::SIGINT && WATCH_DEPTH.load(Ordering::SeqCst) > 0;
        if !watched && !listeners.iter().any(|o| o.sig == l.sig) {
            set_handler(l.sig, false);
        }
        Some(l.task)
    }
}

fn decode_signal(_ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    let sig = payload.downcast::<i32>().map(|b| *b).unwrap_or(0);
    Ok(vec![Value::Num(sig as f64)])
}

/// `(name, callback)` — call `callback(signum)` on the event loop whenever the process receives
/// signal `name`, without keeping the loop alive. Returns a watch key for `signalUnwatch`
/// (`undefined` where signals cannot be watched), or throws a uv-style error for a signal that
/// cannot be caught.
pub(crate) fn op_signal_watch(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let Some(sig) = (match args.first() {
        Some(Value::Str(s)) => number(&s.to_string()),
        _ => None,
    }) else {
        return Ok(Value::Undefined);
    };
    let Some(callback) = args.get(1).filter(|c| c.is_callable()).cloned() else {
        return Err(ctx.make_error("TypeError", "signalWatch expects a callback"));
    };
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        let Some(sender) = ctx.op_state().get::<CompletionSender>().cloned() else {
            return Ok(Value::Undefined);
        };
        if imp::catchable(sig) {
            let registry = ctx.host_mut::<TaskRegistry>().expect("runtime task registry");
            let task = registry.register_stream(callback, decode_signal);
            registry.set_unref(task);
            if let Some(key) = imp::watch(sig, sender, task) {
                return Ok(Value::Num(key as f64));
            }
            ctx.host_mut::<TaskRegistry>().unwrap().cancel(task);
        }
        let err = ctx.make_error("Error", "uv_signal_start EINVAL");
        let _ = ctx.set_member(&err, "code", Value::str("EINVAL"));
        let _ = ctx.set_member(&err, "errno", Value::Num(-(libc::EINVAL as f64)));
        let _ = ctx.set_member(&err, "syscall", Value::str("uv_signal_start"));
        Err(err)
    }
    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    {
        let _ = (sig, callback);
        Ok(Value::Undefined)
    }
}

/// `(key)` — stop a `signalWatch`.
pub(crate) fn op_signal_unwatch(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    if let Some(Value::Num(key)) = args.first() {
        if let Some(task) = imp::unwatch(*key as u64) {
            if let Some(registry) = ctx.host_mut::<TaskRegistry>() {
                registry.cancel(task);
            }
        }
    }
    let _ = (ctx, args);
    Ok(Value::Undefined)
}

/// `(signum)` — deliver `signum` to this process synchronously if it has a listener for it
/// (see `imp::raise_watched`); `false` when it has none and the caller should `kill(2)`.
pub(crate) fn op_signal_raise(_ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    if let Some(Value::Num(sig)) = args.first() {
        return Ok(Value::Bool(imp::raise_watched(*sig as i32)));
    }
    let _ = args;
    Ok(Value::Bool(false))
}

/// A run that a SIGINT breaks (`vm` `breakOnSigint`, the REPL's evaluation): while the guard
/// lives, SIGINT raises `raised` instead of reaching `process.on('SIGINT')` listeners or killing
/// the process, and `fired` tells the run afterwards that it was the signal that stopped it.
pub struct SigintBreak {
    fired: Arc<AtomicBool>,
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    key: Option<u64>,
}

impl SigintBreak {
    pub fn new(raised: Arc<AtomicBool>) -> SigintBreak {
        let fired = Arc::new(AtomicBool::new(false));
        #[cfg(all(unix, not(target_arch = "wasm32")))]
        {
            let key = imp::watchdog_start().then(|| imp::break_register(raised, Arc::clone(&fired)));
            SigintBreak { fired, key }
        }
        #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
        {
            let _ = raised;
            SigintBreak { fired }
        }
    }

    pub fn fired_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.fired)
    }

    pub fn fired(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for SigintBreak {
    fn drop(&mut self) {
        #[cfg(all(unix, not(target_arch = "wasm32")))]
        if let Some(key) = self.key {
            imp::break_unregister(key);
            imp::watchdog_stop();
        }
    }
}

/// `()` — Node's `startSigintWatchdog`.
pub(crate) fn op_sigint_watchdog_start(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    imp::watchdog_start();
    Ok(Value::Undefined)
}

/// `()` — Node's `stopSigintWatchdog`: whether a SIGINT arrived while it was watching.
pub(crate) fn op_sigint_watchdog_stop(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    return Ok(Value::Bool(imp::watchdog_stop()));
    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    Ok(Value::Bool(false))
}

/// `()` — Node's `watchdogHasPendingSigint`.
pub(crate) fn op_sigint_watchdog_pending(_ctx: &mut Ctx, _this: Value, _args: &[Value]) -> Result<Value, Value> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    return Ok(Value::Bool(imp::watchdog_pending()));
    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    Ok(Value::Bool(false))
}
