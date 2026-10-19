//! Minimal `process`: argv/env/platform snapshots, `cwd()`, `exit()`, `nextTick()`, `hrtime()`,
//! and `stdout`/`stderr` writable streams. Enough for scripts to orient themselves and for the
//! Node ecosystem (morgan et al.) to log and time; the fuller `node:process` surface is layered
//! on in lumen-node.

use lumen_host::time::Instant;
use std::io::{Read, Write};

use lumen::embed::{JsFunction, OpError};
use lumen_bind::NativeError;
use lumen_host::{
    CompletionSender, Ctx, Engine, Extension, OpState, RealmProcess, TaskId, TaskRegistry, Value,
};

use crate::console::ConsoleOut;

/// Process-start monotonic reference for `hrtime()`.
struct ProcStart(Instant);

pub(crate) fn extension() -> Extension {
    Extension {
        name: "process",
        modules: &[
            lumen_host::namespace::<process_ns::Module>,
            lumen_host::namespace::<proc_internal::Module>,
        ],
        state_init: Some(|state: &mut OpState| state.put(ProcStart(Instant::now()))),
        js_init: None,
        js_init_snapshot: Some(JS_INIT_AOT),
        lazy_globals: &[],
    }
}

/// js/process.js, precompiled by build.rs: shapes `process.stdout`/`stderr` (over the raw write ops) and `process.hrtime` (over the
/// monotonic op), and stamps `version`/`versions`. We report a Node version string because the
/// ecosystem branches on it for feature detection; `versions.lumen` records the real engine.
const JS_INIT_AOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/process.aot"));

/// The data properties (`argv`, `env`, `platform`) — snapshots taken at startup, like Node's.
/// Runs after `install` because it needs the `process` object that install created. An embedded
/// realm passes its own `(argv, env)`; otherwise they come from the OS process.
pub(crate) fn install_data_props(
    engine: &mut Engine,
    embedded: Option<(&[String], &[(String, String)])>,
) {
    let global = engine.global_this();
    let ctx = engine.ctx();
    let process = match ctx.get_member(&global, "process") {
        Ok(v @ Value::Obj(_)) => v,
        _ => unreachable!("install() defined the process namespace"),
    };

    let (args, vars): (Vec<String>, Vec<(String, String)>) = match embedded {
        Some((argv, env)) => (argv.to_vec(), env.to_vec()),
        None => (
            startup_args().to_vec(),
            std::env::vars()
                .filter(|(key, _)| public_environment_key(key))
                .collect(),
        ),
    };
    let argv0_str = args.first().cloned().unwrap_or_else(|| "lumen".to_string());
    let argv: Vec<Value> = args.into_iter().map(Value::from_string).collect();
    let argv = ctx.make_array(argv);
    let _ = ctx.set_member(&process, "argv", argv);

    // argv0/execPath mirror argv[0] (the running binary), like Node. These are set here rather
    // than in the JS glue because the glue runs before this data-prop pass, so argv isn't ready
    // there yet. `title` defaults to the executable's basename (settable afterwards).
    let _ = ctx.set_member(&process, "argv0", Value::from_string(argv0_str.clone()));
    // Node's execPath is always the absolute path of the running binary (uv_exepath), even when
    // it was started through a relative path; child_process.fork/spawn(process.execPath) rely on it.
    let exec_path = match embedded {
        Some(_) => argv0_str.clone(),
        None => std::env::current_exe()
            .map(|p| p.canonicalize().unwrap_or(p).to_string_lossy().into_owned())
            .unwrap_or_else(|_| argv0_str.clone()),
    };
    let _ = ctx.set_member(&process, "execPath", Value::from_string(exec_path));

    crate::process_env::replace(ctx, vars, cfg!(windows)).unwrap_or_else(|_| {
        panic!("invalid startup environment (invalid key/value or realm limit exceeded)")
    });

    #[cfg(target_arch = "wasm32")]
    let (os_name, arch_name) = ("linux", "wasm32");
    #[cfg(not(target_arch = "wasm32"))]
    let (os_name, arch_name) = (std::env::consts::OS, std::env::consts::ARCH);
    let platform = match os_name {
        // Node's names for them; programs branch on `process.platform === "win32"`.
        "macos" => "darwin",
        "windows" => "win32",
        other => other,
    };
    let _ = ctx.set_member(&process, "platform", Value::str(platform));

    let _ = ctx.set_member(
        &process,
        "pid",
        Value::Num(lumen_host::sysfs::process_id() as f64),
    );
    // Node's architecture names, not Rust's (native addons resolve their platform binary by these).
    let arch = match arch_name {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        "x86" => "ia32",
        other => other,
    };
    let _ = ctx.set_member(&process, "arch", Value::str(arch));

    // Node's process.env stores every value as a string, and on Windows looks names up without
    // regard to case (`process.env.windir` finds `WINDIR`).
    let _ = crate::run_aot(engine, ENV_PROXY_AOT);
}

/// js/env_proxy.js, precompiled by build.rs.
const ENV_PROXY_AOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/env_proxy.aot"));

fn public_environment_key(key: &str) -> bool {
    // Windows stores per-drive working directories under hidden names such as
    // `=C:`. These are OS bookkeeping, not process.env keys.
    !cfg!(windows) || !key.starts_with('=')
}

#[cfg(test)]
mod startup_environment_tests {
    #[test]
    fn windows_drive_state_is_not_a_public_environment_variable() {
        assert!(super::public_environment_key("PATH"));
        assert!(super::public_environment_key("LUMEN_TEST"));
        assert_eq!(super::public_environment_key("=C:"), !cfg!(windows));
    }
}

fn decode_stdin_read(
    ctx: &mut Ctx,
    payload: Box<dyn std::any::Any + Send>,
) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<Vec<u8>, String>>()
        .expect("stdin read payload")
    {
        Ok(bytes) if bytes.is_empty() => Ok(vec![Value::Null]),
        Ok(bytes) => Ok(vec![ctx.make_uint8array(&bytes)?]),
        Err(message) => Err(OpError::from(NativeError::runtime(message)).to_value(ctx)),
    }
}

#[cfg(windows)]
fn terminal_size(fd: i32) -> Option<(u16, u16)> {
    #[repr(C)]
    #[derive(Default)]
    struct Info {
        size: [i16; 2],
        cursor: [i16; 2],
        attrs: u16,
        window: [i16; 4],
        max: [i16; 2],
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetStdHandle(which: u32) -> *mut std::ffi::c_void;
        fn GetConsoleScreenBufferInfo(h: *mut std::ffi::c_void, info: *mut Info) -> i32;
    }
    let which = if fd == 2 {
        -12i32 as u32
    } else {
        -11i32 as u32
    };
    let mut info = Info::default();
    // SAFETY: GetStdHandle takes no pointers; `info` is a CONSOLE_SCREEN_BUFFER_INFO.
    let ok = unsafe { GetConsoleScreenBufferInfo(GetStdHandle(which), &mut info) };
    if ok == 0 {
        return None;
    }
    let cols = info.window[2] - info.window[0] + 1;
    let rows = info.window[3] - info.window[1] + 1;
    Some((cols.max(1) as u16, rows.max(1) as u16))
}

#[cfg(unix)]
fn terminal_size(fd: i32) -> Option<(u16, u16)> {
    match lumen_os::proc::terminal_size(fd) {
        Ok((cols, rows)) if cols > 0 => Some((cols as u16, rows as u16)),
        _ => None,
    }
}

#[cfg(not(any(unix, windows)))]
fn terminal_size(_fd: i32) -> Option<(u16, u16)> {
    None
}

fn write_raw(ctx: &mut Ctx, arg: &Value, to_err: bool) -> Result<(), OpError> {
    let bytes = match ctx.typed_array_bytes(arg) {
        Some(b) => b,
        // Lone surrogates as U+FFFD, like Node's utf8 (see `lumen_host::well_formed_utf8`).
        None => lumen_host::well_formed_utf8(&ctx.coerce_string(arg)?)
            .as_bytes()
            .to_vec(),
    };
    let sinks = ctx
        .host_mut::<ConsoleOut>()
        .expect("console state installed");
    let sink = if to_err {
        &mut sinks.err
    } else {
        &mut sinks.out
    };
    let written = sink.write_all(&bytes).and_then(|()| sink.flush());
    // The reader went away: report it as Node's `write EPIPE`, so process.stdout emits 'error'
    // (a program writing in a loop would otherwise spin forever into a closed pipe). Other
    // failures stay silent, as console's do.
    if let Err(e) = written {
        if e.kind() == std::io::ErrorKind::BrokenPipe {
            return Err(NativeError::runtime("write EPIPE")
                .with_prop("code", "EPIPE")
                .with_prop("syscall", "write")
                .into());
        }
    }
    Ok(())
}

/// The OS process's argv as it was at startup. Setting `process.title` reuses argv's memory (as
/// libuv does), after which the live argv no longer reads back the arguments.
fn startup_args() -> &'static [String] {
    static ARGS: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    ARGS.get_or_init(|| std::env::args().collect())
}

#[cfg(target_os = "macos")]
fn set_os_title(title: &str) {
    use std::os::raw::{c_char, c_int};
    extern "C" {
        fn _NSGetArgv() -> *mut *mut *mut c_char;
        fn _NSGetArgc() -> *mut c_int;
    }
    // SAFETY: the argv strings are the process's own, NUL-terminated, and laid out back to back;
    // only the contiguous run starting at argv[0] is overwritten, within its original length.
    unsafe {
        let argc = *_NSGetArgc();
        let argv = *_NSGetArgv();
        if argc <= 0 || argv.is_null() || (*argv).is_null() {
            return;
        }
        let start = *argv;
        let mut end = start.add(std::ffi::CStr::from_ptr(start).to_bytes().len());
        for i in 1..argc as usize {
            let arg = *argv.add(i);
            if arg != end.add(1) {
                break;
            }
            end = arg.add(std::ffi::CStr::from_ptr(arg).to_bytes().len());
        }
        overwrite_args(start as *mut u8, end.offset_from(start) as usize, title);
    }
}

#[cfg(target_os = "linux")]
fn set_os_title(title: &str) {
    // /proc/self/stat fields 48 and 49 (after the parenthesized comm): the argv area's bounds.
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return;
    };
    let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
        return;
    };
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let (Some(start), Some(end)) = (
        fields.get(45).and_then(|f| f.parse::<usize>().ok()),
        fields.get(46).and_then(|f| f.parse::<usize>().ok()),
    ) else {
        return;
    };
    if start == 0 || end <= start {
        return;
    }
    // SAFETY: [arg_start, arg_end) is this process's own argv area, mapped and writable; the
    // write stays inside it.
    unsafe { overwrite_args(start as *mut u8, end - start - 1, title) };
    let mut name = [0u8; 16];
    let n = title.len().min(15);
    name[..n].copy_from_slice(&title.as_bytes()[..n]);
    extern "C" {
        fn prctl(option: std::os::raw::c_int, ...) -> std::os::raw::c_int;
    }
    // PR_SET_NAME
    unsafe { prctl(15, name.as_ptr()) };
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn set_os_title(_title: &str) {}

/// Write `title` over `cap` bytes at `start` (NUL-padded, plus the terminating NUL at
/// `start + cap`).
#[cfg(any(target_os = "macos", target_os = "linux"))]
unsafe fn overwrite_args(start: *mut u8, cap: usize, title: &str) {
    let n = title.len().min(cap);
    std::ptr::copy_nonoverlapping(title.as_ptr(), start, n);
    std::ptr::write_bytes(start.add(n), 0, cap - n + 1);
}

/// In an embedded realm, `process.exit` / `process.abort` end the realm, not the host: record the
/// code, raise the interrupt, and throw so the calling code unwinds now — the engine rethrows at
/// every safe point after this, so a `catch` around the call cannot keep the program running.
fn end_realm_or_process(ctx: &mut Ctx, code: i32) -> Result<(), NativeError> {
    let Some(realm) = ctx.host_mut::<RealmProcess>() else {
        if lumen::memstats::enabled() {
            ctx.mem_report();
        }
        std::process::exit(code);
    };
    realm.exit_code.get_or_insert(code);
    realm
        .interrupt
        .store(true, std::sync::atomic::Ordering::SeqCst);
    ctx.terminate_for_host();
    Err(NativeError::runtime(format!("process.exit({code})")))
}

/// Ops that change what the whole OS process is (its identity, its image, its signal handling)
/// cannot be granted to a realm that shares a process with its host.
fn refuse_in_realm(ctx: &mut Ctx, what: &str) -> Result<(), NativeError> {
    if ctx.op_state().get::<RealmProcess>().is_some() {
        return Err(NativeError::runtime(format!(
            "{what} is not available to a program embedded in a host process"
        ))
        .with_code("ERR_FEATURE_UNAVAILABLE_ON_PLATFORM"));
    }
    Ok(())
}

/// Node's nextTick queue: drained at every tick checkpoint (after the main script, and after
/// each loop callback), ahead of the promise microtasks, until both are empty — see
/// `Runtime::checkpoint`.
#[derive(Default)]
pub(crate) struct TickQueue {
    pub(crate) queue: std::collections::VecDeque<(Value, Vec<Value>)>,
}

/// libuv's errno for `code` on this platform.
fn uv_errno(code: &str) -> i32 {
    lumen_os::uv::errno(code).unwrap_or(-4094)
}

/// Node's `kill` failure: `Error: kill ESRCH` with `code`, `errno` and `syscall`.
fn kill_error(code: &str, errno: i32) -> NativeError {
    NativeError::runtime(format!("kill {code}"))
        .with_prop("code", code)
        .with_prop("errno", errno)
        .with_prop("syscall", "kill")
}

/// `process.kill` of a child realm's stand-in pid: delivered by the realm that launched it.
/// `None` for any other pid.
fn kill_child_realm(ctx: &mut Ctx, pid: i32, signal: i32) -> Option<Result<(), NativeError>> {
    if pid < lumen_host::REALM_PID_BASE as i32 {
        return None;
    }
    let launcher = ctx
        .op_state()
        .get::<RealmProcess>()
        .and_then(|realm| realm.launcher.clone())?;
    Some(if launcher.signal_pid(pid as u32, signal) {
        Ok(())
    } else {
        Err(kill_error("ESRCH", uv_errno("ESRCH")))
    })
}

/// Node's error for a failed identity change: `Error: EPERM, Operation not permitted` with
/// `code`, `errno` and `syscall`.

#[lumen_bind::module(name = "process")]
mod process_ns {
    use super::*;

    #[op(name = "cwd")]
    fn op_cwd(ctx: &mut Ctx) -> Result<String, NativeError> {
        if let Some(realm) = ctx.op_state().get::<RealmProcess>() {
            return Ok(realm.cwd.to_string_lossy().into_owned());
        }
        match std::env::current_dir() {
            Ok(p) => Ok(p.to_string_lossy().into_owned()),
            Err(e) => Err(NativeError::runtime(format!("cwd unavailable: {e}"))),
        }
    }

    #[op(name = "exit", coerce)]
    fn op_exit(ctx: &mut Ctx, code: Option<f64>) -> Result<(), NativeError> {
        end_realm_or_process(ctx, code.unwrap_or(0.0) as i32)
    }

    #[op(name = "nextTick")]
    fn op_next_tick(ctx: &mut Ctx, callback: JsFunction, #[varargs] extra: &[Value]) {
        let state = ctx.op_state();
        if !state.has::<TickQueue>() {
            state.put(TickQueue::default());
        }
        state
            .get_mut::<TickQueue>()
            .expect("just installed")
            .queue
            .push_back((callback.into_value(), extra.to_vec()));
    }
}

/// Internal primitives the js_init wraps into process.stdout/stderr/hrtime, then deletes the
/// namespace (the capture-and-delete pattern the op crates use).
#[lumen_bind::module(name = "__proc")]
mod proc_internal {
    use super::*;

    /// `process._tickCallback()` — run the nextTick queue and the promise microtasks to quiescence
    /// now, as Node does when a native-to-JS callback returns. A throwing tick unwinds to the
    /// caller; the rest of the queue waits for the next checkpoint.
    #[op(name = "tickCallback")]
    fn op_tick_callback(ctx: &mut Ctx) -> Result<(), OpError> {
        loop {
            loop {
                let next = ctx
                    .op_state()
                    .get_mut::<TickQueue>()
                    .and_then(|q| q.queue.pop_front());
                let Some((callback, args)) = next else {
                    break;
                };
                ctx.invoke(callback, Value::Undefined, &args)?;
            }
            ctx.drain_microtasks_for_host();
            let more = ctx
                .op_state()
                .get::<TickQueue>()
                .is_some_and(|q| !q.queue.is_empty());
            if !more {
                return Ok(());
            }
        }
    }

    /// `(chunk)` — write raw bytes to stdout (no trailing newline, unlike `console.log`). A typed
    /// array is written as-is; anything else is coerced to a string. Backs `process.stdout.write`.
    #[op(name = "writeStdout")]
    fn op_write_stdout(ctx: &mut Ctx, chunk: &Value) -> Result<(), OpError> {
        write_raw(ctx, chunk, false)
    }

    #[op(name = "writeStderr")]
    fn op_write_stderr(ctx: &mut Ctx, chunk: &Value) -> Result<(), OpError> {
        write_raw(ctx, chunk, true)
    }

    /// `(resolve, reject)` — read one chunk from the process's stdin without blocking the loop.
    #[op(name = "readStdin")]
    fn op_read_stdin(ctx: &mut Ctx, resolve: JsFunction, reject: JsFunction) -> f64 {
        let id = lumen_host::register_task(
            ctx,
            resolve.into_value(),
            Some(reject.into_value()),
            decode_stdin_read,
        );
        let sender = ctx
            .op_state()
            .get::<CompletionSender>()
            .expect("runtime installs completion sender")
            .clone();
        // An embedded realm reads the stream its host gave it, never the host's own stdin.
        let source = ctx
            .op_state()
            .get::<RealmProcess>()
            .map(|realm| std::sync::Arc::clone(&realm.stdin));
        sender.run_blocking(id, move || {
            let mut buf = vec![0u8; 65_536];
            let read = match &source {
                Some(source) => source
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .read(&mut buf),
                None => std::io::stdin().read(&mut buf),
            };
            let result = read
                .map(|n| {
                    buf.truncate(n);
                    buf
                })
                .map_err(|e| format!("stdin read: {e}"));
            Box::new(result)
        });
        id as f64
    }

    /// `(taskId, ref)` — whether the pending stdin read returned by `readStdin` keeps the loop alive.
    /// A paused or destroyed `process.stdin` must not: the OS read cannot be cancelled, so it stays
    /// pending until the pipe closes, and Node's semantics (a paused stdin lets the process exit) are
    /// recovered by unref'ing it.
    #[op(name = "stdinRef", coerce)]
    fn op_stdin_ref(ctx: &mut Ctx, id: &Value, keep: Option<bool>) {
        let Value::Num(n) = id else { return };
        let id = *n as TaskId;
        let keep = keep.unwrap_or(true);
        let reg = ctx
            .host_mut::<TaskRegistry>()
            .expect("runtime installs task registry");
        if keep {
            reg.set_ref(id);
        } else {
            reg.set_unref(id);
        }
    }

    /// `[seconds, nanoseconds]` elapsed since process start, from a monotonic clock (Node's
    /// `process.hrtime()` contract). The js_init wrapper handles the optional `prev` diff + `.bigint`.
    #[op(name = "hrtime")]
    fn op_hrtime(ctx: &mut Ctx) -> Vec<f64> {
        let start = ctx
            .host_mut::<ProcStart>()
            .expect("process start installed")
            .0;
        let e = start.elapsed();
        vec![e.as_secs() as f64, e.subsec_nanos() as f64]
    }

    /// `(dir)` — change the process working directory (`std::env::set_current_dir`). Cross-platform,
    /// no FFI. Throws with the OS error on failure (matching `process.chdir`).
    #[op(name = "chdir", coerce)]
    fn op_chdir(ctx: &mut Ctx, dir: String) -> Result<(), NativeError> {
        if let Some(realm) = ctx.host_mut::<RealmProcess>() {
            let target = realm.resolve(&dir);
            return match lumen_host::canonicalize(&target) {
                Ok(path) if lumen_host::sysfs::is_dir(&path) => {
                    realm.cwd = path;
                    Ok(())
                }
                Ok(_) => Err(NativeError::runtime(format!("ENOTDIR: chdir '{dir}'"))),
                Err(e) => Err(NativeError::runtime(format!("ENOENT: chdir '{dir}': {e}"))),
            };
        }
        match std::env::set_current_dir(&dir) {
            Ok(()) => Ok(()),
            Err(e) => {
                let err = NativeError::runtime(format!("chdir '{dir}': {e}"));
                Err(match e.raw_os_error() {
                    Some(errno) => err.with_prop("errno", -errno),
                    None => err,
                })
            }
        }
    }

    /// `process.abort()` — terminate immediately (SIGABRT, core dump where enabled). `std::process::abort`
    /// is the real thing; never returns.
    #[op(name = "abort")]
    fn op_abort(ctx: &mut Ctx) -> Result<(), NativeError> {
        if ctx.op_state().get::<RealmProcess>().is_some() {
            // 134: what a shell reports for SIGABRT.
            return end_realm_or_process(ctx, 134);
        }
        std::process::abort();
    }

    /// `(pid, signal)` — deliver `signal` to `pid`. The JS wrapper maps signal names to numbers.
    #[op(name = "kill", coerce)]
    fn op_kill(ctx: &mut Ctx, pid: f64, signal: f64) -> Result<(), NativeError> {
        let (pid, sig) = (pid as i32, signal as i32);
        if let Some(result) = kill_child_realm(ctx, pid, sig) {
            return result;
        }
        // The realm's own pid is the host's, and 0 / negative pids reach the host's process group.
        if pid <= 0 || pid as u32 == lumen_host::sysfs::process_id() {
            refuse_in_realm(ctx, "process.kill of the host process")?;
        }
        os_kill(ctx, pid, sig)
    }

    /// `(signal, listening)` — record whether the program has a `process.on` listener for `signal`,
    /// so the launching realm can deliver it (see `RealmProcess::signal_handlers`). Answers whether
    /// the program is an embedded realm (signal 0 only asks).
    #[op(name = "signalHandler", coerce)]
    fn op_signal_handler(ctx: &mut Ctx, signal: f64, listening: Option<bool>) -> bool {
        let signal = signal as i64;
        let listening = listening.unwrap_or(false);
        let Some(realm) = ctx.op_state().get::<RealmProcess>() else {
            return false;
        };
        if (1..64).contains(&signal) {
            let bit = 1u64 << signal;
            if listening {
                realm
                    .signal_handlers
                    .fetch_or(bit, std::sync::atomic::Ordering::SeqCst);
            } else {
                realm
                    .signal_handlers
                    .fetch_and(!bit, std::sync::atomic::Ordering::SeqCst);
            }
        }
        true
    }

    /// `([mask])` — read (no-arg, via the standard read-then-restore) or set the file-mode creation
    /// mask through `umask(2)`. Returns the previous mask. Unix-only; returns 0 off unix.
    #[op(name = "umask", coerce)]
    fn op_umask(ctx: &mut Ctx, mask: Option<f64>) -> Result<f64, NativeError> {
        os::umask(ctx, mask)
    }

    // Process-identity getters. POSIX-only; off unix they return `undefined` and the process.js wiring
    // leaves `process.getuid` &c. undefined, matching Node (which does not define them on Windows).
    #[op(name = "getuid")]
    fn op_getuid() -> Option<f64> {
        os::getuid()
    }

    #[op(name = "geteuid")]
    fn op_geteuid() -> Option<f64> {
        os::geteuid()
    }

    #[op(name = "getgid")]
    fn op_getgid() -> Option<f64> {
        os::getgid()
    }

    #[op(name = "getegid")]
    fn op_getegid() -> Option<f64> {
        os::getegid()
    }

    #[op(name = "getppid")]
    fn op_getppid() -> f64 {
        os::getppid()
    }

    #[op(name = "execve", coerce)]
    fn op_execve(
        ctx: &mut Ctx,
        path: String,
        argv: String,
        env: String,
    ) -> Result<(), NativeError> {
        os::execve(ctx, &path, &argv, &env)
    }

    #[op(name = "setuid", coerce)]
    fn op_setuid(ctx: &mut Ctx, id: f64) -> Result<(), NativeError> {
        os::setuid(ctx, id as u32)
    }

    #[op(name = "seteuid", coerce)]
    fn op_seteuid(ctx: &mut Ctx, id: f64) -> Result<(), NativeError> {
        os::seteuid(ctx, id as u32)
    }

    #[op(name = "setgid", coerce)]
    fn op_setgid(ctx: &mut Ctx, id: f64) -> Result<(), NativeError> {
        os::setgid(ctx, id as u32)
    }

    #[op(name = "setegid", coerce)]
    fn op_setegid(ctx: &mut Ctx, id: f64) -> Result<(), NativeError> {
        os::setegid(ctx, id as u32)
    }

    #[op(name = "getgroups")]
    fn op_getgroups(ctx: &mut Ctx) -> Result<Vec<f64>, NativeError> {
        os::getgroups(ctx)
    }

    #[op(name = "setgroups", coerce)]
    fn op_setgroups(ctx: &mut Ctx, groups: String) -> Result<(), NativeError> {
        os::setgroups(ctx, &groups)
    }

    #[op(name = "initgroups", coerce)]
    fn op_initgroups(ctx: &mut Ctx, user: String, group: f64) -> Result<(), NativeError> {
        os::initgroups(ctx, &user, group as u32)
    }

    #[op(name = "uidOf", coerce)]
    fn op_uid_of(name: String) -> Option<f64> {
        os::uid_of(&name)
    }

    #[op(name = "gidOf", coerce)]
    fn op_gid_of(name: String) -> Option<f64> {
        os::gid_of(&name)
    }

    #[op(name = "userNameOf", coerce)]
    fn op_user_name_of(uid: f64) -> Option<String> {
        os::user_name_of(uid as u32)
    }

    /// Real per-process CPU, resident-memory, and kernel resource counters (`getrusage(2)` and its
    /// Windows equivalents, see `lumen_os::sysinfo`). The compact array keeps the native boundary
    /// cheap; lumen-node assigns Node's public names.
    #[op(name = "metrics")]
    fn op_metrics() -> Result<Vec<f64>, OpError> {
        let usage = lumen_os::sysinfo::resource_usage()
            .map_err(|e| OpError::error(format!("getrusage failed: {e}")))?;
        let rss = lumen_os::sysinfo::resident_set_bytes().unwrap_or(usage.max_rss_kib * 1024);
        let (available, constrained) = lumen_os::sysinfo::available_memory(rss);
        let values = [
            rss,
            usage.max_rss_kib,
            usage.user_us,
            usage.system_us,
            usage.minor_faults,
            usage.major_faults,
            usage.swaps,
            usage.block_in,
            usage.block_out,
            usage.msgs_sent,
            usage.msgs_received,
            usage.signals,
            usage.voluntary_switches,
            usage.involuntary_switches,
            available,
            constrained,
        ];
        Ok(values.iter().map(|&v| v as f64).collect())
    }

    /// `isatty(fd)` for the standard streams (an embedded realm's streams are never terminals).
    #[op(name = "isatty", coerce)]
    fn op_isatty(ctx: &mut Ctx, fd: Option<f64>) -> bool {
        use std::io::IsTerminal;
        if ctx.op_state().get::<RealmProcess>().is_some() {
            return false;
        }
        match fd.unwrap_or(-1.0) as i64 {
            0 => std::io::stdin().is_terminal(),
            1 => std::io::stdout().is_terminal(),
            2 => std::io::stderr().is_terminal(),
            _ => false,
        }
    }

    /// `[columns, rows]` of the terminal on `fd` (1 or 2), or undefined when it is not one.
    #[op(name = "ttySize", coerce)]
    fn op_tty_size(fd: Option<f64>) -> Option<Vec<f64>> {
        terminal_size(fd.unwrap_or(1.0) as i32).map(|(cols, rows)| vec![cols as f64, rows as f64])
    }

    /// `process.title` before a program sets it: argv[0] as the process was started (an embedded
    /// realm has no title of its own: `undefined`, and the JS side falls back to its argv0).
    #[op(name = "startupTitle")]
    fn op_startup_title(ctx: &mut Ctx) -> Option<String> {
        if ctx.op_state().get::<RealmProcess>().is_some() {
            return None;
        }
        startup_args().first().cloned()
    }

    /// `process.title = t`: what `ps` shows for the process, written over the argv strings (the only
    /// memory the kernel reports a process's command line from), truncated to fit them.
    #[op(name = "setTitle", coerce)]
    fn op_set_title(ctx: &mut Ctx, title: String) {
        if ctx.op_state().get::<RealmProcess>().is_some() {
            return;
        }
        let _ = startup_args();
        set_os_title(&title);
    }
}

#[cfg(unix)]
fn os_kill(_ctx: &mut Ctx, pid: i32, sig: i32) -> Result<(), NativeError> {
    match lumen_os::proc::kill(pid, sig) {
        Ok(()) => Ok(()),
        Err(e) => Err(kill_error(e.code(), uv_errno(e.code()))),
    }
}

/// `process.kill` on Windows, as libuv's `uv_kill`: signal 0 checks the process is alive;
/// SIGTERM, SIGKILL, SIGINT and SIGQUIT terminate it (exit code 1); other signals are ENOSYS.
#[cfg(windows)]
fn os_kill(ctx: &mut Ctx, pid: i32, sig: i32) -> Result<(), NativeError> {
    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
        fn TerminateProcess(process: isize, code: u32) -> i32;
        fn GetExitCodeProcess(process: isize, code: *mut u32) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }
    const PROCESS_TERMINATE: u32 = 0x0001;
    const PROCESS_QUERY_INFORMATION: u32 = 0x0400;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const STILL_ACTIVE: u32 = 259;
    const ERROR_ACCESS_DENIED: i32 = 5;
    const ERROR_INVALID_PARAMETER: i32 = 87;

    if !matches!(sig, 0 | 2 | 3 | 9 | 15) {
        return Err(kill_error("ENOSYS", uv_errno("ENOSYS")));
    }
    if pid < 0 {
        return Err(kill_error("EINVAL", uv_errno("EINVAL")));
    }
    let os_error = |ctx: &mut Ctx, e: i32| match e {
        ERROR_INVALID_PARAMETER => kill_error("ESRCH", uv_errno("ESRCH")),
        ERROR_ACCESS_DENIED => kill_error("EPERM", uv_errno("EPERM")),
        _ => kill_error("UNKNOWN", uv_errno("UNKNOWN")),
    };
    // pid 0 means this process, as in libuv.
    let pid = if pid == 0 {
        lumen_host::sysfs::process_id()
    } else {
        pid as u32
    };
    let access = PROCESS_TERMINATE | PROCESS_QUERY_INFORMATION | SYNCHRONIZE;
    // SAFETY: plain Win32 calls; the handle is closed on every path below.
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle == 0 {
        let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(os_error(ctx, e));
    }
    let mut status = 0u32;
    // SAFETY: `handle` is open; `status` is a valid out pointer.
    let alive = unsafe { GetExitCodeProcess(handle, &mut status) } != 0 && status == STILL_ACTIVE;
    let result = if sig == 0 {
        if alive {
            Ok(())
        } else {
            Err(kill_error("ESRCH", uv_errno("ESRCH")))
        }
    } else if !alive {
        Err(kill_error("ESRCH", uv_errno("ESRCH")))
    // SAFETY: `handle` is open with PROCESS_TERMINATE.
    } else if unsafe { TerminateProcess(handle, 1) } != 0 {
        Ok(())
    } else {
        let e = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        Err(os_error(ctx, e))
    };
    // SAFETY: `handle` came from OpenProcess and is not used afterwards.
    unsafe { CloseHandle(handle) };
    result
}

#[cfg(not(any(unix, windows)))]
fn os_kill(ctx: &mut Ctx, _pid: i32, _sig: i32) -> Result<(), NativeError> {
    Err(NativeError::runtime(
        "process.kill is not supported on this platform",
    ))
}

#[cfg(unix)]
mod os {
    use super::*;

    pub(super) fn umask(ctx: &mut Ctx, mask: Option<f64>) -> Result<f64, NativeError> {
        let prev = match mask {
            Some(m) => {
                refuse_in_realm(ctx, "process.umask(mask)")?;
                lumen_os::proc::umask(m as u32)
            }
            // No argument: umask(2) has no pure read, so set-to-0-then-restore is the canonical idiom
            // (this is exactly why Node deprecated the read form).
            None => {
                let cur = lumen_os::proc::umask(0);
                lumen_os::proc::umask(cur);
                cur
            }
        };
        Ok((prev & 0o7777) as f64)
    }

    pub(super) fn getuid() -> Option<f64> {
        Some(lumen_os::proc::getuid() as f64)
    }
    pub(super) fn geteuid() -> Option<f64> {
        Some(lumen_os::proc::geteuid() as f64)
    }
    pub(super) fn getgid() -> Option<f64> {
        Some(lumen_os::proc::getgid() as f64)
    }
    pub(super) fn getegid() -> Option<f64> {
        Some(lumen_os::proc::getegid() as f64)
    }
    pub(super) fn getppid() -> f64 {
        lumen_os::proc::getppid() as f64
    }

    pub(super) fn execve(
        ctx: &mut Ctx,
        path: &str,
        argv: &str,
        env: &str,
    ) -> Result<(), NativeError> {
        refuse_in_realm(ctx, "process.execve")?;
        if path.contains('\0') {
            return Err(NativeError::type_error("execve path contains a null byte"));
        }
        let argv: Vec<&str> = argv.split('\0').filter(|value| !value.is_empty()).collect();
        let env: Vec<&str> = env.split('\0').filter(|value| !value.is_empty()).collect();
        let e = lumen_os::ident::execve(path, &argv, &env);
        Err(NativeError::runtime(format!(
            "execve failed: {} (os error {})",
            e.message(),
            e.errno()
        )))
    }

    /// Node's error for a failed identity change: `Error: EPERM, Operation not permitted` with
    /// `code`, `errno` and `syscall`.
    fn identity_result(
        result: Result<(), lumen_os::FsError>,
        syscall: &str,
    ) -> Result<(), NativeError> {
        let Err(e) = result else {
            return Ok(());
        };
        Err(
            NativeError::runtime(format!("{}, {}", e.code(), e.message()))
                .with_prop("code", e.code())
                .with_prop("errno", -e.errno())
                .with_prop("syscall", syscall.to_string()),
        )
    }

    pub(super) fn uid_of(name: &str) -> Option<f64> {
        lumen_os::ident::uid_of(name).map(|id| id as f64)
    }
    pub(super) fn gid_of(name: &str) -> Option<f64> {
        lumen_os::ident::gid_of(name).map(|id| id as f64)
    }
    pub(super) fn user_name_of(uid: u32) -> Option<String> {
        lumen_os::ident::user_name(uid)
    }

    pub(super) fn setuid(ctx: &mut Ctx, id: u32) -> Result<(), NativeError> {
        refuse_in_realm(ctx, "process.setuid")?;
        identity_result(lumen_os::ident::setuid(id), "setuid")
    }
    pub(super) fn seteuid(ctx: &mut Ctx, id: u32) -> Result<(), NativeError> {
        refuse_in_realm(ctx, "process.seteuid")?;
        identity_result(lumen_os::ident::seteuid(id), "seteuid")
    }
    pub(super) fn setgid(ctx: &mut Ctx, id: u32) -> Result<(), NativeError> {
        refuse_in_realm(ctx, "process.setgid")?;
        identity_result(lumen_os::ident::setgid(id), "setgid")
    }
    pub(super) fn setegid(ctx: &mut Ctx, id: u32) -> Result<(), NativeError> {
        refuse_in_realm(ctx, "process.setegid")?;
        identity_result(lumen_os::ident::setegid(id), "setegid")
    }

    pub(super) fn getgroups(_ctx: &mut Ctx) -> Result<Vec<f64>, NativeError> {
        let groups = lumen_os::ident::groups()
            .map_err(|e| NativeError::runtime(format!("getgroups failed: {e}")))?;
        Ok(groups.into_iter().map(|id| id as f64).collect())
    }

    pub(super) fn setgroups(ctx: &mut Ctx, text: &str) -> Result<(), NativeError> {
        refuse_in_realm(ctx, "process.setgroups")?;
        let groups = if text.is_empty() {
            Vec::new()
        } else {
            text.split(',')
                .map(str::parse::<u32>)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| NativeError::type_error("group ids must be numbers"))?
        };
        identity_result(lumen_os::ident::setgroups(&groups), "setgroups")
    }

    pub(super) fn initgroups(ctx: &mut Ctx, user: &str, group: u32) -> Result<(), NativeError> {
        refuse_in_realm(ctx, "process.initgroups")?;
        if user.contains('\0') {
            return Err(NativeError::type_error("user contains a null byte"));
        }
        identity_result(lumen_os::ident::initgroups(user, group), "initgroups")
    }
}

#[cfg(not(unix))]
mod os {
    use super::*;

    pub(super) fn umask(_ctx: &mut Ctx, _mask: Option<f64>) -> Result<f64, NativeError> {
        Ok(0.0)
    }
    pub(super) fn getuid() -> Option<f64> {
        None
    }
    pub(super) fn geteuid() -> Option<f64> {
        None
    }
    pub(super) fn getgid() -> Option<f64> {
        None
    }
    pub(super) fn getegid() -> Option<f64> {
        None
    }
    pub(super) fn getppid() -> f64 {
        // getppid is meaningless without the unix parent model; 0 is the honest "unknown".
        0.0
    }
    pub(super) fn execve(
        ctx: &mut Ctx,
        _path: &str,
        _argv: &str,
        _env: &str,
    ) -> Result<(), NativeError> {
        Err(NativeError::runtime(
            "process.execve is not supported on this platform",
        ))
    }
    pub(super) fn uid_of(_name: &str) -> Option<f64> {
        None
    }
    pub(super) fn gid_of(_name: &str) -> Option<f64> {
        None
    }
    pub(super) fn user_name_of(_uid: u32) -> Option<String> {
        None
    }
    pub(super) fn setuid(ctx: &mut Ctx, _id: u32) -> Result<(), NativeError> {
        Err(NativeError::runtime(
            "setuid is not supported on this platform",
        ))
    }
    pub(super) fn seteuid(ctx: &mut Ctx, _id: u32) -> Result<(), NativeError> {
        Err(NativeError::runtime(
            "seteuid is not supported on this platform",
        ))
    }
    pub(super) fn setgid(ctx: &mut Ctx, _id: u32) -> Result<(), NativeError> {
        Err(NativeError::runtime(
            "setgid is not supported on this platform",
        ))
    }
    pub(super) fn setegid(ctx: &mut Ctx, _id: u32) -> Result<(), NativeError> {
        Err(NativeError::runtime(
            "setegid is not supported on this platform",
        ))
    }
    pub(super) fn getgroups(_ctx: &mut Ctx) -> Result<Vec<f64>, NativeError> {
        Ok(Vec::new())
    }
    pub(super) fn setgroups(ctx: &mut Ctx, _text: &str) -> Result<(), NativeError> {
        Err(NativeError::runtime(
            "setgroups is not supported on this platform",
        ))
    }
    pub(super) fn initgroups(ctx: &mut Ctx, _user: &str, _group: u32) -> Result<(), NativeError> {
        Err(NativeError::runtime(
            "initgroups is not supported on this platform",
        ))
    }
}
