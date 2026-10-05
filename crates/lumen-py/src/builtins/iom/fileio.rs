//! `FileIO`: raw unbuffered I/O on a file descriptor, through the platform layer.

use super::{call, dealloc_warn, is_eagain, resource_warning, unsupported};
use crate::bind::{type_object, KwArgs, Py, This};
use crate::object::*;
use crate::platform::{IoError, PlatformRef};
use crate::vm::Interp;
use lumen_os::fs::flags;

/// Bytes read per step when the size of the file is unknown.
const SMALLCHUNK: usize = 8192;

/// Open a file.
///
/// The mode can be 'r' (default), 'w', 'x' or 'a' for reading,
/// writing, exclusive creation or appending.  The file will be created if it
/// doesn't exist when opened for writing or appending; it will be truncated
/// when opened for writing.  A FileExistsError will be raised if it already
/// exists when opened for creating. Opening a file for creating implies
/// writing so this mode behaves in a similar way to 'w'.Add a '+' to the mode
/// to allow simultaneous reading and writing. A custom opener can be used by
/// passing a callable as *opener*. The underlying file descriptor for the file
/// object is then obtained by calling opener with (*name*, *flags*).
/// *opener* must return an open file descriptor (passing os.open as *opener*
/// results in functionality similar to passing None).
#[lumen_bind::class(module = "_io", name = "FileIO")]
pub struct FileIO {
    pub fd: i32,
    created: bool,
    readable: bool,
    writable: bool,
    appending: bool,
    seekable: Option<bool>,
    closefd: bool,
    blksize: i64,
    finalizing: bool,
    platform: Option<PlatformRef>,
}

impl Drop for FileIO {
    fn drop(&mut self) {
        if self.fd >= 0 && self.closefd {
            if let Some(p) = &self.platform {
                if let Ok(mut p) = p.try_borrow_mut() {
                    let _ = p.fd_close(self.fd);
                }
            }
        }
    }
}

impl FileIO {
    fn empty() -> FileIO {
        FileIO {
            fd: -1,
            created: false,
            readable: false,
            writable: false,
            appending: false,
            seekable: None,
            closefd: true,
            blksize: 0,
            finalizing: false,
            platform: None,
        }
    }

    /// Descriptor and platform for writing out buffered data while being dropped.
    pub fn drop_target(&self) -> Option<(i32, PlatformRef)> {
        let p = self.platform.clone()?;
        (self.fd >= 0).then_some((self.fd, p))
    }

    fn mode_string(&self) -> &'static str {
        match (self.created, self.appending, self.readable, self.writable) {
            (true, _, true, _) => "xb+",
            (true, _, false, _) => "xb",
            (_, true, true, _) => "ab+",
            (_, true, false, _) => "ab",
            (_, _, true, true) => "rb+",
            (_, _, true, false) => "rb",
            _ => "wb",
        }
    }
}

fn closed_err(it: &mut Interp) -> Obj {
    it.value_error("I/O operation on closed file")
}

fn mode_err(it: &mut Interp, action: &str) -> Obj {
    unsupported(it, &format!("File not open for {}", action))
}

fn os_err(it: &mut Interp, e: IoError) -> Obj {
    it.os_error_io(&e, None)
}

/// The open descriptor, or `ValueError` when closed.
fn fd_of(it: &mut Interp, slf: &Py<FileIO>) -> R<i32> {
    let fd = slf.borrow(it)?.fd;
    if fd < 0 {
        return Err(closed_err(it));
    }
    Ok(fd)
}

/// A new `FileIO(file, mode, closefd, opener)` of exactly the native class.
pub fn new_fileio(
    it: &mut Interp,
    file: &Value,
    mode: &str,
    closefd: bool,
    opener: Option<&Value>,
) -> R<Value> {
    let py = Py::new(it, FileIO::empty());
    init(it, &py, file, mode, closefd, opener)?;
    Ok(py.into_value())
}

/// `(fd, readable, writable)` of an open `FileIO` of exactly the native class, for the direct
/// paths of the buffered layer.
pub fn native_fd(it: &mut Interp, v: &Value) -> Option<(i32, bool, bool)> {
    let f = super::exact::<FileIO>(it, v)?;
    let s = f.borrow(it).ok()?;
    (s.fd >= 0).then_some((s.fd, s.readable, s.writable))
}

/// One `read(2)`; standard input first flushes pending standard output.
pub fn read_fd(it: &mut Interp, fd: i32, buf: &mut [u8]) -> R<Result<usize, IoError>> {
    read_some(it, fd, buf)
}

/// `raw._blksize`, or 0 when `raw` has none.
pub fn blksize(it: &mut Interp, raw: &Value) -> i64 {
    if let Some(f) = Py::<FileIO>::from_value(it, raw) {
        if let Ok(f) = f.borrow(it) {
            return f.blksize;
        }
    }
    match it.get_attr_str(raw, "_blksize") {
        Ok(v) if v.is_int_like() => it.index_of(&v).unwrap_or(0),
        _ => 0,
    }
}

fn bad_mode(it: &mut Interp) -> Obj {
    it.value_error("Must have exactly one of create/read/write/append mode and at most one plus")
}

fn init(
    it: &mut Interp,
    slf: &Py<FileIO>,
    file: &Value,
    mode: &str,
    closefd: bool,
    opener: Option<&Value>,
) -> R<()> {
    let platform = it.platform.clone();
    {
        let old = std::mem::replace(&mut *slf.borrow_mut(it)?, FileIO::empty());
        drop(old);
    }
    if matches!(file, Value::Float(_))
        || matches!(file, Value::Obj(o) if matches!(o.kind, Kind::Float(_)))
    {
        return Err(it.type_error("integer argument expected, got float"));
    }
    let mut fd = -1;
    if file.is_int_like() {
        let n = it.index_of(file)?;
        if n < 0 {
            return Err(it.value_error("negative file descriptor"));
        }
        fd = i32::try_from(n)
            .map_err(|_| it.overflow_err("signed integer is greater than maximum"))?;
    }
    let mut st = FileIO::empty();
    let (mut rwa, mut plus) = (false, false);
    let mut fl = 0;
    for c in mode.chars() {
        match c {
            'x' | 'r' | 'w' | 'a' => {
                if rwa {
                    return Err(bad_mode(it));
                }
                rwa = true;
                match c {
                    'x' => {
                        st.created = true;
                        st.writable = true;
                        fl |= flags::O_EXCL | flags::O_CREAT;
                    }
                    'r' => st.readable = true,
                    'w' => {
                        st.writable = true;
                        fl |= flags::O_CREAT | flags::O_TRUNC;
                    }
                    _ => {
                        st.writable = true;
                        st.appending = true;
                        fl |= flags::O_APPEND | flags::O_CREAT;
                    }
                }
            }
            'b' => {}
            '+' => {
                if plus {
                    return Err(bad_mode(it));
                }
                st.readable = true;
                st.writable = true;
                plus = true;
            }
            _ => return Err(it.value_error(&format!("invalid mode: {}", mode))),
        }
    }
    if !rwa {
        return Err(bad_mode(it));
    }
    fl |= if st.readable && st.writable {
        flags::O_RDWR
    } else if st.readable {
        0
    } else {
        flags::O_WRONLY
    };
    let own_fd = fd < 0;
    if fd >= 0 {
        st.closefd = closefd;
    } else {
        if !closefd {
            return Err(it.value_error("Cannot use closefd=False with file name"));
        }
        match opener {
            None => {
                let p = crate::bind::fspath(it, file)?;
                let path = match p.as_str() {
                    Some(s) => s.to_string(),
                    None => crate::bind::bytes_path(&it.bytes_of(&p)?),
                };
                if path.contains('\0') {
                    return Err(it.value_error("embedded null byte"));
                }
                let r = platform.borrow_mut().fd_open(&path, fl, 0o666);
                fd = r.map_err(|e| it.os_error_io(&e, Some(file)))?;
            }
            Some(op) => {
                let r = it.call(op, vec![file.clone(), Value::Int(fl as i64)], Vec::new())?;
                if !r.is_int_like() {
                    return Err(it.type_error("expected integer from opener"));
                }
                let n = it.index_of(&r)?;
                if n < 0 {
                    return Err(it.value_error(&format!("opener returned {}", n)));
                }
                fd = n as i32;
            }
        }
    }
    let fail = |it: &mut Interp, e: Obj| -> R<()> {
        if own_fd {
            let _ = it.platform.borrow_mut().fd_close(fd);
        }
        Err(e)
    };
    let stat = platform.borrow_mut().fd_stat(fd);
    match stat {
        Ok(s) => {
            if s.mode & lumen_os::fs::S_IFMT == lumen_os::fs::S_IFDIR {
                let e = IoError::from_code("EISDIR");
                let e = it.os_error_io(&e, Some(file));
                return fail(it, e);
            }
            if s.blksize > 1 {
                st.blksize = s.blksize as i64;
            }
        }
        Err(e) if e.errno == lumen_os::errno::errno_of_code("EBADF").unwrap_or(9) => {
            let e = os_err(it, e);
            return fail(it, e);
        }
        Err(_) => {}
    }
    st.fd = fd;
    st.platform = Some(platform.clone());
    let appending = st.appending;
    *slf.borrow_mut(it)? = st;
    if let Err(e) = it.set_attr_str(slf.value(), "name", file.clone()) {
        return fail(it, e);
    }
    if appending {
        let _ = platform.borrow_mut().fd_seek(fd, 0, 2);
    }
    Ok(())
}

fn read_some(it: &mut Interp, fd: i32, buf: &mut [u8]) -> R<Result<usize, IoError>> {
    if fd == 0 {
        it.flush_out();
    }
    it.wait_fd(fd, lumen_os::poll::POLLIN)?;
    Ok(it.platform.borrow_mut().fd_read(fd, buf, None))
}

fn readall_fd(it: &mut Interp, slf: &Py<FileIO>) -> R<Value> {
    let fd = fd_of(it, slf)?;
    let platform = it.platform.clone();
    let mut size_hint = 0usize;
    let mut p = platform.borrow_mut();
    if let Ok(st) = p.fd_stat(fd) {
        if st.size > 0 && st.mode & lumen_os::fs::S_IFMT == lumen_os::fs::S_IFREG {
            if let Ok(pos) = p.fd_seek(fd, 0, 1) {
                if st.size >= pos {
                    size_hint = (st.size - pos) as usize + 1;
                }
            }
        }
    }
    drop(p);
    let mut out: Vec<u8> = Vec::with_capacity(size_hint);
    loop {
        let want = if out.len() < size_hint {
            size_hint - out.len()
        } else {
            SMALLCHUNK.max(out.len() / 4)
        };
        let start = out.len();
        out.resize(start + want, 0);
        match read_some(it, fd, &mut out[start..])? {
            Ok(0) => {
                out.truncate(start);
                break;
            }
            Ok(n) => out.truncate(start + n),
            Err(e) if is_eagain(&e) => {
                out.truncate(start);
                if out.is_empty() {
                    return Ok(Value::None);
                }
                break;
            }
            Err(e) => return Err(os_err(it, e)),
        }
    }
    Ok(Value::bytes(out))
}

#[lumen_bind::methods]
impl FileIO {
    #[constructor(hint(py(text_signature = "(file, mode='r', closefd=True, opener=None)")))]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> FileIO {
        let _ = (args, kwargs);
        FileIO::empty()
    }

    #[proto(init)]
    fn __init__(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] file: &Value,
        #[kw]
        #[default("r")]
        mode: &str,
        #[kw]
        #[default(true)]
        closefd: bool,
        #[kw] opener: Option<&Value>,
    ) -> R<()> {
        init(it, &slf.0, file, mode, closefd, opener)
    }

    /// Read at most size bytes, returned as bytes.
    ///
    /// If size is less than 0, read all bytes in the file making multiple read calls.
    /// See ``FileIO.readall``.
    ///
    /// Attempts to make only one system call, retrying only per PEP 475 (EINTR). This
    /// means less data may be returned than requested.
    ///
    /// In non-blocking mode, returns None if no data is available. Return an empty
    /// bytes object at EOF.
    #[method(hint(py(text_signature = "($self, size=-1, /)")))]
    fn read(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Value> {
        let fd = fd_of(it, &slf.0)?;
        if !slf.0.borrow(it)?.readable {
            return Err(mode_err(it, "reading"));
        }
        let size = super::size_arg(it, size)?;
        if size < 0 {
            return readall_fd(it, &slf.0);
        }
        let mut buf = vec![0u8; size as usize];
        match read_some(it, fd, &mut buf)? {
            Ok(n) => {
                buf.truncate(n);
                Ok(Value::bytes(buf))
            }
            Err(e) if is_eagain(&e) => Ok(Value::None),
            Err(e) => Err(os_err(it, e)),
        }
    }

    /// Read all data from the file, returned as bytes.
    ///
    /// Reads until either there is an error or read() returns size 0 (indicates EOF).
    /// If the file is already at EOF, returns an empty bytes object.
    ///
    /// In non-blocking mode, returns as much data as could be read before EAGAIN. If no
    /// data is available (EAGAIN is returned before bytes are read) returns None.
    fn readall(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        readall_fd(it, &slf.0)
    }

    /// Same as RawIOBase.readinto().
    fn readinto(slf: This<Py<Self>>, it: &mut Interp, buffer: &mut [u8]) -> R<Option<usize>> {
        let fd = fd_of(it, &slf.0)?;
        if !slf.0.borrow(it)?.readable {
            return Err(mode_err(it, "reading"));
        }
        match read_some(it, fd, buffer)? {
            Ok(n) => Ok(Some(n)),
            Err(e) if is_eagain(&e) => Ok(None),
            Err(e) => Err(os_err(it, e)),
        }
    }

    /// Write buffer b to file, return number of bytes written.
    ///
    /// Only makes one system call, so not all of the data may be written.
    /// The number of bytes actually written is returned.  In non-blocking mode,
    /// returns None if the write would block.
    fn write(slf: This<Py<Self>>, it: &mut Interp, b: &[u8]) -> R<Option<usize>> {
        let fd = fd_of(it, &slf.0)?;
        if !slf.0.borrow(it)?.writable {
            return Err(mode_err(it, "writing"));
        }
        match it.fd_write(fd, b)? {
            Ok(n) => Ok(Some(n)),
            Err(e) if is_eagain(&e) => Ok(None),
            Err(e) => Err(os_err(it, e)),
        }
    }

    /// Move to new file position and return the file position.
    ///
    /// Argument offset is a byte count.  Optional argument whence defaults to
    /// SEEK_SET or 0 (offset from start of file, offset should be >= 0); other values
    /// are SEEK_CUR or 1 (move relative to current position, positive or negative),
    /// and SEEK_END or 2 (move relative to end of file, usually negative, although
    /// many platforms allow seeking beyond the end of a file).
    ///
    /// Note that not all file objects are seekable.
    fn seek(
        slf: This<Py<Self>>,
        it: &mut Interp,
        pos: &Value,
        #[default(0)] whence: i32,
    ) -> R<u64> {
        let fd = fd_of(it, &slf.0)?;
        if matches!(pos, Value::Float(_)) || !it.has_index(pos) {
            return Err(it.type_error("an integer is required"));
        }
        let pos = it.index_of(pos)?;
        let r = it.platform.borrow_mut().fd_seek(fd, pos, whence);
        r.map_err(|e| os_err(it, e))
    }

    /// Current file position.
    ///
    /// Can raise OSError for non seekable files.
    fn tell(slf: This<Py<Self>>, it: &mut Interp) -> R<u64> {
        let fd = fd_of(it, &slf.0)?;
        let r = it.platform.borrow_mut().fd_seek(fd, 0, 1);
        r.map_err(|e| os_err(it, e))
    }

    /// Truncate the file to at most size bytes and return the truncated size.
    ///
    /// Size defaults to the current file position, as returned by tell().
    /// The current file position is changed to the value of size.
    fn truncate(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Value> {
        let fd = fd_of(it, &slf.0)?;
        if !slf.0.borrow(it)?.writable {
            return Err(mode_err(it, "writing"));
        }
        let size = match size {
            Some(v) if !v.is_none() => v.clone(),
            _ => call(it, slf.0.value(), "tell", Vec::new())?,
        };
        let n = it.index_of(&size)?;
        if n < 0 {
            return Err(os_err(it, IoError::from_code("EINVAL")));
        }
        let r = it.platform.borrow_mut().fd_truncate(fd, n as u64);
        r.map_err(|e| os_err(it, e))?;
        Ok(size)
    }

    /// True if file supports random-access.
    fn seekable(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        let fd = fd_of(it, &slf.0)?;
        if let Some(s) = slf.0.borrow(it)?.seekable {
            return Ok(s);
        }
        let ok = it.platform.borrow_mut().fd_seek(fd, 0, 1).is_ok();
        slf.0.borrow_mut(it)?.seekable = Some(ok);
        Ok(ok)
    }

    /// True if file was opened in a read mode.
    fn readable(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        fd_of(it, &slf.0)?;
        Ok(slf.0.borrow(it)?.readable)
    }

    /// True if file was opened in a write mode.
    fn writable(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        fd_of(it, &slf.0)?;
        Ok(slf.0.borrow(it)?.writable)
    }

    /// Return the underlying file descriptor (an integer).
    fn fileno(slf: This<Py<Self>>, it: &mut Interp) -> R<i32> {
        fd_of(it, &slf.0)
    }

    /// True if the file is connected to a TTY device.
    fn isatty(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        let fd = fd_of(it, &slf.0)?;
        Ok(it.platform.borrow_mut().fd_isatty(fd))
    }

    /// Close the file.
    ///
    /// A closed file cannot be used for further I/O operations.  close() may be
    /// called more than once without error.
    fn close(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
        if slf.0.borrow(it)?.finalizing {
            dealloc_warn(it, slf.0.value(), slf.0.value());
        }
        let base = type_object::<super::base::RawIOBase>(it);
        let f = it.get_attr_str(&Value::Obj(base), "close")?;
        let r = it.call(&f, vec![slf.0.value().clone()], Vec::new());
        let (fd, closefd) = {
            let mut s = slf.0.borrow_mut(it)?;
            let v = (s.fd, s.closefd);
            s.fd = -1;
            v
        };
        if closefd && fd >= 0 {
            let c = it.platform.borrow_mut().fd_close(fd);
            if let Err(e) = c {
                r?;
                return Err(os_err(it, e));
            }
        }
        r.map(|_| ())
    }

    /// True if the file is closed
    #[getter]
    fn closed(&self) -> bool {
        self.fd < 0
    }

    /// True if the file descriptor will be closed by close().
    #[getter]
    fn closefd(&self) -> bool {
        self.closefd
    }

    /// String giving the file mode
    #[getter]
    fn mode(&self) -> &'static str {
        self.mode_string()
    }

    /// Stat st_blksize if available
    #[getter]
    fn _blksize(&self) -> i64 {
        self.blksize
    }

    #[getter]
    fn _finalizing(&self) -> bool {
        self.finalizing
    }

    #[setter]
    fn set__finalizing(&mut self, v: bool) {
        self.finalizing = v;
    }

    fn _dealloc_warn(slf: This<Py<Self>>, it: &mut Interp, object: &Value) -> R<()> {
        let (open, closefd) = {
            let s = slf.0.borrow(it)?;
            (s.fd >= 0, s.closefd)
        };
        if open && closefd {
            let r = it.repr_of(object)?;
            if let Err(e) = resource_warning(it, object, &format!("unclosed file {r}")) {
                if it.exc_is(&e, "Warning") {
                    let msg = format!("Exception ignored while finalizing file {r}");
                    it.write_unraisable(&e, Some(&msg), None);
                }
            }
        }
        Ok(())
    }

    fn _isatty_open_only(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        let fd = fd_of(it, &slf.0)?;
        Ok(it.platform.borrow_mut().fd_isatty(fd))
    }

    fn __getstate__(slf: This<Value>, it: &mut Interp) -> R<Value> {
        let t = it.type_name_of(&slf.0);
        Err(it.type_error(&format!("cannot pickle '{}' instances", t)))
    }

    #[proto(repr)]
    fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        let tn = it.tp_name_of(slf.0.value());
        let (fd, mode, closefd) = {
            let s = slf.0.borrow(it)?;
            (s.fd, s.mode_string(), s.closefd)
        };
        if fd < 0 {
            return Ok(format!("<{} [closed]>", tn));
        }
        let cf = if closefd { "True" } else { "False" };
        match super::getattr_opt(it, slf.0.value(), "name")? {
            None => Ok(format!("<{} fd={} mode='{}' closefd={}>", tn, fd, mode, cf)),
            Some(n) => {
                let r = it.repr_of(&n)?;
                Ok(format!(
                    "<{} name={} mode='{}' closefd={}>",
                    tn, r, mode, cf
                ))
            }
        }
    }
}
