//! `fcntl` on `lumen_os::fdctl`: `fcntl`, `ioctl`, `flock` and `lockf`, with CPython's buffer
//! argument rules (`Modules/fcntlmodule.c`).

/// This module performs file control and I/O control on file descriptors.
/// It is an interface to the fcntl() and ioctl() Unix routines.
/// File descriptors can be obtained with the fileno() method of
/// a file or socket object.
#[lumen_bind::module(name = "fcntl")]
pub mod fcntl {
    use crate::builtins::memview::with_writable;
    use crate::builtins::posixm::as_file_descriptor;
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use lumen_os::fdctl::{self, RecordLock};
    use lumen_os::FsError;

    const BUF_SIZE: usize = 1024;
    const LOCK_SH: i32 = 1;
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    const LOCK_UN: i32 = 8;

    const EINTR: i32 = 4;

    /// Runs `f` until it stops failing with `EINTR`, servicing signal handlers in between.
    fn retry<T>(it: &mut Interp, mut f: impl FnMut() -> Result<T, FsError>) -> R<T> {
        loop {
            match f() {
                Ok(v) => return Ok(v),
                Err(e) if e.errno() == EINTR => it.poll()?,
                Err(e) => return Err(it.os_error_errno(e.errno(), None, None)),
            }
        }
    }

    fn once<T>(it: &mut Interp, r: Result<T, FsError>) -> R<T> {
        r.map_err(|e| it.os_error_errno(e.errno(), None, None))
    }

    fn unsigned_arg(it: &mut Interp, v: &Value, msg: &str) -> R<i32> {
        if !v.is_int_like() {
            return Err(it.type_error(msg));
        }
        let n = it.index_of(v)?;
        if n < 0 {
            return Err(it.overflow_err("can't convert negative int to unsigned"));
        }
        u32::try_from(n).map(|n| n as i32).map_err(|_| it.overflow_err("Python int too large to convert to C unsigned int"))
    }

    fn data_arg(it: &mut Interp, v: &Value) -> R<Option<Vec<u8>>> {
        if let Some(s) = v.as_str() {
            return Ok(Some(s.as_bytes().to_vec()));
        }
        if it.is_buffer(v) {
            return Ok(Some(it.bytes_of(v)?));
        }
        Ok(None)
    }

    /// Perform the operation `cmd` on file descriptor fd.
    ///
    /// The values used for `cmd` are operating system dependent, and are available
    /// as constants in the fcntl module, using the same names as used in
    /// the relevant C header files.  The argument arg is optional, and
    /// defaults to 0; it may be an int or a string.  If arg is given as a string,
    /// the return value of fcntl is a string of that length, containing the
    /// resulting value put in the arg buffer by the operating system.  The length
    /// of the arg string is not allowed to exceed 1024 bytes.  If the arg given
    /// is an integer or if none is specified, the result value is an integer
    /// corresponding to the return value of the fcntl call in the C code.
    #[op]
    fn fcntl(it: &mut Interp, fd: &Value, code: i32, arg: Option<&Value>) -> R<Value> {
        let fd = as_file_descriptor(it, fd)?;
        let mut int_arg = 0;
        if let Some(v) = arg {
            if let Some(data) = data_arg(it, v)? {
                if data.len() > BUF_SIZE {
                    return Err(it.value_error("fcntl string arg too long"));
                }
                let n = data.len();
                let mut buf = data;
                buf.push(0);
                retry(it, || fdctl::fcntl_buf(fd, code, &mut buf))?;
                buf.truncate(n);
                return Ok(Value::bytes(buf));
            }
            int_arg = unsigned_arg(
                it,
                v,
                "fcntl requires a file or file descriptor, an integer and optionally a third integer or a string",
            )?;
        }
        let r = retry(it, || fdctl::fcntl_int(fd, code, int_arg))?;
        Ok(Value::Int(r as i64))
    }

    /// Perform the operation `request` on file descriptor `fd`.
    ///
    /// The values used for `request` are operating system dependent, and are available
    /// as constants in the fcntl or termios library, using the same names as used in
    /// the relevant C header files.
    ///
    /// The argument `arg` is optional, and defaults to 0; it may be an int or a
    /// buffer containing character data (most likely a string or an array).
    ///
    /// If the argument is a mutable buffer (such as an array) and if the
    /// mutate_flag argument (which is only allowed in this case) is true then the
    /// buffer is (in effect) passed to the operating system and changes made by
    /// the OS will be reflected in the contents of the buffer after the call has
    /// returned.  The return value is the integer returned by the ioctl system
    /// call.
    ///
    /// If the argument is a mutable buffer and the mutable_flag argument is false,
    /// the behavior is as if a string had been passed.
    ///
    /// If the argument is an immutable buffer (most likely a string) then a copy
    /// of the buffer is passed to the operating system and the return value is a
    /// string of the same length containing whatever the operating system put in
    /// the buffer.  The length of the arg buffer in this case is not allowed to
    /// exceed 1024 bytes.
    ///
    /// If the arg given is an integer or if none is specified, the result value is
    /// an integer corresponding to the return value of the ioctl call in the C code.
    #[op]
    fn ioctl(it: &mut Interp, fd: &Value, request: &Value, arg: Option<&Value>, #[default(true)] mutate_flag: bool) -> R<Value> {
        let fd = as_file_descriptor(it, fd)?;
        let code = it.index_of(request)? as u32;
        let mut int_arg = 0;
        if let Some(v) = arg {
            if mutate_flag {
                let done = with_writable(it, v, |b| {
                    if b.len() <= BUF_SIZE {
                        let mut tmp = b.to_vec();
                        tmp.push(0);
                        let r = fdctl::ioctl_buf(fd, code, &mut tmp);
                        b.copy_from_slice(&tmp[..b.len()]);
                        r
                    } else {
                        fdctl::ioctl_buf(fd, code, b)
                    }
                })?;
                if let Some(r) = done {
                    let r = once(it, r)?;
                    return Ok(Value::Int(r as i64));
                }
            }
            if let Some(data) = data_arg(it, v)? {
                if data.len() > BUF_SIZE {
                    return Err(it.value_error("ioctl string arg too long"));
                }
                let n = data.len();
                let mut buf = data;
                buf.push(0);
                once(it, fdctl::ioctl_buf(fd, code, &mut buf))?;
                buf.truncate(n);
                return Ok(Value::bytes(buf));
            }
            if !v.is_int_like() {
                return Err(it.type_error(
                    "ioctl requires a file or file descriptor, an integer and optionally an integer or buffer argument",
                ));
            }
            int_arg = i32::try_from(it.index_of(v)?).map_err(|_| it.overflow_err("Python int too large to convert to C int"))?;
        }
        let r = once(it, fdctl::ioctl_int(fd, code, int_arg))?;
        Ok(Value::Int(r as i64))
    }

    fn lock_kind(code: i32, what: &str, it: &mut Interp) -> R<i32> {
        if code == LOCK_UN {
            Ok(fdctl::F_UNLCK)
        } else if code & LOCK_SH != 0 {
            Ok(fdctl::F_RDLCK)
        } else if code & LOCK_EX != 0 {
            Ok(fdctl::F_WRLCK)
        } else {
            Err(it.value_error(&format!("unrecognized {what} argument")))
        }
    }

    /// Perform the lock operation `operation` on file descriptor `fd`.
    ///
    /// See the Unix manual page for flock(2) for details (On some systems, this
    /// function is emulated using fcntl()).
    #[op]
    fn flock(it: &mut Interp, fd: &Value, code: i32) -> R<()> {
        let fd = as_file_descriptor(it, fd)?;
        retry(it, || fdctl::flock(fd, code))
    }

    /// A wrapper around the fcntl() locking calls.
    ///
    /// `fd` is the file descriptor of the file to lock or unlock, and operation is one
    /// of the following values:
    ///
    ///     LOCK_UN - unlock
    ///     LOCK_SH - acquire a shared lock
    ///     LOCK_EX - acquire an exclusive lock
    ///
    /// When operation is LOCK_SH or LOCK_EX, it can also be bitwise ORed with
    /// LOCK_NB to avoid blocking on lock acquisition.  If LOCK_NB is used and the
    /// lock cannot be acquired, an OSError will be raised and the exception will
    /// have an errno attribute set to EACCES or EAGAIN (depending on the operating
    /// system -- for portability, check for either value).
    ///
    /// `len` is the number of bytes to lock, with the default meaning to lock to
    /// EOF.  `start` is the byte offset, relative to `whence`, to that the lock
    /// starts.  `whence` is as with fileobj.seek(), specifically:
    ///
    ///     0 - relative to the start of the file (SEEK_SET)
    ///     1 - relative to the current buffer position (SEEK_CUR)
    ///     2 - relative to the end of the file (SEEK_END)
    #[op]
    fn lockf(it: &mut Interp, fd: &Value, code: i32, len: Option<&Value>, start: Option<&Value>, #[default(0)] whence: i32) -> R<()> {
        let fd = as_file_descriptor(it, fd)?;
        let kind = lock_kind(code, "lockf", it)?;
        let start = match start {
            Some(v) => it.index_of(v)?,
            None => 0,
        };
        let len = match len {
            Some(v) => it.index_of(v)?,
            None => 0,
        };
        let lock = RecordLock { kind, whence, start, len, wait: code & LOCK_NB == 0 };
        retry(it, || fdctl::lockf(fd, lock))
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        for (name, v) in fdctl::fcntl_constants() {
            dict_set_str(&d, name, Value::Int(v));
        }
    }
}
