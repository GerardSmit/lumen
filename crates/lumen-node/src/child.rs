//! `node:child_process` over `std::process::Command` — spawning real OS subprocesses (no native
//! addon needed; the child is a separate process lumen just pipes to). Long-running children stream
//! via one-shot `read`/`write`/`wait` ops that run on *dedicated* threads (CompletionSender), since
//! child stdio can block for an unbounded time and must not pin a shared pool worker; plus a
//! synchronous `execSync` for the `*Sync` APIs.
//!
//! Handles live in a [`ChildRegistry`] in OpState, wrapped in `Arc<Mutex<>>` so the worker threads
//! can read/write them. `kill` sends SIGKILL (std can't send arbitrary signals).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use lumen_host::{ops, CompletionSender, Ctx, OpDecl, TaskId, TaskRegistry, Value};

/// A readable child stream (stdout or stderr), boxed to a common type.
type ReadStream = Arc<Mutex<Option<Box<dyn Read + Send>>>>;
type WriteStream = Arc<Mutex<Option<Box<dyn Write + Send>>>>;

/// The parent's end of an extra stdio slot (`stdio[3]` and up): a socketpair, so it is duplex
/// like Node's, split into the two halves the read/write worker threads lock separately.
struct ExtraPipe {
    reader: ReadStream,
    writer: WriteStream,
}

/// A spawned process: std's `Child`, or (Windows, extra stdio slots) one created directly with
/// `CreateProcessW` — see `win_spawn`.
enum ProcHandle {
    Std(Child),
    #[cfg(windows)]
    Raw(crate::win_spawn::RawChild),
}

impl ProcHandle {
    #[cfg(unix)]
    fn id(&self) -> u32 {
        match self {
            ProcHandle::Std(c) => c.id(),
        }
    }
    fn kill(&mut self) -> std::io::Result<()> {
        match self {
            ProcHandle::Std(c) => c.kill(),
            #[cfg(windows)]
            ProcHandle::Raw(c) => c.kill(),
        }
    }
    /// `Some((code, signal))` once exited.
    fn try_wait(&mut self) -> std::io::Result<Option<(Option<i32>, Option<i32>)>> {
        match self {
            ProcHandle::Std(c) => Ok(c.try_wait()?.map(|s| (s.code(), exit_signal(&s)))),
            #[cfg(windows)]
            ProcHandle::Raw(c) => Ok(c.try_wait()?.map(|code| (Some(code), None))),
        }
    }
}

struct ChildProc {
    child: Arc<Mutex<ProcHandle>>,
    stdin: WriteStream,
    stdout: ReadStream,
    stderr: ReadStream,
    extra: HashMap<u32, ExtraPipe>,
    /// `child.unref()` was called — its pending reads/waits must not keep the loop alive.
    unref: bool,
    /// Task ids of this child's in-flight reads/waits, so `unref()` can retroactively mark the
    /// ones registered before it was called (esbuild issues a read + wait, *then* unrefs).
    pending_tasks: Vec<TaskId>,
}

#[derive(Default)]
pub struct ChildRegistry {
    next: u32,
    procs: HashMap<u32, ChildProc>,
}

pub const CHILD_OPS: &[OpDecl] = ops![
    "spawn" (5) => op_spawn,
    "read" (4) => op_read,
    "write" (4) => op_write,
    "wait" (3) => op_wait,
    "unref" (1) => op_unref,
    "ref" (1) => op_ref,
    "kill" (2) => op_kill,
    "closeStdin" (1) => op_close_stdin,
    "writeFd" (5) => op_write_fd,
    "closeFd" (2) => op_close_fd,
    "execSync" (5) => op_exec_sync,
];

// ---- helpers ----------------------------------------------------------------------------------

fn read_string_array(ctx: &mut Ctx, v: &Value) -> Result<Vec<String>, Value> {
    let mut out = Vec::new();
    if v.as_obj().is_none() {
        return Ok(out);
    }
    let len = match ctx.get_member(v, "length") {
        Ok(Value::Num(n)) => n as usize,
        _ => return Ok(out),
    };
    for i in 0..len {
        let el = ctx
            .get_member(v, &i.to_string())
            .unwrap_or(Value::Undefined);
        out.push(ctx.coerce_string(&el)?.to_string());
    }
    Ok(out)
}

/// `["pipe"|"inherit"|"ignore", ...]` → the Stdio for one fd.
fn stdio_for(name: &str) -> Stdio {
    match name {
        "inherit" => Stdio::inherit(),
        "ignore" => Stdio::null(),
        _ => Stdio::piped(),
    }
}

fn opt_string(ctx: &mut Ctx, v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::Str(s)) => Some(s.to_string()),
        Some(v) if !matches!(v, Value::Undefined | Value::Null) => {
            ctx.coerce_string(v).ok().map(|s| s.to_string())
        }
        _ => None,
    }
}

/// The child's working directory: the one asked for, else the realm's (an embedded realm's cwd is
/// not the host process's), else inherited. A relative `cwd` is relative to the realm's.
fn apply_cwd(ctx: &mut Ctx, command: &mut Command, cwd: Option<String>) {
    let realm = ctx.op_state().get::<lumen_host::RealmProcess>();
    match (cwd, realm) {
        (Some(cwd), Some(realm)) => {
            command.current_dir(realm.resolve(&cwd));
        }
        (Some(cwd), None) => {
            command.current_dir(cwd);
        }
        (None, Some(realm)) => {
            command.current_dir(&realm.cwd);
        }
        (None, None) => {}
    }
}

/// env: an array of `[k, v]` pairs *replaces* the environment (Node semantics); absent = inherit.
fn apply_env(ctx: &mut Ctx, command: &mut Command, env_pairs: &Value) -> Result<(), Value> {
    if env_pairs.as_obj().is_none() {
        return Ok(());
    }
    if let Ok(Value::Num(n)) = ctx.get_member(env_pairs, "length") {
        command.env_clear();
        for i in 0..(n as usize) {
            let pair = ctx
                .get_member(env_pairs, &i.to_string())
                .unwrap_or(Value::Undefined);
            let k = ctx.get_member(&pair, "0").unwrap_or(Value::Undefined);
            let v = ctx.get_member(&pair, "1").unwrap_or(Value::Undefined);
            command.env(
                ctx.coerce_string(&k)?.to_string(),
                ctx.coerce_string(&v)?.to_string(),
            );
        }
    }
    Ok(())
}

fn build_command(
    ctx: &mut Ctx,
    cmd: &str,
    args: &[Value],
) -> Result<(Command, Vec<String>), Value> {
    let arg_list = read_string_array(ctx, args.get(1).unwrap_or(&Value::Undefined))?;
    let cwd = opt_string(ctx, args.get(2));
    let env_pairs = args.get(3).cloned().unwrap_or(Value::Undefined);
    let stdio = read_string_array(ctx, args.get(4).unwrap_or(&Value::Undefined))?;

    let verbatim = matches!(args.get(5), Some(Value::Bool(true)));

    let mut command = Command::new(cmd);
    add_args(&mut command, &arg_list, verbatim);
    apply_cwd(ctx, &mut command, cwd);
    apply_env(ctx, &mut command, &env_pairs)?;
    let mut stdio = stdio;
    while stdio.len() < 3 {
        stdio.push("pipe".to_string());
    }
    Ok((command, stdio))
}

#[cfg(unix)]
extern "C" {
    fn dup2(src: std::os::raw::c_int, dst: std::os::raw::c_int) -> std::os::raw::c_int;
    fn kill(pid: std::os::raw::c_int, sig: std::os::raw::c_int) -> std::os::raw::c_int;
}

/// Wire `stdio[3..]` slots marked `"pipe"` (a browser's `--remote-debugging-pipe` talks on fds
/// 3 and 4): each gets a socketpair whose child end is `dup2`'d onto its fd number after the
/// fork. Both ends are CLOEXEC, so the child sees only the numbered fd and the parent's end is
/// not leaked into it. Returns the parent halves keyed by fd.
#[cfg(unix)]
fn wire_extra_stdio(command: &mut Command, stdio: &[String]) -> std::io::Result<HashMap<u32, ExtraPipe>> {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;

    let mut extra = HashMap::new();
    let mut child_ends = Vec::new();
    for (fd, kind) in stdio.iter().enumerate().skip(3) {
        if kind != "pipe" {
            continue;
        }
        let (parent, child) = UnixStream::pair()?;
        let reader: Box<dyn Read + Send> = Box::new(parent.try_clone()?);
        let writer: Box<dyn Write + Send> = Box::new(parent);
        extra.insert(
            fd as u32,
            ExtraPipe {
                reader: Arc::new(Mutex::new(Some(reader))),
                writer: Arc::new(Mutex::new(Some(writer))),
            },
        );
        child_ends.push((child, fd as std::os::raw::c_int));
    }
    if !child_ends.is_empty() {
        // SAFETY: runs in the forked child before exec; dup2 is async-signal-safe and the
        // sockets it renumbers are kept alive by the closure until exec replaces the image.
        unsafe {
            command.pre_exec(move || {
                for (sock, fd) in &child_ends {
                    if dup2(sock.as_raw_fd(), *fd) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
    Ok(extra)
}

#[cfg(not(unix))]
fn wire_extra_stdio(_command: &mut Command, stdio: &[String]) -> std::io::Result<HashMap<u32, ExtraPipe>> {
    if stdio.iter().skip(3).any(|k| k == "pipe") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "extra stdio pipes (stdio[3] and up) are only supported on unix",
        ));
    }
    Ok(HashMap::new())
}

// ---- ops --------------------------------------------------------------------------------------

/// `(cmd, argsArray, cwd, envPairsOrNull, stdioArray) -> { childId, pid }`.
/// Append the child's arguments. `verbatim` (Node's `windowsVerbatimArguments`, which `shell` sets
/// for `cmd.exe`) passes them through without Windows' quoting: `cmd /d /s /c "<line>"` must reach
/// cmd.exe exactly as written. Unix has no command-line quoting, so it is a no-op there.
fn add_args(command: &mut Command, arg_list: &[String], verbatim: bool) {
    #[cfg(windows)]
    if verbatim {
        use std::os::windows::process::CommandExt;
        for arg in arg_list {
            command.raw_arg(arg);
        }
        return;
    }
    let _ = verbatim;
    command.args(arg_list);
}

/// A failed spawn as an error carrying the errno `code` (`ENOENT` for a missing program); the JS
/// glue turns it into Node's `spawn <file> ENOENT` error with `syscall`, `path` and `spawnargs`.
fn spawn_failure(ctx: &mut Ctx, cmd: &str, e: &std::io::Error) -> Value {
    let code = match e.kind() {
        std::io::ErrorKind::NotFound => "ENOENT",
        std::io::ErrorKind::PermissionDenied => "EACCES",
        _ => match e.raw_os_error() {
            Some(20) => "ENOTDIR",
            _ => "EINVAL",
        },
    };
    let err = ctx.make_error("Error", format!("spawn {cmd} {code}"));
    let _ = ctx.set_member(&err, "code", Value::str(code));
    err
}

fn op_spawn(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let cmd = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    #[cfg(windows)]
    if args
        .get(4)
        .is_some_and(|v| read_string_array(ctx, v).is_ok_and(|s| s.iter().skip(3).any(|k| k == "pipe")))
    {
        return spawn_windows_extra(ctx, &cmd, args);
    }
    let (mut command, stdio) = build_command(ctx, &cmd, args)?;
    command
        .stdin(stdio_for(&stdio[0]))
        .stdout(stdio_for(&stdio[1]))
        .stderr(stdio_for(&stdio[2]));
    let extra = wire_extra_stdio(&mut command, &stdio)
        .map_err(|e| ctx.make_error("Error", format!("spawn {cmd}: {e}")))?;

    let mut child =
        lumen_host::spawn_command(ctx, &mut command).map_err(|e| spawn_failure(ctx, &cmd, &e))?;
    let pid = child.id();
    let stdout: Option<Box<dyn Read + Send>> = child.stdout.take().map(|s| Box::new(s) as _);
    let stderr: Option<Box<dyn Read + Send>> = child.stderr.take().map(|s| Box::new(s) as _);
    let stdin: Option<Box<dyn Write + Send>> = child.stdin.take().map(|s| Box::new(s) as _);
    let proc = ChildProc {
        stdin: Arc::new(Mutex::new(stdin)),
        stdout: Arc::new(Mutex::new(stdout)),
        stderr: Arc::new(Mutex::new(stderr)),
        extra,
        child: Arc::new(Mutex::new(ProcHandle::Std(child))),
        unref: false,
        pending_tasks: Vec::new(),
    };
    Ok(register_child(ctx, proc, pid))
}

fn register_child(ctx: &mut Ctx, proc: ChildProc, pid: u32) -> Value {
    let reg = ctx
        .host_mut::<ChildRegistry>()
        .expect("child registry installed");
    let id = reg.next;
    reg.next += 1;
    reg.procs.insert(id, proc);

    let o = Value::Obj(ctx.new_object());
    let _ = ctx.set_member(&o, "childId", Value::Num(id as f64));
    let _ = ctx.set_member(&o, "pid", Value::Num(pid as f64));
    o
}

/// Windows spawn with extra stdio slots: `std::process::Command` cannot pass fds 3+, so the
/// child is created directly and gets them through the C runtime's inheritance block, as
/// libuv does (see `win_spawn`).
#[cfg(windows)]
fn spawn_windows_extra(ctx: &mut Ctx, cmd: &str, args: &[Value]) -> Result<Value, Value> {
    let arg_list = read_string_array(ctx, args.get(1).unwrap_or(&Value::Undefined))?;
    let cwd = opt_string(ctx, args.get(2));
    let env_pairs = args.get(3).cloned().unwrap_or(Value::Undefined);
    let mut stdio = read_string_array(ctx, args.get(4).unwrap_or(&Value::Undefined))?;
    while stdio.len() < 3 {
        stdio.push("pipe".to_string());
    }
    let verbatim = matches!(args.get(5), Some(Value::Bool(true)));
    // The same cwd rule as `apply_cwd`: the one asked for (relative to the realm's), else the
    // realm's, else inherited.
    let cwd = match ctx.op_state().get::<lumen_host::RealmProcess>() {
        Some(realm) => Some(match &cwd {
            Some(c) => realm.resolve(c),
            None => realm.cwd.clone(),
        }),
        None => cwd.map(std::path::PathBuf::from),
    };
    let env = if env_pairs.as_obj().is_some() {
        let mut pairs = Vec::new();
        if let Ok(Value::Num(n)) = ctx.get_member(&env_pairs, "length") {
            for i in 0..(n as usize) {
                let pair = ctx
                    .get_member(&env_pairs, &i.to_string())
                    .unwrap_or(Value::Undefined);
                let k = ctx.get_member(&pair, "0").unwrap_or(Value::Undefined);
                let v = ctx.get_member(&pair, "1").unwrap_or(Value::Undefined);
                pairs.push((
                    ctx.coerce_string(&k)?.to_string(),
                    ctx.coerce_string(&v)?.to_string(),
                ));
            }
        }
        Some(pairs)
    } else {
        None
    };
    let spec = crate::win_spawn::SpawnSpec {
        program: cmd,
        args: &arg_list,
        verbatim,
        cwd,
        env,
        stdio: &stdio,
    };
    let spawned = crate::win_spawn::spawn(&spec).map_err(|e| spawn_failure(ctx, cmd, &e))?;
    let pid = spawned.child.id();
    let extra = spawned
        .extra
        .into_iter()
        .map(|(fd, r, w)| {
            (
                fd,
                ExtraPipe {
                    reader: Arc::new(Mutex::new(Some(r))),
                    writer: Arc::new(Mutex::new(Some(w))),
                },
            )
        })
        .collect();
    let proc = ChildProc {
        stdin: Arc::new(Mutex::new(spawned.stdin)),
        stdout: Arc::new(Mutex::new(spawned.stdout)),
        stderr: Arc::new(Mutex::new(spawned.stderr)),
        extra,
        child: Arc::new(Mutex::new(ProcHandle::Raw(spawned.child))),
        unref: false,
        pending_tasks: Vec::new(),
    };
    Ok(register_child(ctx, proc, pid))
}

fn take_resolve_reject(
    ctx: &mut Ctx,
    res: Option<&Value>,
    rej: Option<&Value>,
) -> Result<(Value, Value), Value> {
    match (res, rej) {
        (Some(r), Some(j)) if r.is_callable() && j.is_callable() => Ok((r.clone(), j.clone())),
        _ => Err(ctx.make_error("TypeError", "child op expects (resolve, reject)")),
    }
}

/// Child stdio blocks for an unbounded time, so it runs on dedicated threads (via CompletionSender)
/// rather than the shared pool — otherwise a long-lived child (e.g. an esbuild service) would pin
/// pool workers for its whole lifetime and starve everything else.
fn completions(ctx: &mut Ctx) -> CompletionSender {
    ctx.op_state()
        .get::<CompletionSender>()
        .expect("runtime installs the completion sender")
        .clone()
}

/// `(childId, which, resolve, reject)` — read a chunk from stdout (which=1), stderr (which=2) or
/// an extra stdio pipe (which=3+). Resolves with a Uint8Array, or `null` at EOF.
fn op_read(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let which = args.get(1).and_then(Value::as_num_opt).unwrap_or(1.0) as u32;
    let (resolve, reject) = take_resolve_reject(ctx, args.get(2), args.get(3))?;

    let (handle, unref) = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get(&child_id))
        .and_then(|p| {
            let h = match which {
                2 => p.stderr.clone(),
                3.. => p.extra.get(&which)?.reader.clone(),
                _ => p.stdout.clone(),
            };
            Some((h, p.unref))
        })
        .ok_or_else(|| ctx.make_error("Error", "child: unknown process or stdio slot"))?;

    let reg = ctx.host_mut::<TaskRegistry>().expect("registry");
    let id = reg.register(resolve, Some(reject), decode_read);
    // The task inherits the child's current ref state; `child.ref()`/`unref()` can toggle it later.
    if unref {
        reg.set_unref(id);
    }
    track_child_task(ctx, child_id, id);
    completions(ctx).run_blocking(id, move || {
        let mut guard = handle.lock().expect("stream lock");
        let result: Result<Vec<u8>, String> = match guard.as_mut() {
            Some(stream) => {
                let mut buf = vec![0u8; 65536];
                match stream.read(&mut buf) {
                    Ok(0) => Ok(Vec::new()),
                    Ok(n) => {
                        buf.truncate(n);
                        Ok(buf)
                    }
                    Err(e) => Err(format!("read: {e}")),
                }
            }
            None => Ok(Vec::new()),
        };
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_read(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<Vec<u8>, String>>()
        .expect("read payload")
    {
        Ok(bytes) if bytes.is_empty() => Ok(vec![Value::Null]), // EOF
        Ok(bytes) => Ok(vec![ctx.make_uint8array(&bytes)?]),
        Err(e) => Err(ctx.make_error("Error", e)),
    }
}

/// `(childId, bytes, resolve, reject)` — write to the child's stdin.
fn op_write(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let data = ctx
        .typed_array_bytes(args.get(1).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "child write expects bytes"))?;
    let (resolve, reject) = take_resolve_reject(ctx, args.get(2), args.get(3))?;

    let handle = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get(&child_id))
        .map(|p| p.stdin.clone())
        .ok_or_else(|| ctx.make_error("Error", "child: unknown process"))?;

    let id = ctx.host_mut::<TaskRegistry>().expect("registry").register(
        resolve,
        Some(reject),
        decode_ok,
    );
    completions(ctx).run_blocking(id, move || {
        let mut guard = handle.lock().expect("stdin lock");
        let result: Result<(), String> = match guard.as_mut() {
            Some(stdin) => stdin
                .write_all(&data)
                .and_then(|()| stdin.flush())
                .map_err(|e| format!("write: {e}")),
            None => Err("child stdin is closed".to_string()),
        };
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_ok(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<(), String>>()
        .expect("ok payload")
    {
        Ok(()) => Ok(vec![]),
        Err(e) => Err(ctx.make_error("Error", e)),
    }
}

/// `(childId, resolve, reject)` — wait for exit; resolves with `[code, signal]`: the exit code
/// and `null`, or `null` and the terminating signal number.
fn op_wait(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let (resolve, reject) = take_resolve_reject(ctx, args.get(1), args.get(2))?;
    let (handle, unref) = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get(&child_id))
        .map(|p| (p.child.clone(), p.unref))
        .ok_or_else(|| ctx.make_error("Error", "child: unknown process"))?;

    let reg = ctx.host_mut::<TaskRegistry>().expect("registry");
    let id = reg.register(resolve, Some(reject), decode_exit);
    if unref {
        reg.set_unref(id);
    }
    track_child_task(ctx, child_id, id);
    completions(ctx).run_blocking(id, move || {
        // Poll try_wait(), releasing the child lock between checks, so `kill()` can acquire it (a
        // blocking wait() would hold the lock for the child's whole life and deadlock kill).
        let result: Result<(Option<i32>, Option<i32>), String> = loop {
            {
                match handle.lock().expect("child lock").try_wait() {
                    Ok(Some(status)) => break Ok(status),
                    Ok(None) => {}
                    Err(e) => break Err(e.to_string()),
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        Box::new(result)
    });
    Ok(Value::Undefined)
}

fn decode_exit(ctx: &mut Ctx, payload: Box<dyn std::any::Any + Send>) -> Result<Vec<Value>, Value> {
    match *payload
        .downcast::<Result<(Option<i32>, Option<i32>), String>>()
        .expect("exit payload")
    {
        Ok((code, signal)) => {
            let num = |n: Option<i32>| n.map_or(Value::Null, |n| Value::Num(n as f64));
            Ok(vec![ctx.make_array(vec![num(code), num(signal)])])
        }
        Err(e) => Err(ctx.make_error("Error", e)),
    }
}

#[cfg(unix)]
fn exit_signal(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(not(unix))]
fn exit_signal(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

/// Remember a child's in-flight task so `unref()` can mark it later (see [`ChildProc::pending_tasks`]).
fn track_child_task(ctx: &mut Ctx, child_id: u32, task: TaskId) {
    if let Some(p) = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get_mut(&child_id))
    {
        p.pending_tasks.push(task);
    }
}

/// Set the child's ref state and apply it to every in-flight read/wait. esbuild toggles this per
/// request (`refCount`): the persistent service child is unref'd while idle so it doesn't block
/// exit, and ref'd during a transform so the loop waits for the response.
fn set_child_ref(ctx: &mut Ctx, child_id: u32, unref: bool) {
    let pending = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get_mut(&child_id))
        .map(|p| {
            p.unref = unref;
            p.pending_tasks.clone()
        })
        .unwrap_or_default();
    if let Some(reg) = ctx.host_mut::<TaskRegistry>() {
        for id in pending {
            if unref {
                reg.set_unref(id);
            } else {
                reg.set_ref(id);
            }
        }
    }
}

/// `(childId)` — `child.unref()`.
fn op_unref(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    set_child_ref(ctx, child_id, true);
    Ok(Value::Undefined)
}

/// `(childId)` — `child.ref()`.
fn op_ref(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    set_child_ref(ctx, child_id, false);
    Ok(Value::Undefined)
}

/// `(childId, fd, bytes, resolve, reject)` — write to an extra stdio pipe (fd 3+).
fn op_write_fd(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let fd = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let data = ctx
        .typed_array_bytes(args.get(2).unwrap_or(&Value::Undefined))
        .ok_or_else(|| ctx.make_error("TypeError", "child write expects bytes"))?;
    let (resolve, reject) = take_resolve_reject(ctx, args.get(3), args.get(4))?;

    let handle = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get(&child_id))
        .and_then(|p| p.extra.get(&fd))
        .map(|e| e.writer.clone())
        .ok_or_else(|| ctx.make_error("Error", "child: unknown process or stdio slot"))?;

    let id = ctx.host_mut::<TaskRegistry>().expect("registry").register(
        resolve,
        Some(reject),
        decode_ok,
    );
    completions(ctx).run_blocking(id, move || {
        let mut guard = handle.lock().expect("pipe lock");
        let result: Result<(), String> = match guard.as_mut() {
            Some(w) => w
                .write_all(&data)
                .and_then(|()| w.flush())
                .map_err(|e| format!("write: {e}")),
            None => Err("child pipe is closed".to_string()),
        };
        Box::new(result)
    });
    Ok(Value::Undefined)
}

/// `(childId, fd)` — close the parent's write half of an extra stdio pipe (EOF to the child).
fn op_close_fd(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let fd = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    if let Some(e) = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get(&child_id))
        .and_then(|p| p.extra.get(&fd))
    {
        e.writer.lock().expect("pipe lock").take();
    }
    Ok(Value::Undefined)
}

/// `(childId, signal)` — `signal` is the numeric signal (0 means the default, SIGTERM). On unix
/// any signal is delivered with kill(2); elsewhere every signal is a hard kill.
fn op_kill(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    let signal = args.get(1).and_then(Value::as_num_opt).unwrap_or(0.0) as i32;
    let killed = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get(&child_id))
        .map(|p| send_signal(&p.child, signal))
        .unwrap_or(false);
    Ok(Value::Bool(killed))
}

#[cfg(unix)]
fn send_signal(child: &Arc<Mutex<ProcHandle>>, signal: i32) -> bool {
    let mut child = child.lock().expect("child lock");
    if signal == 9 {
        return child.kill().is_ok();
    }
    // A reaped child has no pid to signal; `try_wait` also keeps us from signalling a recycled one.
    if matches!(child.try_wait(), Ok(Some(_))) {
        return false;
    }
    let sig = if signal == 0 { 15 } else { signal };
    // SAFETY: plain syscall on a pid this process spawned and has not reaped.
    unsafe { kill(child.id() as std::os::raw::c_int, sig) == 0 }
}

#[cfg(not(unix))]
fn send_signal(child: &Arc<Mutex<ProcHandle>>, _signal: i32) -> bool {
    child.lock().expect("child lock").kill().is_ok()
}

fn op_close_stdin(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let child_id = args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32;
    if let Some(p) = ctx
        .host_mut::<ChildRegistry>()
        .and_then(|r| r.procs.get(&child_id))
    {
        p.stdin.lock().expect("stdin lock").take(); // drop → EOF to the child
    }
    Ok(Value::Undefined)
}

/// `(cmd, argsArray, inputBytesOrNull, cwd, envPairsOrNull) -> { stdout, stderr, status }`. Synchronous: spawns,
/// writes optional stdin, waits, and captures output — for `execFileSync`/`spawnSync`/`execSync`.
fn op_exec_sync(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let cmd = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let arg_list = read_string_array(ctx, args.get(1).unwrap_or(&Value::Undefined))?;
    let input = ctx.typed_array_bytes(args.get(2).unwrap_or(&Value::Undefined));
    let cwd = opt_string(ctx, args.get(3));
    let env_pairs = args.get(4).cloned().unwrap_or(Value::Undefined);
    let verbatim = matches!(args.get(5), Some(Value::Bool(true)));

    let mut command = Command::new(&cmd);
    add_args(&mut command, &arg_list, verbatim);
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    apply_cwd(ctx, &mut command, cwd);
    apply_env(ctx, &mut command, &env_pairs)?;
    // `timeout` (ms): past it the child is killed and the result says so (Node's ETIMEDOUT).
    let timeout = args
        .get(6)
        .and_then(Value::as_num_opt)
        .filter(|t| t.is_finite() && *t > 0.0)
        .map(|t| std::time::Duration::from_secs_f64(t / 1000.0));
    let mut child =
        lumen_host::spawn_command(ctx, &mut command).map_err(|e| spawn_failure(ctx, &cmd, &e))?;
    // Feed stdin from its own thread (dropping it signals EOF): a child that fills its stdout
    // pipe before draining stdin would otherwise deadlock against this write.
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let (tx, rx) = std::sync::mpsc::channel::<(u8, Vec<u8>)>();
    let pipes: [(u8, Option<Box<dyn Read + Send>>); 2] = [
        (1, child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>)),
        (2, child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>)),
    ];
    for (which, pipe) in pipes {
        if let Some(mut pipe) = pipe {
            let tx = tx.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = pipe.read_to_end(&mut buf);
                let _ = tx.send((which, buf));
            });
        }
    }
    drop(tx);
    let wait_error = |ctx: &mut Ctx, e: std::io::Error| ctx.make_error("Error", format!("exec {cmd}: {e}"));
    let mut timed_out = false;
    let status = match timeout {
        None => child.wait().map_err(|e| wait_error(ctx, e))?,
        Some(limit) => {
            let started = std::time::Instant::now();
            loop {
                if let Some(status) = child.try_wait().map_err(|e| wait_error(ctx, e))? {
                    break status;
                }
                let elapsed = started.elapsed();
                if elapsed >= limit {
                    timed_out = true;
                    let _ = child.kill();
                    break child.wait().map_err(|e| wait_error(ctx, e))?;
                }
                std::thread::sleep((limit - elapsed).min(std::time::Duration::from_millis(5)));
            }
        }
    };
    // After a timeout, a grandchild may still hold the pipes: take what arrived, don't wait.
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    loop {
        let got = if timed_out {
            rx.recv_timeout(std::time::Duration::from_millis(200)).ok()
        } else {
            rx.recv().ok()
        };
        match got {
            Some((1, bytes)) => stdout = bytes,
            Some((_, bytes)) => stderr = bytes,
            None => break,
        }
    }

    let o = Value::Obj(ctx.new_object());
    let stdout = ctx.make_uint8array(&stdout)?;
    let stderr = ctx.make_uint8array(&stderr)?;
    let _ = ctx.set_member(&o, "stdout", stdout);
    let _ = ctx.set_member(&o, "stderr", stderr);
    let _ = ctx.set_member(
        &o,
        "status",
        match status.code() {
            Some(c) if !timed_out => Value::Num(c as f64),
            _ => Value::Null,
        },
    );
    let _ = ctx.set_member(&o, "timedOut", Value::Bool(timed_out));
    Ok(o)
}
