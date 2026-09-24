//! Windows named pipes for `net` (`\\.\pipe\name` paths): the server side (`listen(path)`) and the
//! client side (`connect(path)`), the transport libuv's `uv_pipe_t` uses on Windows.
//!
//! Every handle is opened for overlapped I/O. A synchronous pipe handle serializes its operations,
//! so a read blocked on one thread would hold up writes from another; with overlapped I/O each
//! blocking call (read, write, accept) waits on its own event instead, and `CancelIoEx` can abort
//! a blocked read or accept when the socket or server closes.

use std::ffi::c_void;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

type Handle = isize;
type Bool = i32;

const INVALID_HANDLE_VALUE: Handle = -1;
const PIPE_ACCESS_DUPLEX: u32 = 0x3;
const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;
const FILE_FLAG_FIRST_PIPE_INSTANCE: u32 = 0x0008_0000;
const PIPE_TYPE_BYTE: u32 = 0x0;
const PIPE_READMODE_BYTE: u32 = 0x0;
const PIPE_WAIT: u32 = 0x0;
const PIPE_REJECT_REMOTE_CLIENTS: u32 = 0x8;
const PIPE_UNLIMITED_INSTANCES: u32 = 255;
const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const BUFFER_SIZE: u32 = 65536;
const FILE_TYPE_PIPE: u32 = 3;

const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_BROKEN_PIPE: i32 = 109;
const ERROR_PIPE_BUSY: i32 = 231;
const ERROR_NO_DATA: i32 = 232;
const ERROR_PIPE_NOT_CONNECTED: i32 = 233;
const ERROR_PIPE_CONNECTED: i32 = 535;
const ERROR_OPERATION_ABORTED: i32 = 995;
const ERROR_IO_PENDING: i32 = 997;
const ERROR_SEM_TIMEOUT: i32 = 121;

#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: Handle,
}

// win_spawn.rs declares some of these with its own handle/pointer types.
#[allow(clashing_extern_declarations)]
#[link(name = "kernel32")]
extern "system" {
    fn CreateNamedPipeW(
        name: *const u16,
        open_mode: u32,
        pipe_mode: u32,
        max_instances: u32,
        out_buffer: u32,
        in_buffer: u32,
        default_timeout: u32,
        security: *mut c_void,
    ) -> Handle;
    fn ConnectNamedPipe(pipe: Handle, overlapped: *mut Overlapped) -> Bool;
    fn CreateFileW(
        name: *const u16,
        access: u32,
        share: u32,
        security: *mut c_void,
        disposition: u32,
        flags: u32,
        template: Handle,
    ) -> Handle;
    fn WaitNamedPipeW(name: *const u16, timeout: u32) -> Bool;
    fn ReadFile(
        file: Handle,
        buffer: *mut c_void,
        len: u32,
        read: *mut u32,
        overlapped: *mut Overlapped,
    ) -> Bool;
    fn WriteFile(
        file: Handle,
        buffer: *const c_void,
        len: u32,
        written: *mut u32,
        overlapped: *mut Overlapped,
    ) -> Bool;
    fn GetOverlappedResult(
        file: Handle,
        overlapped: *mut Overlapped,
        transferred: *mut u32,
        wait: Bool,
    ) -> Bool;
    fn CreateEventW(security: *mut c_void, manual_reset: Bool, initial: Bool, name: *const u16) -> Handle;
    fn CancelIoEx(file: Handle, overlapped: *mut Overlapped) -> Bool;
    fn CloseHandle(handle: Handle) -> Bool;
    fn GetFileType(file: Handle) -> u32;
}

fn wide(path: &str) -> Vec<u16> {
    path.encode_utf16().chain(std::iter::once(0)).collect()
}

/// One overlapped operation on `handle`: start it with `start(overlapped)`, then wait for it.
/// Returns the byte count, or the Win32 error.
fn overlapped_call(
    handle: Handle,
    start: impl FnOnce(*mut Overlapped) -> Bool,
) -> Result<u32, i32> {
    // SAFETY: a fresh manual-reset event; closed below on every path.
    let event = unsafe { CreateEventW(std::ptr::null_mut(), 1, 0, std::ptr::null()) };
    if event == 0 {
        return Err(io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    let mut ov = Overlapped {
        internal: 0,
        internal_high: 0,
        offset: 0,
        offset_high: 0,
        event,
    };
    let ok = start(&mut ov);
    let result = if ok != 0 {
        let mut n = 0u32;
        // SAFETY: the operation completed; this only reads its byte count.
        unsafe { GetOverlappedResult(handle, &mut ov, &mut n, 0) };
        Ok(n)
    } else {
        let err = io::Error::last_os_error().raw_os_error().unwrap_or(0);
        if err == ERROR_IO_PENDING {
            let mut n = 0u32;
            // SAFETY: `ov` lives until the wait returns, which is when the kernel is done with it.
            if unsafe { GetOverlappedResult(handle, &mut ov, &mut n, 1) } != 0 {
                Ok(n)
            } else {
                Err(io::Error::last_os_error().raw_os_error().unwrap_or(0))
            }
        } else {
            Err(err)
        }
    };
    // SAFETY: `event` is the handle created above.
    unsafe { CloseHandle(event) };
    result
}

/// A connected pipe end (either side). Clones share the handle.
#[derive(Clone)]
pub struct PipeStream {
    inner: Arc<PipeInner>,
}

/// The handle, the number of calls using it, and whether a close was requested. The handle is
/// closed by whoever leaves it unused after a close request, so no call sees it recycled.
struct PipeInner {
    state: Mutex<PipeState>,
}

struct PipeState {
    handle: Handle,
    in_use: usize,
    closed: bool,
}

// SAFETY: the handle is used only through overlapped calls, each with its own OVERLAPPED/event,
// which Windows allows from any thread concurrently; the state is behind a mutex.
unsafe impl Send for PipeInner {}
unsafe impl Sync for PipeInner {}

/// How long a pipe whose writable side ended stays open for the peer's last bytes before it
/// closes, which is how the peer sees end of stream (libuv's pipe `eof_timer`, 50 ms).
const EOF_LINGER: std::time::Duration = std::time::Duration::from_millis(50);

fn close_state(st: &mut PipeState) {
    if st.handle != INVALID_HANDLE_VALUE {
        // SAFETY: no call is using the handle (in_use == 0), and it is marked invalid below.
        unsafe { CloseHandle(st.handle) };
        st.handle = INVALID_HANDLE_VALUE;
    }
}

impl Drop for PipeInner {
    fn drop(&mut self) {
        let st = self.state.get_mut().unwrap_or_else(|p| p.into_inner());
        close_state(st);
    }
}

impl PipeStream {
    fn from_handle(handle: Handle) -> PipeStream {
        PipeStream {
            inner: Arc::new(PipeInner {
                state: Mutex::new(PipeState {
                    handle,
                    in_use: 0,
                    closed: false,
                }),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PipeState> {
        self.inner.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Borrow the handle for one call, or `None` once closed.
    fn enter(&self) -> Option<Handle> {
        let mut st = self.lock();
        if st.closed || st.handle == INVALID_HANDLE_VALUE {
            return None;
        }
        st.in_use += 1;
        Some(st.handle)
    }

    fn leave(&self) {
        let mut st = self.lock();
        st.in_use -= 1;
        if st.closed && st.in_use == 0 {
            close_state(&mut st);
        }
    }

    /// End the writable side. Pipes have no half-close, so (like libuv's eof timer) the pipe
    /// closes after a short linger for the peer's last bytes.
    pub fn shutdown_write(&self) {
        let me = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(EOF_LINGER);
            me.close();
        });
    }

    /// Connect to the server at `path`, waiting while every instance is busy.
    pub fn connect(path: &str) -> io::Result<PipeStream> {
        let name = wide(path);
        loop {
            // SAFETY: `name` is a NUL-terminated UTF-16 path.
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    0,
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                // A path to an ordinary file opens too; only a pipe is something to talk to.
                // SAFETY: `handle` was just opened.
                if unsafe { GetFileType(handle) } != FILE_TYPE_PIPE {
                    // SAFETY: as above; not used afterwards.
                    unsafe { CloseHandle(handle) };
                    return Err(io::Error::from(io::ErrorKind::ConnectionRefused));
                }
                return Ok(PipeStream::from_handle(handle));
            }
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(ERROR_PIPE_BUSY) => {
                    // SAFETY: as above.
                    if unsafe { WaitNamedPipeW(name.as_ptr(), 30_000) } == 0 {
                        let e = io::Error::last_os_error();
                        if e.raw_os_error() == Some(ERROR_SEM_TIMEOUT) {
                            return Err(io::Error::new(io::ErrorKind::TimedOut, e));
                        }
                        if e.raw_os_error() != Some(ERROR_FILE_NOT_FOUND) {
                            return Err(e);
                        }
                    }
                }
                _ => return Err(err),
            }
        }
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(handle) = self.enter() else {
            return Ok(0);
        };
        let len = buf.len().min(u32::MAX as usize) as u32;
        let ptr = buf.as_mut_ptr().cast::<c_void>();
        // SAFETY: `buf` outlives the call, which waits for the read to finish.
        let result = overlapped_call(handle, |ov| unsafe {
            ReadFile(handle, ptr, len, std::ptr::null_mut(), ov)
        });
        self.leave();
        match result {
            Ok(n) => Ok(n as usize),
            // The other end closed, or this end's close cancelled the read: end of stream.
            Err(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED | ERROR_OPERATION_ABORTED) => Ok(0),
            Err(code) => Err(io::Error::from_raw_os_error(code)),
        }
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let Some(handle) = self.enter() else {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        };
        let len = buf.len().min(u32::MAX as usize) as u32;
        let ptr = buf.as_ptr().cast::<c_void>();
        // SAFETY: `buf` outlives the call, which waits for the write to finish.
        let result = overlapped_call(handle, |ov| unsafe {
            WriteFile(handle, ptr, len, std::ptr::null_mut(), ov)
        });
        self.leave();
        match result {
            Ok(n) => Ok(n as usize),
            Err(ERROR_NO_DATA | ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED) => {
                Err(io::Error::from(io::ErrorKind::BrokenPipe))
            }
            Err(code) => Err(io::Error::from_raw_os_error(code)),
        }
    }

    /// Close this end: in-flight calls are cancelled (a read reports end of stream) and the
    /// handle closes once they have returned, so the peer sees end of stream.
    pub fn close(&self) {
        let mut st = self.lock();
        if st.closed {
            return;
        }
        st.closed = true;
        if st.in_use == 0 {
            close_state(&mut st);
        } else {
            // SAFETY: cancels the in-flight calls on the still-open handle; the last of them to
            // return closes it.
            unsafe { CancelIoEx(st.handle, std::ptr::null_mut()) };
        }
    }
}

/// A listening pipe server: the instance waiting for the next client.
pub struct PipeListener {
    name: Vec<u16>,
    next: Mutex<Handle>,
    closed: AtomicBool,
}

// SAFETY: `next` is only swapped under its mutex; the handle is used with overlapped calls.
unsafe impl Send for PipeListener {}
unsafe impl Sync for PipeListener {}

fn create_instance(name: &[u16], first: bool) -> io::Result<Handle> {
    let mut mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    // SAFETY: `name` is a NUL-terminated UTF-16 path; default security.
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            mode,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            BUFFER_SIZE,
            BUFFER_SIZE,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let err = io::Error::last_os_error();
        // Another server owns the name (FILE_FLAG_FIRST_PIPE_INSTANCE): Node reports EADDRINUSE.
        if first && err.raw_os_error() == Some(5) {
            return Err(io::Error::new(io::ErrorKind::AddrInUse, err));
        }
        return Err(err);
    }
    Ok(handle)
}

impl PipeListener {
    pub fn bind(path: &str) -> io::Result<PipeListener> {
        let name = wide(path);
        let first = create_instance(&name, true)?;
        Ok(PipeListener {
            name,
            next: Mutex::new(first),
            closed: AtomicBool::new(false),
        })
    }

    /// Wait for a client on the pending instance, then put a fresh instance in its place.
    pub fn accept(&self) -> io::Result<PipeStream> {
        let handle = *self.next.lock().unwrap_or_else(|p| p.into_inner());
        if self.closed.load(Ordering::SeqCst) || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        }
        // SAFETY: `handle` is the listener's pending instance.
        let connected = match overlapped_call(handle, |ov| unsafe { ConnectNamedPipe(handle, ov) }) {
            Ok(_) | Err(ERROR_PIPE_CONNECTED) => true,
            // A client that connected and left before the wait: the instance is unusable.
            Err(ERROR_NO_DATA) => false,
            Err(code) => return Err(io::Error::from_raw_os_error(code)),
        };
        if self.closed.load(Ordering::SeqCst) {
            return Err(io::Error::from(io::ErrorKind::NotConnected));
        }
        let replacement = create_instance(&self.name, false)?;
        *self.next.lock().unwrap_or_else(|p| p.into_inner()) = replacement;
        let stream = PipeStream::from_handle(handle);
        if connected {
            Ok(stream)
        } else {
            drop(stream);
            self.accept()
        }
    }

    /// Stop listening: a blocked `accept` returns an error. The pending instance (and with it the
    /// pipe name) is released when the last owner drops the listener.
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        let next = *self.next.lock().unwrap_or_else(|p| p.into_inner());
        if next != INVALID_HANDLE_VALUE {
            // SAFETY: cancels the pending instance's ConnectNamedPipe, if one is waiting.
            unsafe { CancelIoEx(next, std::ptr::null_mut()) };
        }
    }
}

impl Drop for PipeListener {
    fn drop(&mut self) {
        let next = *self.next.get_mut().unwrap_or_else(|p| p.into_inner());
        if next != INVALID_HANDLE_VALUE {
            // SAFETY: the listener owns its pending instance.
            unsafe { CloseHandle(next) };
        }
    }
}
