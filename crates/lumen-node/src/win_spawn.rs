//! Windows spawn for children that get extra stdio slots (`stdio[3]` and up) — what a browser's
//! `--remote-debugging-pipe` talks on (Puppeteer reads the browser on fd 4, writes it on fd 3).
//!
//! `std::process::Command` cannot hand a child more than the three standard handles, so this
//! path calls `CreateProcessW` itself and passes the extra slots the way libuv (and therefore
//! Node) does: the C runtime's inheritance block in `STARTUPINFOW.lpReserved2`, which the child's
//! CRT reads at startup to populate its fd table — `_get_osfhandle(3)` in the child then yields
//! the handle we passed for slot 3. Only the handles listed in a
//! `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` are inherited, so the child does not pick up whatever else
//! happens to be inheritable in this process.
//!
//! Each extra slot is a duplex named pipe, as in libuv: the child's end is an ordinary
//! synchronous handle, the parent's end is opened for overlapped I/O so a blocked read on the
//! slot does not serialize a concurrent write to it (synchronous handles share one lock per file
//! object, and `child_process` keeps a read pending on every extra slot).

#![allow(non_snake_case, clippy::upper_case_acronyms)]

use std::ffi::{c_void, OsStr};
use std::io::{self, Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

type HANDLE = *mut c_void;
type BOOL = i32;
type DWORD = u32;

const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;
const GENERIC_READ: DWORD = 0x8000_0000;
const GENERIC_WRITE: DWORD = 0x4000_0000;
const FILE_SHARE_READ: DWORD = 1;
const FILE_SHARE_WRITE: DWORD = 2;
const OPEN_EXISTING: DWORD = 3;
const PIPE_ACCESS_DUPLEX: DWORD = 3;
const FILE_FLAG_OVERLAPPED: DWORD = 0x4000_0000;
const FILE_FLAG_FIRST_PIPE_INSTANCE: DWORD = 0x0008_0000;
const PIPE_REJECT_REMOTE_CLIENTS: DWORD = 8;
const STD_INPUT_HANDLE: DWORD = -10i32 as DWORD;
const STD_OUTPUT_HANDLE: DWORD = -11i32 as DWORD;
const STD_ERROR_HANDLE: DWORD = -12i32 as DWORD;
const DUPLICATE_SAME_ACCESS: DWORD = 2;
const STARTF_USESTDHANDLES: DWORD = 0x100;
const CREATE_UNICODE_ENVIRONMENT: DWORD = 0x400;
const EXTENDED_STARTUPINFO_PRESENT: DWORD = 0x0008_0000;
const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;
const WAIT_OBJECT_0: DWORD = 0;
const ERROR_BROKEN_PIPE: i32 = 109;
const ERROR_PIPE_NOT_CONNECTED: i32 = 233;
const ERROR_IO_PENDING: i32 = 997;
const ERROR_OPERATION_ABORTED: i32 = 995;
// C runtime fd flags in the lpReserved2 inheritance block (ucrt's `FOPEN`, `FPIPE`, `FDEV`).
const FOPEN: u8 = 0x01;
const FPIPE: u8 = 0x08;
const FDEV: u8 = 0x40;

#[repr(C)]
struct SECURITY_ATTRIBUTES {
    nLength: DWORD,
    lpSecurityDescriptor: *mut c_void,
    bInheritHandle: BOOL,
}

#[repr(C)]
struct STARTUPINFOW {
    cb: DWORD,
    lpReserved: *mut u16,
    lpDesktop: *mut u16,
    lpTitle: *mut u16,
    dwX: DWORD,
    dwY: DWORD,
    dwXSize: DWORD,
    dwYSize: DWORD,
    dwXCountChars: DWORD,
    dwYCountChars: DWORD,
    dwFillAttribute: DWORD,
    dwFlags: DWORD,
    wShowWindow: u16,
    cbReserved2: u16,
    lpReserved2: *mut u8,
    hStdInput: HANDLE,
    hStdOutput: HANDLE,
    hStdError: HANDLE,
}

#[repr(C)]
struct STARTUPINFOEXW {
    StartupInfo: STARTUPINFOW,
    lpAttributeList: *mut c_void,
}

#[repr(C)]
struct PROCESS_INFORMATION {
    hProcess: HANDLE,
    hThread: HANDLE,
    dwProcessId: DWORD,
    dwThreadId: DWORD,
}

#[repr(C)]
struct OVERLAPPED {
    Internal: usize,
    InternalHigh: usize,
    Offset: DWORD,
    OffsetHigh: DWORD,
    hEvent: HANDLE,
}

#[link(name = "kernel32")]
extern "system" {
    fn CreateProcessW(
        app: *const u16,
        cmd: *mut u16,
        pa: *mut SECURITY_ATTRIBUTES,
        ta: *mut SECURITY_ATTRIBUTES,
        inherit: BOOL,
        flags: DWORD,
        env: *mut c_void,
        cwd: *const u16,
        si: *mut STARTUPINFOW,
        pi: *mut PROCESS_INFORMATION,
    ) -> BOOL;
    fn CreatePipe(r: *mut HANDLE, w: *mut HANDLE, sa: *mut SECURITY_ATTRIBUTES, size: DWORD) -> BOOL;
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: DWORD,
        pipe_mode: DWORD,
        max_instances: DWORD,
        out_size: DWORD,
        in_size: DWORD,
        timeout: DWORD,
        sa: *mut SECURITY_ATTRIBUTES,
    ) -> HANDLE;
    fn CreateFileW(
        name: *const u16,
        access: DWORD,
        share: DWORD,
        sa: *mut SECURITY_ATTRIBUTES,
        disposition: DWORD,
        flags: DWORD,
        template: HANDLE,
    ) -> HANDLE;
    fn ReadFile(h: HANDLE, buf: *mut u8, n: DWORD, read: *mut DWORD, ov: *mut OVERLAPPED) -> BOOL;
    fn WriteFile(h: HANDLE, buf: *const u8, n: DWORD, written: *mut DWORD, ov: *mut OVERLAPPED) -> BOOL;
    fn GetOverlappedResult(h: HANDLE, ov: *mut OVERLAPPED, n: *mut DWORD, wait: BOOL) -> BOOL;
    fn CreateEventW(sa: *mut SECURITY_ATTRIBUTES, manual: BOOL, initial: BOOL, name: *const u16) -> HANDLE;
    fn CloseHandle(h: HANDLE) -> BOOL;
    fn GetStdHandle(which: DWORD) -> HANDLE;
    fn GetCurrentProcess() -> HANDLE;
    fn GetCurrentProcessId() -> DWORD;
    fn DuplicateHandle(
        src_proc: HANDLE,
        src: HANDLE,
        dst_proc: HANDLE,
        dst: *mut HANDLE,
        access: DWORD,
        inherit: BOOL,
        options: DWORD,
    ) -> BOOL;
    fn WaitForSingleObject(h: HANDLE, ms: DWORD) -> DWORD;
    fn GetExitCodeProcess(h: HANDLE, code: *mut DWORD) -> BOOL;
    fn TerminateProcess(h: HANDLE, code: u32) -> BOOL;
    fn InitializeProcThreadAttributeList(
        list: *mut c_void,
        count: DWORD,
        flags: DWORD,
        size: *mut usize,
    ) -> BOOL;
    fn UpdateProcThreadAttribute(
        list: *mut c_void,
        flags: DWORD,
        attribute: usize,
        value: *mut c_void,
        size: usize,
        prev: *mut c_void,
        ret_size: *mut usize,
    ) -> BOOL;
    fn DeleteProcThreadAttributeList(list: *mut c_void);
}

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn last_error() -> io::Error {
    io::Error::last_os_error()
}

/// An owned kernel handle, closed on drop.
struct Owned(HANDLE);
// SAFETY: a Windows kernel handle is a process-wide value usable from any thread.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
            // SAFETY: we own this handle and close it exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

fn inheritable_sa() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as DWORD,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    }
}

/// One direction of the parent's overlapped end of a duplex pipe. Each half owns its own event,
/// so the read and write halves can have I/O in flight at the same time.
struct OvHalf {
    pipe: Arc<Owned>,
    event: Owned,
}

impl OvHalf {
    fn new(pipe: Arc<Owned>) -> io::Result<OvHalf> {
        // SAFETY: plain Win32 call; a manual-reset event, initially unsignalled.
        let ev = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
        if ev.is_null() {
            return Err(last_error());
        }
        Ok(OvHalf { pipe, event: Owned(ev) })
    }

    /// Issue one overlapped transfer and wait for it. Broken/disconnected pipe reads as EOF.
    fn transfer(&self, read: bool, buf: *mut u8, len: usize) -> io::Result<usize> {
        let len = len.min(u32::MAX as usize) as DWORD;
        let mut ov = OVERLAPPED {
            Internal: 0,
            InternalHigh: 0,
            Offset: 0,
            OffsetHigh: 0,
            hEvent: self.event.0,
        };
        let mut n: DWORD = 0;
        // SAFETY: `buf` is valid for `len` bytes and `ov` outlives the operation (we wait for it
        // with GetOverlappedResult before returning).
        let ok = unsafe {
            if read {
                ReadFile(self.pipe.0, buf, len, std::ptr::null_mut(), &mut ov)
            } else {
                WriteFile(self.pipe.0, buf, len, std::ptr::null_mut(), &mut ov)
            }
        };
        if ok == 0 {
            let err = last_error();
            if err.raw_os_error() != Some(ERROR_IO_PENDING) {
                return eof_or(err);
            }
        }
        // SAFETY: waits for the operation started above on the same OVERLAPPED.
        if unsafe { GetOverlappedResult(self.pipe.0, &mut ov, &mut n, 1) } == 0 {
            return eof_or(last_error());
        }
        Ok(n as usize)
    }
}

fn eof_or(err: io::Error) -> io::Result<usize> {
    match err.raw_os_error() {
        Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_OPERATION_ABORTED) => Ok(0),
        _ => Err(err),
    }
}

pub struct PipeReader(OvHalf);
pub struct PipeWriter(OvHalf);

impl Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.0.transfer(true, buf.as_mut_ptr(), buf.len())
    }
}

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = self.0.transfer(false, buf.as_ptr() as *mut u8, buf.len())?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        }
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A duplex named pipe: the parent's overlapped end (split into read/write halves) and the
/// child's synchronous, inheritable end.
fn duplex_pipe() -> io::Result<(PipeReader, PipeWriter, Owned)> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let mut nonce = [0u8; 8];
    let _ = lumen_host::fill_random(&mut nonce);
    let name = format!(
        r"\\.\pipe\lumen-{}-{}-{:016x}",
        // SAFETY: plain Win32 call with no arguments.
        unsafe { GetCurrentProcessId() },
        COUNTER.fetch_add(1, Ordering::Relaxed),
        u64::from_le_bytes(nonce)
    );
    let wname = wide(OsStr::new(&name));
    // SAFETY: plain Win32 calls on a NUL-terminated name we own; results are checked.
    let server = unsafe {
        CreateNamedPipeW(
            wname.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_REJECT_REMOTE_CLIENTS,
            1,
            65536,
            65536,
            0,
            std::ptr::null_mut(),
        )
    };
    if server == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    let server = Arc::new(Owned(server));
    let mut sa = inheritable_sa();
    // SAFETY: as above; the client end is inheritable so the child can receive it.
    let client = unsafe {
        CreateFileW(
            wname.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            0,
            &mut sa,
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if client == INVALID_HANDLE_VALUE {
        return Err(last_error());
    }
    let client = Owned(client);
    // Opening the client connects the (single) server instance; no ConnectNamedPipe needed.
    Ok((
        PipeReader(OvHalf::new(server.clone())?),
        PipeWriter(OvHalf::new(server)?),
        client,
    ))
}

/// A spawned process: waitable and killable.
pub struct RawChild {
    process: Owned,
    pid: u32,
}

impl RawChild {
    pub fn id(&self) -> u32 {
        self.pid
    }

    /// `Some(exit code)` once the process has exited.
    pub fn try_wait(&mut self) -> io::Result<Option<i32>> {
        // SAFETY: waiting on / querying a process handle we own.
        unsafe {
            if WaitForSingleObject(self.process.0, 0) != WAIT_OBJECT_0 {
                return Ok(None);
            }
            let mut code: DWORD = 0;
            if GetExitCodeProcess(self.process.0, &mut code) == 0 {
                return Err(last_error());
            }
            Ok(Some(code as i32))
        }
    }

    pub fn kill(&mut self) -> io::Result<()> {
        if matches!(self.try_wait(), Ok(Some(_))) {
            return Ok(());
        }
        // SAFETY: terminating a process handle we own.
        if unsafe { TerminateProcess(self.process.0, 1) } == 0 {
            return Err(last_error());
        }
        Ok(())
    }
}

/// The parent's ends of a spawned child's stdio.
pub struct Spawned {
    pub child: RawChild,
    pub stdin: Option<Box<dyn Write + Send>>,
    pub stdout: Option<Box<dyn Read + Send>>,
    pub stderr: Option<Box<dyn Read + Send>>,
    /// `(fd, reader, writer)` for each extra `"pipe"` slot.
    pub extra: Vec<(u32, Box<dyn Read + Send>, Box<dyn Write + Send>)>,
}

pub struct SpawnSpec<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub verbatim: bool,
    pub cwd: Option<PathBuf>,
    /// `Some` replaces the environment; `None` inherits this process's.
    pub env: Option<Vec<(String, String)>>,
    /// `"pipe" | "inherit" | "ignore"` per fd.
    pub stdio: &'a [String],
}

/// Quote one argument the way `CommandLineToArgvW` / the MSVC CRT parse it back (std's rules).
fn append_arg(cmd: &mut String, arg: &str, force_quotes: bool) {
    let quote = force_quotes || arg.is_empty() || arg.contains([' ', '\t']);
    if quote {
        cmd.push('"');
    }
    let mut backslashes = 0usize;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
        } else {
            if c == '"' {
                // Double the run of backslashes before a quote, then escape the quote itself.
                cmd.extend(std::iter::repeat_n('\\', backslashes + 1));
            }
            backslashes = 0;
        }
        cmd.push(c);
    }
    if quote {
        // Trailing backslashes precede the closing quote: double them.
        cmd.extend(std::iter::repeat_n('\\', backslashes));
        cmd.push('"');
    }
}

/// Resolve the program like `CreateProcess` users expect: a path is taken as given (with `.exe`
/// appended when that is what exists); a bare name is searched on the child's `PATH`.
fn resolve_program(program: &str, env: &Option<Vec<(String, String)>>, cwd: &Option<PathBuf>) -> PathBuf {
    let with_exe = |p: &Path| -> Option<PathBuf> {
        if p.is_file() {
            return Some(p.to_path_buf());
        }
        if p.extension().is_none() {
            let e = p.with_extension("exe");
            if e.is_file() {
                return Some(e);
            }
        }
        None
    };
    let p = Path::new(program);
    if program.contains(['/', '\\']) || p.is_absolute() {
        let full = match cwd {
            Some(c) if p.is_relative() => c.join(p),
            _ => p.to_path_buf(),
        };
        return with_exe(&full).unwrap_or(full);
    }
    let path_var = match env {
        Some(pairs) => pairs
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
            .map(|(_, v)| std::ffi::OsString::from(v)),
        None => std::env::var_os("PATH"),
    };
    if let Some(path_var) = path_var {
        for dir in std::env::split_paths(&path_var) {
            if let Some(found) = with_exe(&dir.join(program)) {
                return found;
            }
        }
    }
    p.to_path_buf()
}

/// The child's end of a standard slot, plus the parent's end when it is a pipe.
enum StdSlot {
    Pipe { child: Owned, parent: std::fs::File },
    Child(Owned),
    None,
}

fn std_slot(kind: &str, fd: u32) -> io::Result<StdSlot> {
    match kind {
        "inherit" => {
            let which = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE][fd as usize];
            // SAFETY: duplicating our own standard handle into an inheritable copy.
            unsafe {
                let h = GetStdHandle(which);
                if h.is_null() || h == INVALID_HANDLE_VALUE {
                    return Ok(StdSlot::None);
                }
                let mut dup: HANDLE = std::ptr::null_mut();
                if DuplicateHandle(GetCurrentProcess(), h, GetCurrentProcess(), &mut dup, 0, 1, DUPLICATE_SAME_ACCESS) == 0 {
                    return Ok(StdSlot::None);
                }
                Ok(StdSlot::Child(Owned(dup)))
            }
        }
        "ignore" => {
            let mut sa = inheritable_sa();
            let name = wide(OsStr::new("NUL"));
            // SAFETY: opening the null device with an inheritable handle.
            let h = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    &mut sa,
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if h == INVALID_HANDLE_VALUE {
                return Err(last_error());
            }
            Ok(StdSlot::Child(Owned(h)))
        }
        _ => {
            // An anonymous pipe; only the child's end is inheritable.
            let mut r: HANDLE = std::ptr::null_mut();
            let mut w: HANDLE = std::ptr::null_mut();
            // SAFETY: plain Win32 calls; results are checked.
            unsafe {
                if CreatePipe(&mut r, &mut w, std::ptr::null_mut(), 0) == 0 {
                    return Err(last_error());
                }
                let (child_src, parent) = if fd == 0 { (r, w) } else { (w, r) };
                let mut child: HANDLE = std::ptr::null_mut();
                let dup_ok = DuplicateHandle(
                    GetCurrentProcess(),
                    child_src,
                    GetCurrentProcess(),
                    &mut child,
                    0,
                    1,
                    DUPLICATE_SAME_ACCESS,
                );
                CloseHandle(child_src);
                if dup_ok == 0 {
                    let e = last_error();
                    CloseHandle(parent);
                    return Err(e);
                }
                Ok(StdSlot::Pipe {
                    child: Owned(child),
                    parent: std::fs::File::from_raw_handle(parent as _),
                })
            }
        }
    }
}

pub fn spawn(spec: &SpawnSpec) -> io::Result<Spawned> {
    let program = resolve_program(spec.program, &spec.env, &spec.cwd);
    if !program.is_file() {
        return Err(io::Error::from(io::ErrorKind::NotFound));
    }

    let mut cmdline = String::new();
    append_arg(&mut cmdline, &program.to_string_lossy(), true);
    for arg in spec.args {
        cmdline.push(' ');
        if spec.verbatim {
            cmdline.push_str(arg);
        } else {
            append_arg(&mut cmdline, arg, false);
        }
    }
    let mut wcmd = wide(OsStr::new(&cmdline));
    let wprog = wide(program.as_os_str());
    let wcwd = spec.cwd.as_ref().map(|c| wide(c.as_os_str()));
    let mut wenv: Option<Vec<u16>> = spec.env.as_ref().map(|pairs| {
        let mut block = Vec::new();
        for (k, v) in pairs {
            block.extend(OsStr::new(&format!("{k}={v}")).encode_wide());
            block.push(0);
        }
        if pairs.is_empty() {
            block.push(0);
        }
        block.push(0);
        block
    });

    // Standard slots, then the extra ones.
    let mut slots = Vec::new();
    for fd in 0..3u32 {
        slots.push(std_slot(spec.stdio.get(fd as usize).map_or("pipe", String::as_str), fd)?);
    }
    let mut extra_parent = Vec::new();
    let mut extra_child: Vec<(u32, Owned)> = Vec::new();
    for (fd, kind) in spec.stdio.iter().enumerate().skip(3) {
        if kind == "pipe" {
            let (r, w, child) = duplex_pipe()?;
            extra_parent.push((fd as u32, r, w));
            extra_child.push((fd as u32, child));
        }
    }

    let child_handle = |s: &StdSlot| -> HANDLE {
        match s {
            StdSlot::Pipe { child, .. } | StdSlot::Child(child) => child.0,
            StdSlot::None => std::ptr::null_mut(),
        }
    };

    // The CRT inheritance block: int count; u8 flags[count]; HANDLE handles[count] (packed).
    let count = spec.stdio.len().max(3);
    let hsize = std::mem::size_of::<HANDLE>();
    let mut crt = vec![0u8; 4 + count * (1 + hsize)];
    crt[..4].copy_from_slice(&(count as i32).to_le_bytes());
    for fd in 0..count {
        let (handle, flags) = if fd < 3 {
            match &slots[fd] {
                StdSlot::Pipe { child, .. } => (child.0, FOPEN | FPIPE),
                StdSlot::Child(child) => (child.0, FOPEN | FDEV),
                StdSlot::None => (INVALID_HANDLE_VALUE, 0),
            }
        } else {
            match extra_child.iter().find(|(f, _)| *f as usize == fd) {
                Some((_, h)) => (h.0, FOPEN | FPIPE),
                None => (INVALID_HANDLE_VALUE, 0),
            }
        };
        crt[4 + fd] = flags;
        let at = 4 + count + fd * hsize;
        crt[at..at + hsize].copy_from_slice(&(handle as usize).to_le_bytes()[..hsize]);
    }

    // Inherit exactly the child's handles.
    let mut inherit: Vec<HANDLE> = Vec::new();
    for h in slots
        .iter()
        .map(child_handle)
        .chain(extra_child.iter().map(|(_, h)| h.0))
    {
        if !h.is_null() && h != INVALID_HANDLE_VALUE && !inherit.contains(&h) {
            inherit.push(h);
        }
    }
    let mut attr_size = 0usize;
    // SAFETY: the documented two-call sizing protocol; the list buffer outlives CreateProcessW.
    unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut attr_size) };
    let mut attr_buf = vec![0usize; attr_size.div_ceil(std::mem::size_of::<usize>())];
    let attr_list = attr_buf.as_mut_ptr() as *mut c_void;
    // SAFETY: as above; `inherit` stays alive until the list is deleted.
    unsafe {
        if InitializeProcThreadAttributeList(attr_list, 1, 0, &mut attr_size) == 0 {
            return Err(last_error());
        }
        if UpdateProcThreadAttribute(
            attr_list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
            inherit.as_mut_ptr() as *mut c_void,
            inherit.len() * hsize,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ) == 0
        {
            let e = last_error();
            DeleteProcThreadAttributeList(attr_list);
            return Err(e);
        }
    }

    // SAFETY: zeroed POD Win32 structs, then filled in.
    let mut si: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as DWORD;
    si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    si.StartupInfo.hStdInput = child_handle(&slots[0]);
    si.StartupInfo.hStdOutput = child_handle(&slots[1]);
    si.StartupInfo.hStdError = child_handle(&slots[2]);
    si.StartupInfo.cbReserved2 = crt.len() as u16;
    si.StartupInfo.lpReserved2 = crt.as_mut_ptr();
    si.lpAttributeList = attr_list;
    let mut pi: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };

    // SAFETY: every pointer refers to a live, NUL-terminated buffer owned by this frame.
    let ok = unsafe {
        let r = CreateProcessW(
            wprog.as_ptr(),
            wcmd.as_mut_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            1,
            CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            wenv.as_mut().map_or(std::ptr::null_mut(), |e| e.as_mut_ptr() as *mut c_void),
            wcwd.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            &mut si.StartupInfo,
            &mut pi,
        );
        DeleteProcThreadAttributeList(attr_list);
        r
    };
    if ok == 0 {
        return Err(last_error());
    }
    // SAFETY: the thread handle is ours and unused.
    unsafe { CloseHandle(pi.hThread) };
    // The child's ends drop here (closed in the parent), so EOF propagates when the child exits.
    drop(extra_child);

    let mut parents = slots.into_iter().map(|s| match s {
        StdSlot::Pipe { parent, .. } => Some(parent),
        _ => None,
    });
    let stdin = parents.next().flatten().map(|f| Box::new(f) as Box<dyn Write + Send>);
    let stdout = parents.next().flatten().map(|f| Box::new(f) as Box<dyn Read + Send>);
    let stderr = parents.next().flatten().map(|f| Box::new(f) as Box<dyn Read + Send>);
    Ok(Spawned {
        child: RawChild {
            process: Owned(pi.hProcess),
            pid: pi.dwProcessId,
        },
        stdin,
        stdout,
        stderr,
        extra: extra_parent
            .into_iter()
            .map(|(fd, r, w)| (fd, Box::new(r) as Box<dyn Read + Send>, Box::new(w) as Box<dyn Write + Send>))
            .collect(),
    })
}
