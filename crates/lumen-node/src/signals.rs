//! Process signals as Node's `process.on('SIGUSR2', ...)` sees them: a listener installs an
//! OS handler that writes the signal number to a self-pipe; one watcher thread (started on the
//! first listener) reads it and wakes every realm listening for that signal through its event
//! loop. Nothing is installed until a program listens for a signal.

use lumen_host::{CompletionSender, Ctx, TaskId, TaskRegistry, Value};

/// The signal number for a Node signal name (`"SIGTERM"`), on this platform.
pub(crate) fn number(name: &str) -> Option<i32> {
    #[cfg(unix)]
    {
        let n = match name {
            "SIGHUP" => libc::SIGHUP,
            "SIGINT" => libc::SIGINT,
            "SIGQUIT" => libc::SIGQUIT,
            "SIGILL" => libc::SIGILL,
            "SIGTRAP" => libc::SIGTRAP,
            "SIGABRT" => libc::SIGABRT,
            "SIGIOT" => libc::SIGIOT,
            "SIGBUS" => libc::SIGBUS,
            "SIGFPE" => libc::SIGFPE,
            "SIGKILL" => libc::SIGKILL,
            "SIGUSR1" => libc::SIGUSR1,
            "SIGSEGV" => libc::SIGSEGV,
            "SIGUSR2" => libc::SIGUSR2,
            "SIGPIPE" => libc::SIGPIPE,
            "SIGALRM" => libc::SIGALRM,
            "SIGTERM" => libc::SIGTERM,
            "SIGCHLD" => libc::SIGCHLD,
            "SIGCONT" => libc::SIGCONT,
            "SIGSTOP" => libc::SIGSTOP,
            "SIGTSTP" => libc::SIGTSTP,
            "SIGTTIN" => libc::SIGTTIN,
            "SIGTTOU" => libc::SIGTTOU,
            "SIGURG" => libc::SIGURG,
            "SIGXCPU" => libc::SIGXCPU,
            "SIGXFSZ" => libc::SIGXFSZ,
            "SIGVTALRM" => libc::SIGVTALRM,
            "SIGPROF" => libc::SIGPROF,
            "SIGWINCH" => libc::SIGWINCH,
            "SIGIO" => libc::SIGIO,
            "SIGSYS" => libc::SIGSYS,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            "SIGSTKFLT" => libc::SIGSTKFLT,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            "SIGPOLL" => libc::SIGPOLL,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            "SIGPWR" => libc::SIGPWR,
            #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
            "SIGINFO" => libc::SIGINFO,
            _ => return None,
        };
        Some(n)
    }
    #[cfg(not(unix))]
    {
        Some(match name {
            "SIGHUP" => 1,
            "SIGINT" => 2,
            "SIGILL" => 4,
            "SIGABRT" => 22,
            "SIGFPE" => 8,
            "SIGKILL" => 9,
            "SIGSEGV" => 11,
            "SIGTERM" => 15,
            "SIGBREAK" => 21,
            "SIGWINCH" => 28,
            _ => return None,
        })
    }
}

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
    use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
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

    extern "C" fn on_signal(sig: libc::c_int) {
        let fd = WRITE_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            // SAFETY: write(2) and errno access are async-signal-safe; errno is restored so the
            // interrupted code does not observe the handler's.
            unsafe {
                let errno = errno_ptr();
                let saved = *errno;
                let byte = sig as u8;
                libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
                *errno = saved;
            }
        }
    }

    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    unsafe fn errno_ptr() -> *mut libc::c_int {
        libc::__error()
    }
    #[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "freebsd")))]
    unsafe fn errno_ptr() -> *mut libc::c_int {
        libc::__errno_location()
    }

    fn start_watcher() -> bool {
        START.call_once(|| {
            let mut fds = [0 as libc::c_int; 2];
            // SAFETY: `fds` is a valid two-int buffer.
            if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
                return;
            }
            // SAFETY: plain fcntl on descriptors this call owns.
            unsafe {
                for fd in fds {
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
                let flags = libc::fcntl(fds[1], libc::F_GETFL);
                libc::fcntl(fds[1], libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
            let read_fd = fds[0];
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
                        let listeners = LISTENERS.lock().unwrap();
                        for &sig in &buf[..n as usize] {
                            for l in listeners.iter().filter(|l| l.sig == sig as i32) {
                                l.sender.send(l.task, Box::new(sig as i32));
                            }
                        }
                    }
                });
            if spawned.is_ok() {
                WRITE_FD.store(fds[1], Ordering::Relaxed);
            }
        });
        WRITE_FD.load(Ordering::Relaxed) >= 0
    }

    fn set_handler(sig: i32, install: bool) -> bool {
        // SAFETY: a zeroed sigaction with a valid handler and an empty mask.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            let handler: extern "C" fn(libc::c_int) = on_signal;
            action.sa_sigaction = if install { handler as usize } else { libc::SIG_DFL };
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut()) == 0
        }
    }

    pub(super) fn catchable(sig: i32) -> bool {
        sig != libc::SIGKILL && sig != libc::SIGSTOP
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

    /// Raise `sig` on the calling thread when this process handles it: the handler then runs
    /// before the call returns, so signals a program sends itself are neither delayed nor merged
    /// with an identical pending one (as a process-directed `kill` landing on another thread
    /// may be).
    pub(super) fn raise_watched(sig: i32) -> bool {
        if !LISTENERS.lock().unwrap().iter().any(|l| l.sig == sig) {
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
        if !listeners.iter().any(|o| o.sig == l.sig) {
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
