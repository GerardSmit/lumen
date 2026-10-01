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

use lumen_host::{
    ops, ChildRealm, ChildRealmExit, ChildRealmRequest, CompletionSender, Ctx, OpDecl, TaskId,
    TaskRegistry, Value,
};

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
    /// A child realm in this process (see [`spawn_child_realm`]).
    Realm(Arc<dyn ChildRealm>),
    #[cfg(windows)]
    Raw(crate::win_spawn::RawChild),
}

impl ProcHandle {
    #[cfg(unix)]
    fn id(&self) -> u32 {
        match self {
            ProcHandle::Std(c) => c.id(),
            ProcHandle::Realm(realm) => realm.pid(),
        }
    }
    fn kill(&mut self) -> std::io::Result<()> {
        match self {
            ProcHandle::Std(c) => c.kill(),
            ProcHandle::Realm(realm) => {
                realm.terminate(9);
                Ok(())
            }
            #[cfg(windows)]
            ProcHandle::Raw(c) => c.kill(),
        }
    }
    /// `Some((code, signal))` once exited.
    fn try_wait(&mut self) -> std::io::Result<Option<(Option<i32>, Option<i32>)>> {
        match self {
            ProcHandle::Std(c) => Ok(c.try_wait()?.map(|s| (s.code(), exit_signal(&s)))),
            ProcHandle::Realm(realm) => Ok(realm.exit().map(|exit| match exit {
                ChildRealmExit::Exited(code) => (Some(code), None),
                ChildRealmExit::Signalled(signal) => (None, Some(signal)),
            })),
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
    /// A child realm's in-process stdio pipes, so the launching realm's end can wake the child
    /// out of a blocked read or write without taking the locks reads and writes hold.
    pipes: Vec<mem_pipe::Handle>,
}

#[derive(Default)]
pub struct ChildRegistry {
    next: u32,
    procs: HashMap<u32, ChildProc>,
}

pub const CHILD_OPS: &[OpDecl] = ops![
    "spawn" (7) => op_spawn,
    "read" (4) => op_read,
    "write" (4) => op_write,
    "wait" (3) => op_wait,
    "unref" (1) => op_unref,
    "ref" (1) => op_ref,
    "kill" (2) => op_kill,
    "closeStdin" (1) => op_close_stdin,
    "writeFd" (5) => op_write_fd,
    "closeFd" (2) => op_close_fd,
    "execSync" (8) => op_exec_sync,
    "ipcOpen" (1) => op_ipc_open,
    "ipcRead" (1) => op_ipc_read,
    "ipcWrite" (2) => op_ipc_write,
    "ipcClose" (1) => op_ipc_close,
];

// Dedicated inherited IPC descriptors are socketpairs. Keep the child event loop
// nonblocking; JS controls channel ref/unref and bounds queued serialized messages.
fn ipc_fd(ctx: &mut Ctx, args: &[Value]) -> Result<i32, Value> {
    match args.first().and_then(Value::as_num_opt) {
        Some(fd) if fd >= 3.0 && fd <= i32::MAX as f64 && fd.fract() == 0.0 => Ok(fd as i32),
        _ => Err(ctx.make_error("TypeError", "IPC descriptor must be an integer of at least 3")),
    }
}
fn ipc_failure(ctx: &mut Ctx) -> Value {
    let error=std::io::Error::last_os_error();
    let value=ctx.make_error("Error",format!("IPC descriptor: {error}"));
    let _=ctx.set_member(&value,"code",Value::str(lumen_os::errno::uv_code(&error)));value
}
fn op_ipc_open(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let fd=ipc_fd(ctx,args)?;
    #[cfg(unix)] {
        let flags=unsafe {libc::fcntl(fd,libc::F_GETFL)};
        if flags<0 || unsafe {libc::fcntl(fd,libc::F_SETFL,flags|libc::O_NONBLOCK)}<0 { return Err(ipc_failure(ctx)); }
        Ok(Value::Undefined)
    }
    #[cfg(not(unix))] {let _=fd;Err(ctx.make_error("Error","Dedicated subprocess IPC is currently supported on Unix only"))}
}
fn op_ipc_read(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let fd=ipc_fd(ctx,args)?;
    #[cfg(unix)] {
        let mut bytes=[0u8;65536];let count=unsafe {libc::read(fd,bytes.as_mut_ptr().cast(),bytes.len())};
        if count<0 {
            let error=std::io::Error::last_os_error();
            if matches!(error.kind(),std::io::ErrorKind::WouldBlock|std::io::ErrorKind::Interrupted) {return Ok(Value::Null);}
            return Err(ipc_failure(ctx));
        }
        ctx.make_uint8array(&bytes[..count as usize])
    }
    #[cfg(not(unix))] {let _=fd;Err(ctx.make_error("Error","Dedicated subprocess IPC is currently supported on Unix only"))}
}
fn op_ipc_write(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let fd=ipc_fd(ctx,args)?;
    let bytes=args.get(1).and_then(|value|ctx.typed_array_bytes(value)).ok_or_else(||ctx.make_error("TypeError","IPC write requires bytes"))?;
    #[cfg(unix)] {
        // send suppresses SIGPIPE on Linux/Android; macOS socket setup uses SO_NOSIGPIPE.
        #[cfg(any(target_os="linux",target_os="android"))] let count=unsafe {libc::send(fd,bytes.as_ptr().cast(),bytes.len(),libc::MSG_NOSIGNAL)};
        #[cfg(not(any(target_os="linux",target_os="android")))] let count=unsafe {libc::write(fd,bytes.as_ptr().cast(),bytes.len())};
        if count<0 {
            let error=std::io::Error::last_os_error();
            if matches!(error.kind(),std::io::ErrorKind::WouldBlock|std::io::ErrorKind::Interrupted) {return Ok(Value::Num(0.0));}
            return Err(ipc_failure(ctx));
        }
        Ok(Value::Num(count as f64))
    }
    #[cfg(not(unix))] {let _=(fd,bytes);Err(ctx.make_error("Error","Dedicated subprocess IPC is currently supported on Unix only"))}
}
fn op_ipc_close(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let fd=ipc_fd(ctx,args)?;
    #[cfg(unix)] {
        // A child realm's channel is closed by the host that made it, after the realm ends; the
        // descriptor number must stay valid until then. Shutting it down is what the peer sees.
        let owned=ctx.op_state().get::<lumen_host::RealmProcess>().is_some_and(|realm|realm.owned_fds.contains(&fd));
        let status=if owned {unsafe {libc::shutdown(fd,libc::SHUT_RDWR)}} else {unsafe {libc::close(fd)}};
        if status<0 {return Err(ipc_failure(ctx));}
        Ok(Value::Undefined)
    }
    #[cfg(not(unix))] {let _=fd;Err(ctx.make_error("Error","Dedicated subprocess IPC is currently supported on Unix only"))}
}

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
    // `fd:N`: the child gets (a duplicate of) this process's descriptor N.
    #[cfg(unix)]
    if let Some(fd) = name.strip_prefix("fd:").and_then(|n| n.parse::<i32>().ok()) {
        use std::os::fd::FromRawFd;
        // SAFETY: F_DUPFD_CLOEXEC on a descriptor number; a failure leaves the slot empty.
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if dup < 0 {
            return Stdio::null();
        }
        // SAFETY: `dup` is a fresh descriptor owned by nobody else.
        return Stdio::from(unsafe { std::os::fd::OwnedFd::from_raw_fd(dup) });
    }
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
fn wire_extra_stdio(command: &mut Command, stdio: &[String], ipc_fd: &mut Option<i32>) -> std::io::Result<HashMap<u32, ExtraPipe>> {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::os::unix::process::CommandExt;

    let mut extra = HashMap::new();
    let mut child_ends = Vec::new();
    let mut inherited = Vec::new();
    for (fd, kind) in stdio.iter().enumerate().skip(3) {
        if let Some(src) = kind.strip_prefix("fd:").and_then(|n| n.parse::<std::os::raw::c_int>().ok()) {
            inherited.push((src, fd as std::os::raw::c_int));
            continue;
        }
        if kind != "pipe" && kind != "ipc" {
            continue;
        }
        let (parent, child) = UnixStream::pair()?;
        if kind == "ipc" {
            // The parent's end of the IPC channel goes to JS as a raw descriptor (net's
            // adoptFd), so handles can travel over it as SCM_RIGHTS.
            use std::os::fd::IntoRawFd;
            #[cfg(any(target_os="macos",target_os="ios"))] {
                let enabled:libc::c_int=1;
                for sock in [parent.as_raw_fd(), child.as_raw_fd()] {
                    // SAFETY: setsockopt on a live socket with a c_int option.
                    unsafe {libc::setsockopt(sock,libc::SOL_SOCKET,libc::SO_NOSIGPIPE,(&enabled as *const libc::c_int).cast(),std::mem::size_of_val(&enabled) as libc::socklen_t)};
                }
            }
            *ipc_fd = Some(parent.into_raw_fd());
            child_ends.push((child, fd as std::os::raw::c_int));
            continue;
        }
        #[cfg(any(target_os="macos",target_os="ios"))] {
            let enabled:libc::c_int=1;
            if unsafe {libc::setsockopt(child.as_raw_fd(),libc::SOL_SOCKET,libc::SO_NOSIGPIPE,(&enabled as *const libc::c_int).cast(),std::mem::size_of_val(&enabled) as libc::socklen_t)}<0 { return Err(std::io::Error::last_os_error()); }
        }
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
    if !child_ends.is_empty() || !inherited.is_empty() {
        // SAFETY: runs in the forked child before exec; dup2/fcntl are async-signal-safe and the
        // sockets it renumbers are kept alive by the closure until exec replaces the image.
        unsafe {
            command.pre_exec(move || {
                for (sock, fd) in &child_ends {
                    if dup2(sock.as_raw_fd(), *fd) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                for (src, fd) in &inherited {
                    if src == fd {
                        let flags = libc::fcntl(*fd, libc::F_GETFD);
                        libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC);
                    } else if dup2(*src, *fd) < 0 {
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
fn wire_extra_stdio(_command: &mut Command, stdio: &[String], _ipc_fd: &mut Option<i32>) -> std::io::Result<HashMap<u32, ExtraPipe>> {
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
    let code = lumen_os::errno::uv_code(e);
    let err = ctx.make_error("Error", format!("spawn {cmd} {code}"));
    let _ = ctx.set_member(&err, "code", Value::str(code));
    err
}

struct SpawnOpts {
    argv0: Option<String>,
    uid: Option<u32>,
    gid: Option<u32>,
    detached: bool,
}

fn read_spawn_opts(ctx: &mut Ctx, v: Option<&Value>) -> SpawnOpts {
    let mut o = SpawnOpts { argv0: None, uid: None, gid: None, detached: false };
    let Some(v) = v.filter(|v| v.as_obj().is_some()) else { return o };
    let get = |ctx: &mut Ctx, k: &str| ctx.get_member(v, k).unwrap_or(Value::Undefined);
    let argv0 = get(ctx, "argv0");
    o.argv0 = opt_string(ctx, Some(&argv0));
    if let Value::Num(n) = get(ctx, "uid") {
        o.uid = Some(n as u32);
    }
    if let Value::Num(n) = get(ctx, "gid") {
        o.gid = Some(n as u32);
    }
    o.detached = matches!(get(ctx, "detached"), Value::Bool(true));
    o
}

#[cfg(unix)]
fn apply_spawn_opts(command: &mut Command, o: &SpawnOpts) {
    use std::os::unix::process::CommandExt;
    if let Some(a) = &o.argv0 {
        command.arg0(a);
    }
    if let Some(uid) = o.uid {
        command.uid(uid);
    }
    if let Some(gid) = o.gid {
        command.gid(gid);
    }
    if o.detached {
        // SAFETY: setsid is async-signal-safe and runs in the forked child before exec.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
}

#[cfg(not(unix))]
fn apply_spawn_opts(_command: &mut Command, _o: &SpawnOpts) {}

fn op_spawn(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let cmd = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    if let Some(started) = spawn_child_realm(ctx, &cmd, args)? {
        return Ok(started);
    }
    #[cfg(windows)]
    if args
        .get(4)
        .is_some_and(|v| read_string_array(ctx, v).is_ok_and(|s| s.iter().skip(3).any(|k| k == "pipe")))
    {
        return spawn_windows_extra(ctx, &cmd, args);
    }
    let (mut command, stdio) = build_command(ctx, &cmd, args)?;
    let spawn_opts = read_spawn_opts(ctx, args.get(6));
    apply_spawn_opts(&mut command, &spawn_opts);
    command
        .stdin(stdio_for(&stdio[0]))
        .stdout(stdio_for(&stdio[1]))
        .stderr(stdio_for(&stdio[2]));
    let mut ipc_fd = None;
    let extra = wire_extra_stdio(&mut command, &stdio, &mut ipc_fd)
        .map_err(|e| ctx.make_error("Error", format!("spawn {cmd}: {e}")))?;

    let mut child = match lumen_host::spawn_command(ctx, &mut command) {
        Ok(child) => child,
        Err(e) => {
            #[cfg(unix)]
            if let Some(fd) = ipc_fd {
                // SAFETY: the parent's end of the channel, owned here and never handed out.
                unsafe { libc::close(fd) };
            }
            return Err(spawn_failure(ctx, &cmd, &e));
        }
    };
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
        pipes: Vec::new(),
    };
    let info = register_child(ctx, proc, pid);
    if let Some(fd) = ipc_fd {
        let _ = ctx.set_member(&info, "ipcFd", Value::Num(fd as f64));
    }
    Ok(info)
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
        pipes: Vec::new(),
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

    let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_read);
    let reg = ctx.host_mut::<TaskRegistry>().expect("registry");
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

    let id = lumen_host::register_task(
        ctx,
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

    let id = lumen_host::register_task(ctx, resolve, Some(reject), decode_exit);
    let reg = ctx.host_mut::<TaskRegistry>().expect("registry");
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

    let id = lumen_host::register_task(
        ctx,
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

/// `(childId, signal)` — `signal` is the numeric signal (0 only probes that the child exists). On unix
/// any signal is delivered with kill(2); elsewhere every signal is a hard kill. A child realm gets
/// it as an OS process would: `SIGKILL` stops it at once, any other reaches its `process.on`
/// listeners or takes the default action (see `ChildRealm::signal`).
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
    if let ProcHandle::Realm(realm) = &*child {
        if realm.exit().is_some() {
            return false;
        }
        if signal != 0 {
            realm.signal(signal);
        }
        return true;
    }
    if signal == 9 {
        return child.kill().is_ok();
    }
    // A reaped child has no pid to signal; `try_wait` also keeps us from signalling a recycled one.
    if matches!(child.try_wait(), Ok(Some(_))) {
        return false;
    }
    let sig = signal;
    // SAFETY: plain syscall on a pid this process spawned and has not reaped.
    unsafe { kill(child.id() as std::os::raw::c_int, sig) == 0 }
}

#[cfg(not(unix))]
fn send_signal(child: &Arc<Mutex<ProcHandle>>, signal: i32) -> bool {
    let mut child = child.lock().expect("child lock");
    if let ProcHandle::Realm(realm) = &*child {
        if realm.exit().is_some() {
            return false;
        }
        if signal != 0 {
            realm.signal(signal);
        }
        return true;
    }
    child.kill().is_ok()
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

#[cfg(unix)]
fn signal_pid(pid: u32, signal: i32) {
    // SAFETY: plain syscall on a child this call spawned and has not yet reaped.
    unsafe {
        kill(pid as std::os::raw::c_int, signal);
    }
}

/// `(cmd, argsArray, inputBytesOrNull, cwd, envPairsOrNull, verbatim, timeoutMs, opts)` with
/// `opts = { stdio: [in, out, err], argv0, uid, gid, killSignal, maxBuffer }`. Synchronous: spawns,
/// writes optional stdin, waits, and captures piped output — for the `*Sync` APIs. A child whose
/// output passes `maxBuffer` or that outlives `timeout` is killed with `killSignal`.
fn op_exec_sync(ctx: &mut Ctx, _t: Value, args: &[Value]) -> Result<Value, Value> {
    let cmd = ctx
        .coerce_string(args.first().unwrap_or(&Value::Undefined))?
        .to_string();
    let arg_list = read_string_array(ctx, args.get(1).unwrap_or(&Value::Undefined))?;
    let input = ctx.typed_array_bytes(args.get(2).unwrap_or(&Value::Undefined));
    let cwd = opt_string(ctx, args.get(3));
    let env_pairs = args.get(4).cloned().unwrap_or(Value::Undefined);
    let verbatim = matches!(args.get(5), Some(Value::Bool(true)));
    let opts = args.get(7).cloned().unwrap_or(Value::Undefined);
    let (stdio, max_buffer, kill_signal) = if opts.as_obj().is_some() {
        let stdio_v = ctx.get_member(&opts, "stdio").unwrap_or(Value::Undefined);
        let stdio = read_string_array(ctx, &stdio_v)?;
        let max = match ctx.get_member(&opts, "maxBuffer") {
            Ok(Value::Num(n)) if n.is_finite() && n >= 0.0 => Some(n as usize),
            _ => None,
        };
        let sig = match ctx.get_member(&opts, "killSignal") {
            Ok(Value::Num(n)) if n > 0.0 => n as i32,
            _ => 15,
        };
        (stdio, max, sig)
    } else {
        (Vec::new(), None, 15)
    };
    let mode = |i: usize| stdio.get(i).map(String::as_str).unwrap_or("pipe");
    let timeout = args
        .get(6)
        .and_then(Value::as_num_opt)
        .filter(|t| t.is_finite() && *t > 0.0)
        .map(|t| std::time::Duration::from_secs_f64(t / 1000.0));
    if let Some(site) = realm_site(ctx, &cmd) {
        let sync = SyncRealm {
            input,
            modes: [mode(0).to_string(), mode(1).to_string(), mode(2).to_string()],
            max_buffer,
            kill_signal,
            timeout,
        };
        let env = read_env_pairs(ctx, &env_pairs)?;
        return exec_sync_in_realm(ctx, site, &cmd, &arg_list, cwd, env, sync);
    }
    let spawn_opts = read_spawn_opts(ctx, Some(&opts));

    let mut command = Command::new(&cmd);
    add_args(&mut command, &arg_list, verbatim);
    apply_spawn_opts(&mut command, &spawn_opts);
    command
        .stdout(stdio_for(mode(1)))
        .stderr(stdio_for(mode(2)));
    command.stdin(match mode(0) {
        "inherit" => Stdio::inherit(),
        "pipe" if input.is_some() => Stdio::piped(),
        _ => Stdio::null(),
    });
    apply_cwd(ctx, &mut command, cwd);
    apply_env(ctx, &mut command, &env_pairs)?;
    let mut child =
        lumen_host::spawn_command(ctx, &mut command).map_err(|e| spawn_failure(ctx, &cmd, &e))?;
    let pid = child.id();
    // Feed stdin from its own thread (dropping it signals EOF): a child that fills its stdout
    // pipe before draining stdin would otherwise deadlock against this write.
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let exceeded = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<(u8, Vec<u8>)>();
    let pipes: [(u8, Option<Box<dyn Read + Send>>); 2] = [
        (1, child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>)),
        (2, child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>)),
    ];
    let mut piped = [false; 3];
    for (which, pipe) in pipes {
        if let Some(mut pipe) = pipe {
            piped[which as usize] = true;
            let tx = tx.clone();
            let exceeded = Arc::clone(&exceeded);
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = vec![0u8; 65536];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&chunk[..n]);
                            if max_buffer.is_some_and(|max| buf.len() > max) {
                                exceeded.store(true, std::sync::atomic::Ordering::SeqCst);
                                #[cfg(unix)]
                                signal_pid(pid, kill_signal);
                                break;
                            }
                        }
                    }
                }
                let _ = tx.send((which, buf));
            });
        }
    }
    drop(tx);
    let wait_error = |ctx: &mut Ctx, e: std::io::Error| ctx.make_error("Error", format!("exec {cmd}: {e}"));
    let mut timed_out = false;
    let stop = |child: &mut Child| {
        #[cfg(unix)]
        signal_pid(child.id(), kill_signal);
        #[cfg(not(unix))]
        let _ = child.kill();
    };
    let interrupt = ctx.interrupt_for_host();
    let status = if timeout.is_none() && !cfg!(unix) && max_buffer.is_some()
        || timeout.is_some()
        || interrupt.is_some()
    {
        let started = std::time::Instant::now();
        loop {
            if let Some(status) = child.try_wait().map_err(|e| wait_error(ctx, e))? {
                break status;
            }
            if interrupt
                .as_ref()
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
            {
                stop(&mut child);
                let _ = child.wait();
                ctx.poll_interrupt_for_host()?;
            }
            if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                timed_out = true;
                stop(&mut child);
                break child.wait().map_err(|e| wait_error(ctx, e))?;
            }
            if !cfg!(unix) && exceeded.load(std::sync::atomic::Ordering::SeqCst) {
                stop(&mut child);
                break child.wait().map_err(|e| wait_error(ctx, e))?;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    } else {
        child.wait().map_err(|e| wait_error(ctx, e))?
    };
    // After a timeout, a grandchild may still hold the pipes: take what arrived, don't wait.
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    loop {
        let got = if timed_out || exceeded.load(std::sync::atomic::Ordering::SeqCst) {
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

    exec_result(
        ctx,
        ExecOutcome {
            stdout,
            stderr,
            piped,
            code: status.code(),
            signal: exit_signal(&status),
            pid,
            timed_out,
            exceeded: exceeded.load(std::sync::atomic::Ordering::SeqCst),
        },
    )
}

struct ExecOutcome {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    piped: [bool; 3],
    code: Option<i32>,
    signal: Option<i32>,
    pid: u32,
    timed_out: bool,
    exceeded: bool,
}

/// The object `execSync` / `spawnSync` read their result from.
fn exec_result(ctx: &mut Ctx, outcome: ExecOutcome) -> Result<Value, Value> {
    let o = Value::Obj(ctx.new_object());
    let stdout = if outcome.piped[1] { ctx.make_uint8array(&outcome.stdout)? } else { Value::Null };
    let stderr = if outcome.piped[2] { ctx.make_uint8array(&outcome.stderr)? } else { Value::Null };
    let number = |n: Option<i32>| n.map_or(Value::Null, |n| Value::Num(n as f64));
    let _ = ctx.set_member(&o, "stdout", stdout);
    let _ = ctx.set_member(&o, "stderr", stderr);
    let _ = ctx.set_member(&o, "status", number(outcome.code));
    let _ = ctx.set_member(&o, "signal", number(outcome.signal));
    let _ = ctx.set_member(&o, "pid", Value::Num(outcome.pid as f64));
    let _ = ctx.set_member(&o, "timedOut", Value::Bool(outcome.timed_out));
    let _ = ctx.set_member(&o, "maxBufferExceeded", Value::Bool(outcome.exceeded));
    Ok(o)
}



// ---- child realms -----------------------------------------------------------------------------

/// A pipe inside the process: what a child realm's stdin, stdout and stderr are when the child
/// is another realm and not an OS process. Bounded, so a producer outrunning its consumer waits
/// as it would on a kernel pipe.
mod mem_pipe {
    use std::collections::VecDeque;
    use std::io::{self, Read, Write};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex, PoisonError};
    use std::time::Duration;

    const CAPACITY: usize = 1 << 20;
    const POLL: Duration = Duration::from_millis(100);

    #[derive(Default)]
    struct State {
        bytes: VecDeque<u8>,
        writer_gone: bool,
        reader_gone: bool,
    }

    #[derive(Default)]
    struct Pipe {
        state: Mutex<State>,
        changed: Condvar,
    }

    impl Pipe {
        fn lock(&self) -> std::sync::MutexGuard<'_, State> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    pub struct Reader {
        pipe: Arc<Pipe>,
        stop: Option<Arc<AtomicBool>>,
    }

    pub struct Writer {
        pipe: Arc<Pipe>,
        stop: Option<Arc<AtomicBool>>,
    }

    /// Closes both ends of a pipe from outside, without the locks its reader and writer sit in:
    /// a blocked write fails with `BrokenPipe` and a blocked read ends at end-of-file.
    pub struct Handle(Arc<Pipe>);

    impl Handle {
        pub fn shut(&self) {
            let mut state = self.0.lock();
            state.reader_gone = true;
            state.writer_gone = true;
            self.0.changed.notify_all();
        }
    }

    /// `reader_stop` ends the reader's wait with end-of-file (the realm reading its stdin was
    /// stopped while the write end is still held by the parent); `writer_stop` ends a write that
    /// is waiting for room with `BrokenPipe` (the realm writing its stdout was stopped while the
    /// read end is still held by a parent that is not reading).
    pub fn pipe(
        reader_stop: Option<Arc<AtomicBool>>,
        writer_stop: Option<Arc<AtomicBool>>,
    ) -> (Reader, Writer, Handle) {
        let pipe = Arc::new(Pipe::default());
        (
            Reader {
                pipe: Arc::clone(&pipe),
                stop: reader_stop,
            },
            Writer {
                pipe: Arc::clone(&pipe),
                stop: writer_stop,
            },
            Handle(pipe),
        )
    }

    fn stopped(stop: &Option<Arc<AtomicBool>>) -> bool {
        stop.as_ref().is_some_and(|stop| stop.load(Ordering::SeqCst))
    }

    impl Read for Reader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if out.is_empty() {
                return Ok(0);
            }
            let mut state = self.pipe.lock();
            loop {
                if !state.bytes.is_empty() {
                    let count = out.len().min(state.bytes.len());
                    for (slot, byte) in out.iter_mut().zip(state.bytes.drain(..count)) {
                        *slot = byte;
                    }
                    self.pipe.changed.notify_all();
                    return Ok(count);
                }
                if state.writer_gone || stopped(&self.stop) {
                    return Ok(0);
                }
                state = self
                    .pipe
                    .changed
                    .wait_timeout(state, POLL)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }
    }

    impl Drop for Reader {
        fn drop(&mut self) {
            self.pipe.lock().reader_gone = true;
            self.pipe.changed.notify_all();
        }
    }

    impl Write for Writer {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            if data.is_empty() {
                return Ok(0);
            }
            let mut state = self.pipe.lock();
            loop {
                if state.reader_gone {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                let room = CAPACITY.saturating_sub(state.bytes.len());
                if room > 0 {
                    let count = room.min(data.len());
                    state.bytes.extend(&data[..count]);
                    self.pipe.changed.notify_all();
                    return Ok(count);
                }
                if stopped(&self.stop) {
                    return Err(io::ErrorKind::BrokenPipe.into());
                }
                state = self
                    .pipe
                    .changed
                    .wait_timeout(state, POLL)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Drop for Writer {
        fn drop(&mut self) {
            self.pipe.lock().writer_gone = true;
            self.pipe.changed.notify_all();
        }
    }
}

/// Wake every child realm of this realm out of a blocked read or write on its stdio pipes. The
/// realm calls this as it ends, before waiting for its children: nobody reads their output
/// any more.
pub(crate) fn close_child_pipes(ctx: &mut Ctx) {
    if let Some(registry) = ctx.host_mut::<ChildRegistry>() {
        for proc in registry.procs.values() {
            for pipe in &proc.pipes {
                pipe.shut();
            }
        }
    }
}

/// Writes to the realm's own stdout or stderr: a child that inherits them.
struct Inherited(Arc<Mutex<Box<dyn Write + Send>>>);

impl Write for Inherited {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut writer = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        writer.write_all(bytes)?;
        writer.flush()?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Reads the realm's own stdin: a child that inherits it. The source is shared and one reader at
/// a time may wait on it holding the lock, so a second reader polls for the lock and gives up
/// with end-of-file when its own realm is stopped, instead of queueing behind a read that may
/// never return.
struct InheritedStdin(Arc<Mutex<Box<dyn Read + Send>>>, Arc<std::sync::atomic::AtomicBool>);

impl Read for InheritedStdin {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.1.load(std::sync::atomic::Ordering::SeqCst) {
                return Ok(0);
            }
            match self.0.try_lock() {
                Ok(mut source) => return source.read(out),
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    return poisoned.into_inner().read(out)
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
    }
}

/// What `spawn(process.execPath, ...)` needs from the realm making the call.
struct RealmSite {
    launcher: Arc<dyn lumen_host::RealmLauncher>,
    stdin: Arc<Mutex<Box<dyn Read + Send>>>,
    stdout: Arc<Mutex<Box<dyn Write + Send>>>,
    stderr: Arc<Mutex<Box<dyn Write + Send>>>,
    cwd: std::path::PathBuf,
    /// The calling realm's own stop request.
    interrupt: Arc<std::sync::atomic::AtomicBool>,
}

/// The calling realm when `cmd` is its own `process.execPath` and it can launch child realms.
fn realm_site(ctx: &mut Ctx, cmd: &str) -> Option<RealmSite> {
    ctx.op_state().get::<lumen_host::RealmProcess>().and_then(|realm| {
        Some(RealmSite {
            launcher: realm.launcher.clone().filter(|_| realm.exec_path == cmd)?,
            stdin: Arc::clone(&realm.stdin),
            stdout: Arc::clone(&realm.stdout),
            stderr: Arc::clone(&realm.stderr),
            cwd: realm.cwd.clone(),
            interrupt: Arc::clone(&realm.interrupt),
        })
    })
}

fn read_env_pairs(ctx: &mut Ctx, pairs: &Value) -> Result<Vec<(String, String)>, Value> {
    let mut env = Vec::new();
    if pairs.as_obj().is_some() {
        if let Ok(Value::Num(count)) = ctx.get_member(pairs, "length") {
            for index in 0..(count as usize) {
                let pair = ctx
                    .get_member(pairs, &index.to_string())
                    .unwrap_or(Value::Undefined);
                let key = ctx.get_member(&pair, "0").unwrap_or(Value::Undefined);
                let value = ctx.get_member(&pair, "1").unwrap_or(Value::Undefined);
                env.push((
                    ctx.coerce_string(&key)?.to_string(),
                    ctx.coerce_string(&value)?.to_string(),
                ));
            }
        }
    }
    Ok(env)
}

/// Options that take the next argument as their value (`--opt=value` carries its own).
const OPTIONS_WITH_VALUE: &[&str] = &[
    "-r",
    "--require",
    "--import",
    "--loader",
    "--experimental-loader",
    "-C",
    "--conditions",
    "--inspect-port",
    "--input-type",
];

/// Where the script is in `[...execArgv, script, ...args]`.
fn locate_script(args: &[String]) -> Result<usize, String> {
    let mut at = 0;
    while at < args.len() {
        let arg = args[at].as_str();
        let name = arg.split('=').next().unwrap_or(arg);
        match arg {
            "--" => {
                return if at + 1 < args.len() {
                    Ok(at + 1)
                } else {
                    Err("no script to run".to_string())
                }
            }
            "-" => return Err("a child realm cannot run a script from stdin".to_string()),
            _ if !arg.starts_with('-') => return Ok(at),
            _ if matches!(name, "-e" | "--eval" | "-p" | "--print" | "-pe") => {
                return Err(
                    "a child realm runs a script file; -e/--eval and -p/--print are not supported"
                        .to_string(),
                )
            }
            _ if OPTIONS_WITH_VALUE.contains(&arg) => at += 2,
            _ => at += 1,
        }
    }
    Err("no script to run".to_string())
}

/// A child realm's stdio, wired to its parent.
struct Wired {
    request: ChildRealmRequest,
    parent_stdin: Option<Box<dyn Write + Send>>,
    parent_stdout: Option<Box<dyn Read + Send>>,
    parent_stderr: Option<Box<dyn Read + Send>>,
    extra: HashMap<u32, ExtraPipe>,
    pipes: Vec<mem_pipe::Handle>,
    ipc_fd: Option<i32>,
}

/// Build what a child realm runs with from a `spawn(process.execPath, args, ...)`: the script
/// (`argv` is `[execPath, script, ...args]`; `execArgv` is dropped), its working directory,
/// environment and stdio. `pipe` becomes an in-process pipe, `inherit` the realm's own stream
/// and `ignore` an empty or discarded one. The IPC slot is a socketpair whose child end is a real
/// descriptor number the child reads from `LUMEN_FORK_IPC_FD`, as in a forked process; only that
/// slot's number is rewritten there. The other `pipe` slots are unreachable from inside the
/// child, which has no process of its own to inherit them into.
fn wire_child_realm(
    site: &RealmSite,
    cmd: &str,
    arg_list: &[String],
    cwd: Option<String>,
    mut env: Vec<(String, String)>,
    mut stdio: Vec<String>,
) -> Result<Wired, String> {
    while stdio.len() < 3 {
        stdio.push("pipe".to_string());
    }
    let cwd = match cwd {
        Some(requested) => {
            let requested = std::path::PathBuf::from(requested);
            if requested.is_absolute() {
                requested
            } else {
                site.cwd.join(requested)
            }
        }
        None => site.cwd.clone(),
    };
    let script_at = locate_script(arg_list)?;
    let script = std::path::PathBuf::from(&arg_list[script_at]);
    let script = if script.is_absolute() { script } else { cwd.join(script) };
    let mut argv = vec![cmd.to_string(), script.to_string_lossy().into_owned()];
    argv.extend(arg_list[script_at + 1..].iter().cloned());

    let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut pipes = Vec::new();
    let mut parent_stdin: Option<Box<dyn Write + Send>> = None;
    let mut parent_stdout: Option<Box<dyn Read + Send>> = None;
    let mut parent_stderr: Option<Box<dyn Read + Send>> = None;

    let stdin: Box<dyn Read + Send> = match stdio[0].as_str() {
        "pipe" => {
            let (reader, writer, handle) = mem_pipe::pipe(Some(Arc::clone(&interrupt)), None);
            parent_stdin = Some(Box::new(writer));
            pipes.push(handle);
            Box::new(reader)
        }
        "inherit" => Box::new(InheritedStdin(Arc::clone(&site.stdin), Arc::clone(&interrupt))),
        _ => Box::new(std::io::empty()),
    };
    let mut output = |kind: &str,
                      parent: &mut Option<Box<dyn Read + Send>>,
                      own: &Arc<Mutex<Box<dyn Write + Send>>>|
     -> Box<dyn Write + Send> {
        match kind {
            "pipe" => {
                let (reader, writer, handle) = mem_pipe::pipe(None, Some(Arc::clone(&interrupt)));
                *parent = Some(Box::new(reader));
                pipes.push(handle);
                Box::new(writer)
            }
            "inherit" => Box::new(Inherited(Arc::clone(own))),
            _ => Box::new(std::io::sink()),
        }
    };
    let stdout = output(&stdio[1], &mut parent_stdout, &site.stdout);
    let stderr = output(&stdio[2], &mut parent_stderr, &site.stderr);

    // The program's own entries come last: an inherited one from this realm's launcher may precede.
    let ipc_slot = env
        .iter()
        .rev()
        .find(|(key, _)| key == "LUMEN_FORK_IPC_FD" || key == "NODE_CHANNEL_FD")
        .and_then(|(_, value)| value.parse::<usize>().ok());
    let mut extra = HashMap::new();
    let mut owned_fds = Vec::new();
    let mut resources: Vec<Box<dyn Send>> = Vec::new();
    let mut ipc_fd = None;
    for (slot, kind) in stdio.iter().enumerate().skip(3) {
        if kind != "pipe" && kind != "ipc" {
            continue;
        }
        #[cfg(unix)]
        {
            let (parent, child) = socket_pair().map_err(|e| e.to_string())?;
            if kind == "ipc" {
                // As for an OS child: the parent's end goes to JS as a raw descriptor (net's adoptFd).
                ipc_fd = Some(std::os::fd::IntoRawFd::into_raw_fd(parent));
            } else {
                let reader: Box<dyn Read + Send> =
                    Box::new(parent.try_clone().map_err(|e| e.to_string())?);
                extra.insert(
                    slot as u32,
                    ExtraPipe {
                        reader: Arc::new(Mutex::new(Some(reader))),
                        writer: Arc::new(Mutex::new(Some(Box::new(parent)))),
                    },
                );
            }
            let raw = std::os::unix::io::AsRawFd::as_raw_fd(&child);
            owned_fds.push(raw);
            if ipc_slot == Some(slot) || kind == "ipc" {
                for (key, value) in env.iter_mut() {
                    if key == "LUMEN_FORK_IPC_FD" || key == "NODE_CHANNEL_FD" {
                        *value = raw.to_string();
                    }
                }
            }
            if kind == "ipc" {
                // The child realm adopts this end (net's adoptFd) and closes it with its channel.
                let _ = std::os::fd::IntoRawFd::into_raw_fd(child);
            } else {
                resources.push(Box::new(child));
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (slot, ipc_slot, &mut env);
            return Err("extra stdio pipes (stdio[3] and up) are only supported on unix".to_string());
        }
    }
    Ok(Wired {
        request: ChildRealmRequest {
            argv,
            env,
            cwd,
            stdin,
            stdout,
            stderr,
            interrupt,
            owned_fds,
            resources,
        },
        parent_stdin,
        parent_stdout,
        parent_stderr,
        extra,
        pipes,
        ipc_fd,
    })
}

/// `spawn(process.execPath, [...execArgv, script, ...args])` (what `fork` is) in an embedded
/// realm: the script runs as a child realm in this process. `Ok(None)` when `cmd` is anything else,
/// or the realm cannot launch one, and the ordinary OS spawn follows.
fn spawn_child_realm(ctx: &mut Ctx, cmd: &str, args: &[Value]) -> Result<Option<Value>, Value> {
    let Some(site) = realm_site(ctx, cmd) else {
        return Ok(None);
    };
    let arg_list = read_string_array(ctx, args.get(1).unwrap_or(&Value::Undefined))?;
    let cwd = opt_string(ctx, args.get(2));
    let env = read_env_pairs(ctx, args.get(3).unwrap_or(&Value::Undefined))?;
    let stdio = read_string_array(ctx, args.get(4).unwrap_or(&Value::Undefined))?;
    let wired = wire_child_realm(&site, cmd, &arg_list, cwd, env, stdio)
        .map_err(|e| ctx.make_error("Error", format!("spawn {cmd}: {e}")))?;
    let Wired {
        request,
        parent_stdin,
        parent_stdout,
        parent_stderr,
        extra,
        pipes,
        ipc_fd,
    } = wired;
    let realm = site
        .launcher
        .launch(request)
        .map_err(|e| spawn_failure(ctx, cmd, &e))?;
    let pid = realm.pid();
    let proc = ChildProc {
        stdin: Arc::new(Mutex::new(parent_stdin)),
        stdout: Arc::new(Mutex::new(parent_stdout)),
        stderr: Arc::new(Mutex::new(parent_stderr)),
        extra,
        child: Arc::new(Mutex::new(ProcHandle::Realm(realm))),
        unref: false,
        pending_tasks: Vec::new(),
        pipes,
    };
    let info = register_child(ctx, proc, pid);
    if let Some(fd) = ipc_fd {
        let _ = ctx.set_member(&info, "ipcFd", Value::Num(fd as f64));
    }
    Ok(Some(info))
}

/// How a synchronous `spawnSync` / `execSync` of `process.execPath` is run.
struct SyncRealm {
    input: Option<Vec<u8>>,
    modes: [String; 3],
    max_buffer: Option<usize>,
    kill_signal: i32,
    timeout: Option<std::time::Duration>,
}

/// Run the script as a child realm and block this realm's thread until it ends, collecting its
/// piped output up to `maxBuffer`. A timeout or an output past `maxBuffer` sends `killSignal`
/// (and `SIGKILL` if the child has not ended two seconds later); the calling realm being stopped
/// stops the child.
fn exec_sync_in_realm(
    ctx: &mut Ctx,
    site: RealmSite,
    cmd: &str,
    arg_list: &[String],
    cwd: Option<String>,
    env: Vec<(String, String)>,
    sync: SyncRealm,
) -> Result<Value, Value> {
    use std::sync::atomic::Ordering;
    let SyncRealm { input, modes, max_buffer, kill_signal, timeout } = sync;
    let stdio = vec![
        match modes[0].as_str() {
            "inherit" => "inherit",
            "pipe" if input.is_some() => "pipe",
            _ => "ignore",
        }
        .to_string(),
        modes[1].clone(),
        modes[2].clone(),
    ];
    let mut wired = wire_child_realm(&site, cmd, arg_list, cwd, env, stdio)
        .map_err(|e| ctx.make_error("Error", format!("spawn {cmd}: {e}")))?;
    let (parent_stdin, parent_stdout, parent_stderr) = (
        wired.parent_stdin.take(),
        wired.parent_stdout.take(),
        wired.parent_stderr.take(),
    );
    let realm = site
        .launcher
        .launch(wired.request)
        .map_err(|e| spawn_failure(ctx, cmd, &e))?;
    let pid = realm.pid();
    if let (Some(input), Some(mut stdin)) = (input, parent_stdin) {
        std::thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
    }
    let exceeded = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (tx, rx) = std::sync::mpsc::channel::<(u8, Vec<u8>)>();
    let mut piped = [false; 3];
    for (which, pipe) in [(1u8, parent_stdout), (2u8, parent_stderr)] {
        let Some(mut pipe) = pipe else { continue };
        piped[which as usize] = true;
        let (tx, exceeded) = (tx.clone(), Arc::clone(&exceeded));
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = vec![0u8; 65536];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if max_buffer.is_some_and(|max| buf.len() > max) {
                            exceeded.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            }
            let _ = tx.send((which, buf));
        });
    }
    drop(tx);
    let started = std::time::Instant::now();
    let mut stopped_at: Option<std::time::Instant> = None;
    let mut timed_out = false;
    let exit = loop {
        if let Some(exit) = realm.exit() {
            break exit;
        }
        if site.interrupt.load(Ordering::SeqCst) {
            realm.terminate(9);
        }
        if stopped_at.is_none() {
            if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                timed_out = true;
            }
            if timed_out || exceeded.load(Ordering::SeqCst) {
                realm.signal(kill_signal);
                stopped_at = Some(std::time::Instant::now());
            }
        } else if stopped_at.is_some_and(|at| at.elapsed() >= std::time::Duration::from_secs(2)) {
            realm.terminate(9);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    };
    let exceeded = exceeded.load(Ordering::SeqCst);
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    loop {
        let got = if timed_out || exceeded {
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
    let (code, signal) = match exit {
        ChildRealmExit::Exited(code) => (Some(code), None),
        ChildRealmExit::Signalled(signal) => (None, Some(signal)),
    };
    exec_result(
        ctx,
        ExecOutcome { stdout, stderr, piped, code, signal, pid, timed_out, exceeded },
    )
}

/// A connected pair of stream sockets for a child realm's IPC channel: the parent keeps the first,
/// the child reads and writes the second by descriptor number. Both are close-on-exec, so no
/// subprocess either realm starts inherits them.
#[cfg(unix)]
fn socket_pair() -> std::io::Result<(std::os::unix::net::UnixStream, std::os::unix::net::UnixStream)> {
    use std::os::unix::io::AsRawFd;
    let (parent, child) = std::os::unix::net::UnixStream::pair()?;
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    {
        let enabled: libc::c_int = 1;
        // SAFETY: a valid socket and a correctly sized int option.
        let status = unsafe {
            libc::setsockopt(
                child.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&enabled as *const libc::c_int).cast(),
                std::mem::size_of_val(&enabled) as libc::socklen_t,
            )
        };
        if status < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    let _ = child.as_raw_fd();
    Ok((parent, child))
}

#[cfg(test)]
mod tests {
    use super::locate_script;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn the_script_follows_options_and_their_values() {
        assert_eq!(locate_script(&args(&["a.js", "b"])), Ok(0));
        assert_eq!(locate_script(&args(&["--no-warnings", "a.js"])), Ok(1));
        assert_eq!(locate_script(&args(&["-r", "x", "a.js"])), Ok(2));
        assert_eq!(locate_script(&args(&["--require=x", "a.js"])), Ok(1));
        assert_eq!(locate_script(&args(&["--import", "x", "-C", "dev", "--input-type", "module", "a.js"])), Ok(6));
        assert_eq!(locate_script(&args(&["--inspect-port", "9", "--loader", "l", "a.js"])), Ok(4));
        assert_eq!(locate_script(&args(&["--", "-weird.js"])), Ok(1));
    }

    #[test]
    fn eval_and_missing_scripts_are_refused() {
        assert!(locate_script(&args(&["-e", "1"])).is_err());
        assert!(locate_script(&args(&["--eval=1"])).is_err());
        assert!(locate_script(&args(&["-p", "1"])).is_err());
        assert!(locate_script(&args(&["-r", "x"])).is_err());
        assert!(locate_script(&args(&["-"])).is_err());
        assert!(locate_script(&args(&[])).is_err());
    }
}
