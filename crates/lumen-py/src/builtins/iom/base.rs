//! The abstract bases `_IOBase`, `_RawIOBase`, `_BufferedIOBase` and `_TextIOBase`. They hold no
//! state of their own: every method works through the object's (possibly overridden) methods,
//! so Python subclasses and the native classes share them.

use super::{call, check_closed, closed_error, getattr_opt, unsupported, DEFAULT_BUFFER_SIZE};
use crate::bind::{KwArgs, This};
use crate::object::*;
use crate::vm::Interp;

const CLOSED_ATTR: &str = "__IOBase_closed";

fn iobase_closed(it: &mut Interp, v: &Value) -> R<bool> {
    match getattr_opt(it, v, CLOSED_ATTR)? {
        Some(x) => it.truthy(&x),
        None => Ok(false),
    }
}

fn check_flag(
    it: &mut Interp,
    v: &Value,
    method: &str,
    msg: Option<&Value>,
    default: &str,
) -> R<Value> {
    let r = call(it, v, method, Vec::new())?;
    if !it.truthy(&r)? {
        let m = match msg {
            Some(Value::None) | None => default.to_string(),
            Some(m) => m.as_str().unwrap_or(default).to_string(),
        };
        return Err(unsupported(it, &m));
    }
    Ok(Value::Bool(true))
}

/// The abstract base class for all I/O classes.
///
/// This class provides dummy implementations for many methods that
/// derived classes can override selectively; the default implementations
/// represent a file that cannot be read, written or seeked.
///
/// Even though IOBase does not declare read, readinto, or write because
/// their signatures will vary, implementations and clients should
/// consider those methods part of the interface. Also, implementations
/// may raise UnsupportedOperation when operations they do not support are
/// called.
///
/// The basic type used for binary data read from or written to a file is
/// bytes. Other bytes-like objects are accepted as method arguments too.
/// In some cases (such as readinto), a writable object is required. Text
/// I/O classes work with str data.
///
/// Note that calling any method (except additional calls to close(),
/// which are ignored) on a closed stream should raise a ValueError.
///
/// IOBase (and its subclasses) support the iterator protocol, meaning
/// that an IOBase object can be iterated over yielding the lines in a
/// stream.
///
/// IOBase also supports the :keyword:`with` statement. In this example,
/// fp is closed after the suite of the with statement is complete:
///
/// with open('spam.txt', 'r') as fp:
///     fp.write('Spam and eggs!')
#[lumen_bind::class(module = "_io", name = "_IOBase")]
pub struct IOBase;

#[lumen_bind::methods]
impl IOBase {
    #[constructor]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> IOBase {
        let _ = (args, kwargs);
        IOBase
    }

    fn __del__(slf: This<Value>, it: &mut Interp) {
        let closed = match it.get_attr_str(&slf.0, "closed") {
            Ok(v) => it.truthy(&v).unwrap_or(true),
            Err(_) => true,
        };
        if closed {
            return;
        }
        let _ = it.set_attr_str(&slf.0, "_finalizing", Value::Bool(true));
        let _ = call(it, &slf.0, "close", Vec::new());
    }

    /// Change the stream position to the given byte offset.
    ///
    ///   offset
    ///     The stream position, relative to 'whence'.
    ///   whence
    ///     The relative position to seek from.
    ///
    /// The offset is interpreted relative to the position indicated by whence.
    /// Values for whence are:
    ///
    /// * os.SEEK_SET or 0 -- start of stream (the default); offset should be zero or positive
    /// * os.SEEK_CUR or 1 -- current stream position; offset may be negative
    /// * os.SEEK_END or 2 -- end of stream; offset is usually negative
    ///
    /// Return the new absolute position.
    #[method(hint(py(text_signature = "($self, offset, whence=os.SEEK_SET, /)")))]
    fn seek(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = (slf, args);
        Err(unsupported(it, "seek"))
    }

    /// Return current stream position.
    fn tell(slf: This<Value>, it: &mut Interp) -> R<Value> {
        call(it, &slf.0, "seek", vec![Value::Int(0), Value::Int(1)])
    }

    /// Truncate file to size bytes.
    ///
    /// File pointer is left unchanged. Size defaults to the current IO position
    /// as reported by tell(). Return the new size.
    #[method(hint(py(text_signature = "($self, size=None, /)")))]
    fn truncate(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = (slf, args);
        Err(unsupported(it, "truncate"))
    }

    /// Flush write buffers, if applicable.
    ///
    /// This is not implemented for read-only and non-blocking streams.
    fn flush(slf: This<Value>, it: &mut Interp) -> R<()> {
        if iobase_closed(it, &slf.0)? {
            return Err(closed_error(it));
        }
        Ok(())
    }

    /// Flush and close the IO object.
    ///
    /// This method has no effect if the file is already closed.
    fn close(slf: This<Value>, it: &mut Interp) -> R<()> {
        if iobase_closed(it, &slf.0)? {
            return Ok(());
        }
        let r = call(it, &slf.0, "flush", Vec::new());
        it.set_attr_str(&slf.0, CLOSED_ATTR, Value::Bool(true))?;
        r.map(|_| ())
    }

    #[getter]
    fn closed(slf: This<Value>, it: &mut Interp) -> R<bool> {
        iobase_closed(it, &slf.0)
    }

    /// Return whether object supports random access.
    ///
    /// If False, seek(), tell() and truncate() will raise OSError.
    /// This method may need to do a test seek().
    fn seekable(slf: This<Value>) -> bool {
        let _ = slf;
        false
    }

    /// Return whether object was opened for reading.
    ///
    /// If False, read() will raise OSError.
    fn readable(slf: This<Value>) -> bool {
        let _ = slf;
        false
    }

    /// Return whether object was opened for writing.
    ///
    /// If False, write() will raise OSError.
    fn writable(slf: This<Value>) -> bool {
        let _ = slf;
        false
    }

    #[method(name = "_checkClosed", hint(py(text_signature = "($self, /)")))]
    fn check_closed_m(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = args;
        check_closed(it, &slf.0)?;
        Ok(Value::None)
    }

    #[method(name = "_checkSeekable", hint(py(text_signature = "($self, /)")))]
    fn check_seekable(slf: This<Value>, it: &mut Interp, msg: Option<&Value>) -> R<Value> {
        check_flag(
            it,
            &slf.0,
            "seekable",
            msg,
            "File or stream is not seekable.",
        )
    }

    #[method(name = "_checkReadable", hint(py(text_signature = "($self, /)")))]
    fn check_readable(slf: This<Value>, it: &mut Interp, msg: Option<&Value>) -> R<Value> {
        check_flag(
            it,
            &slf.0,
            "readable",
            msg,
            "File or stream is not readable.",
        )
    }

    #[method(name = "_checkWritable", hint(py(text_signature = "($self, /)")))]
    fn check_writable(slf: This<Value>, it: &mut Interp, msg: Option<&Value>) -> R<Value> {
        check_flag(
            it,
            &slf.0,
            "writable",
            msg,
            "File or stream is not writable.",
        )
    }

    /// Return underlying file descriptor if one exists.
    ///
    /// Raise OSError if the IO object does not use a file descriptor.
    fn fileno(slf: This<Value>, it: &mut Interp) -> R<Value> {
        let _ = slf;
        Err(unsupported(it, "fileno"))
    }

    /// Return whether this is an 'interactive' stream.
    ///
    /// Return False if it can't be determined.
    fn isatty(slf: This<Value>, it: &mut Interp) -> R<bool> {
        check_closed(it, &slf.0)?;
        Ok(false)
    }

    #[proto(enter, hint(py(text_signature = "")))]
    fn __enter__(slf: This<Value>, it: &mut Interp) -> R<Value> {
        check_closed(it, &slf.0)?;
        Ok(slf.0)
    }

    #[proto(exit, hint(py(text_signature = "")))]
    fn __exit__(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = args;
        call(it, &slf.0, "close", Vec::new())
    }

    /// Read and return a line from the stream.
    ///
    /// If size is specified, at most size bytes will be read.
    ///
    /// The line terminator is always b'\n' for binary files; for text
    /// files, the newlines argument to open can be used to select the line
    /// terminator(s) recognized.
    #[method(hint(py(text_signature = "($self, size=-1, /)")))]
    fn readline(slf: This<Value>, it: &mut Interp, size: Option<&Value>) -> R<Value> {
        let limit = super::size_arg(it, size)?;
        let v = slf.0;
        let peek = getattr_opt(it, &v, "peek")?;
        let mut res: Vec<u8> = Vec::new();
        while limit < 0 || (res.len() as i64) < limit {
            let mut nreadahead = 1i64;
            if let Some(peek) = &peek {
                let ahead = it.call(peek, vec![Value::Int(1)], Vec::new())?;
                let Some(ahead) = super::bytes_result(it, &ahead, "peek")? else {
                    break;
                };
                if ahead.is_empty() {
                    break;
                }
                let n = ahead
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(ahead.len(), |p| p + 1);
                nreadahead = n as i64;
                if limit >= 0 {
                    nreadahead = nreadahead.min(limit - res.len() as i64);
                }
            }
            let b = call(it, &v, "read", vec![Value::Int(nreadahead)])?;
            let b = match &b {
                Value::None => break,
                Value::Obj(o) if matches!(o.kind, Kind::Bytes(_) | Kind::ByteArray(_)) => {
                    it.bytes_of(&b)?
                }
                _ => {
                    let t = it.type_name_of(&b);
                    return Err(it.new_exc_str(
                        "OSError",
                        &format!("read() should have returned a bytes object, not '{}'", t),
                    ));
                }
            };
            if b.is_empty() {
                break;
            }
            res.extend_from_slice(&b);
            if res.last() == Some(&b'\n') {
                break;
            }
        }
        Ok(Value::bytes(res))
    }

    /// Return a list of lines from the stream.
    ///
    /// hint can be specified to control the number of lines read: no more
    /// lines will be read if the total size (in bytes/characters) of all
    /// lines so far exceeds hint.
    #[method(hint(py(text_signature = "($self, hint=-1, /)")))]
    fn readlines(slf: This<Value>, it: &mut Interp, hint: Option<&Value>) -> R<Value> {
        let hint = super::size_arg(it, hint)?;
        let v = slf.0;
        if hint <= 0 {
            let items = it.iterate_to_vec(&v)?;
            return Ok(Value::list(items));
        }
        let iter = it.get_iter(&v)?;
        let mut out = Vec::new();
        let mut total = 0i64;
        while let Some(line) = it.iter_next(&iter)? {
            let n = it.len_of(&line)?;
            out.push(line);
            total += n as i64;
            if total >= hint {
                break;
            }
        }
        Ok(Value::list(out))
    }

    /// Write a list of lines to stream.
    ///
    /// Line separators are not added, so it is usual for each of the
    /// lines provided to have a line separator at the end.
    fn writelines(slf: This<Value>, it: &mut Interp, lines: &Value) -> R<()> {
        let v = slf.0;
        check_closed(it, &v)?;
        let iter = it.get_iter(lines)?;
        while let Some(line) = it.iter_next(&iter)? {
            call(it, &v, "write", vec![line])?;
        }
        Ok(())
    }

    #[proto(iter)]
    fn __iter__(slf: This<Value>, it: &mut Interp) -> R<Value> {
        check_closed(it, &slf.0)?;
        Ok(slf.0)
    }

    #[proto(next)]
    fn __next__(slf: This<Value>, it: &mut Interp) -> R<Option<Value>> {
        let line = call(it, &slf.0, "readline", Vec::new())?;
        if it.len_of(&line)? == 0 {
            return Ok(None);
        }
        Ok(Some(line))
    }
}

/// Base class for raw binary I/O.
#[lumen_bind::class(module = "_io", name = "_RawIOBase")]
pub struct RawIOBase;

#[lumen_bind::methods]
impl RawIOBase {
    #[constructor]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> RawIOBase {
        let _ = (args, kwargs);
        RawIOBase
    }

    fn read(slf: This<Value>, it: &mut Interp, #[default(-1)] size: isize) -> R<Value> {
        if size < 0 {
            return call(it, &slf.0, "readall", Vec::new());
        }
        let ba = Value::bytearray(vec![0; size as usize]);
        let n = call(it, &slf.0, "readinto", vec![ba.clone()])?;
        if n.is_none() {
            return Ok(Value::None);
        }
        let n = it.index_of(&n)?;
        let mut data = it.bytes_of(&ba)?;
        if n < 0 || n as usize > data.len() {
            return Err(it.value_error(&format!(
                "readinto returned {} outside buffer size {}",
                n, size
            )));
        }
        data.truncate(n as usize);
        Ok(Value::bytes(data))
    }

    fn readinto(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = (slf, args);
        Err(it.new_exc_str("NotImplementedError", ""))
    }

    fn write(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = (slf, args);
        Err(it.new_exc_str("NotImplementedError", ""))
    }

    /// Read until EOF, using multiple read() call.
    fn readall(slf: This<Value>, it: &mut Interp) -> R<Value> {
        let mut out: Vec<u8> = Vec::new();
        loop {
            let data = call(
                it,
                &slf.0,
                "read",
                vec![Value::Int(DEFAULT_BUFFER_SIZE as i64)],
            )?;
            match &data {
                Value::None => {
                    if out.is_empty() {
                        return Ok(Value::None);
                    }
                    break;
                }
                Value::Obj(o) if matches!(o.kind, Kind::Bytes(_)) => {
                    let b = it.bytes_of(&data)?;
                    if b.is_empty() {
                        break;
                    }
                    out.extend_from_slice(&b);
                }
                _ => return Err(it.type_error("read() should return bytes")),
            }
        }
        Ok(Value::bytes(out))
    }
}

/// Base class for buffered IO objects.
///
/// The main difference with RawIOBase is that the read() method
/// supports omitting the size argument, and does not have a default
/// implementation that defers to readinto().
///
/// In addition, read(), readinto() and write() may raise
/// BlockingIOError if the underlying raw stream is in non-blocking
/// mode and not ready; unlike their raw counterparts, they will never
/// return None.
///
/// A typical implementation should not inherit from a RawIOBase
/// implementation, but wrap one.
#[lumen_bind::class(module = "_io", name = "_BufferedIOBase")]
pub struct BufferedIOBase;

fn buffered_readinto(it: &mut Interp, v: &Value, buffer: &mut [u8], method: &str) -> R<usize> {
    let data = call(it, v, method, vec![Value::Int(buffer.len() as i64)])?;
    let data = match super::bytes_result(it, &data, method)? {
        Some(d) => d,
        None => return Err(it.type_error(&format!("{}() should return bytes", method))),
    };
    if data.len() > buffer.len() {
        return Err(it.value_error(&format!(
            "{}() returned too much data: {} bytes requested, {} returned",
            method,
            buffer.len(),
            data.len()
        )));
    }
    buffer[..data.len()].copy_from_slice(&data);
    Ok(data.len())
}

#[lumen_bind::methods]
impl BufferedIOBase {
    #[constructor]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> BufferedIOBase {
        let _ = (args, kwargs);
        BufferedIOBase
    }

    /// Read and return up to n bytes.
    ///
    /// If the size argument is omitted, None, or negative, read and
    /// return all data until EOF.
    ///
    /// If the size argument is positive, and the underlying raw stream is
    /// not 'interactive', multiple raw reads may be issued to satisfy
    /// the byte count (unless EOF is reached first).
    /// However, for interactive raw streams (as well as sockets and pipes),
    /// at most one raw read will be issued, and a short result does not
    /// imply that EOF is imminent.
    ///
    /// Return an empty bytes object on EOF.
    ///
    /// Return None if the underlying raw stream was open in non-blocking
    /// mode and no data is available at the moment.
    #[method(hint(py(text_signature = "($self, size=-1, /)")))]
    fn read(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = (slf, args);
        Err(unsupported(it, "read"))
    }

    /// Read and return up to size bytes, with at most one read() call to the underlying raw stream.
    ///
    /// Return an empty bytes object on EOF.
    /// A short result does not imply that EOF is imminent.
    #[method(hint(py(text_signature = "($self, size=-1, /)")))]
    fn read1(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = (slf, args);
        Err(unsupported(it, "read1"))
    }

    fn readinto(slf: This<Value>, it: &mut Interp, buffer: &mut [u8]) -> R<usize> {
        buffered_readinto(it, &slf.0, buffer, "read")
    }

    fn readinto1(slf: This<Value>, it: &mut Interp, buffer: &mut [u8]) -> R<usize> {
        buffered_readinto(it, &slf.0, buffer, "read1")
    }

    /// Write buffer b to the IO stream.
    ///
    /// Return the number of bytes written, which is always
    /// the length of b in bytes.
    ///
    /// Raise BlockingIOError if the buffer is full and the
    /// underlying raw stream cannot accept more data at the moment.
    #[method(hint(py(text_signature = "($self, b, /)")))]
    fn write(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = (slf, args);
        Err(unsupported(it, "write"))
    }

    /// Disconnect this buffer from its underlying raw stream and return it.
    ///
    /// After the raw stream has been detached, the buffer is in an unusable
    /// state.
    fn detach(slf: This<Value>, it: &mut Interp) -> R<Value> {
        let _ = slf;
        Err(unsupported(it, "detach"))
    }
}

/// Base class for text I/O.
///
/// This class provides a character and line based interface to stream
/// I/O. There is no readinto method because Python's character strings
/// are immutable.
#[lumen_bind::class(module = "_io", name = "_TextIOBase")]
pub struct TextIOBase;

#[lumen_bind::methods]
impl TextIOBase {
    #[constructor]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> TextIOBase {
        let _ = (args, kwargs);
        TextIOBase
    }

    /// Separate the underlying buffer from the TextIOBase and return it.
    ///
    /// After the underlying buffer has been detached, the TextIO is in an unusable state.
    fn detach(slf: This<Value>, it: &mut Interp) -> R<Value> {
        let _ = slf;
        Err(unsupported(it, "detach"))
    }

    /// Read at most size characters from stream.
    ///
    /// Read from underlying buffer until we have size characters or we hit EOF.
    /// If size is negative or omitted, read until EOF.
    #[method(hint(py(text_signature = "($self, size=-1, /)")))]
    fn read(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = (slf, args);
        Err(unsupported(it, "read"))
    }

    /// Read until newline or EOF.
    ///
    /// Return an empty string if EOF is hit immediately.
    /// If size is specified, at most size characters will be read.
    #[method(hint(py(text_signature = "($self, size=-1, /)")))]
    fn readline(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = (slf, args);
        Err(unsupported(it, "readline"))
    }

    /// Write string s to stream.
    ///
    /// Return the number of characters written
    /// (which is always equal to the length of the string).
    #[method(hint(py(text_signature = "($self, s, /)")))]
    fn write(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = (slf, args);
        Err(unsupported(it, "write"))
    }

    /// Encoding of the text stream.
    ///
    /// Subclasses should override.
    #[getter]
    fn encoding(slf: This<Value>) -> Value {
        let _ = slf;
        Value::None
    }

    /// Line endings translated so far.
    ///
    /// Only line endings translated during reading are considered.
    ///
    /// Subclasses should override.
    #[getter]
    fn newlines(slf: This<Value>) -> Value {
        let _ = slf;
        Value::None
    }

    /// The error setting of the decoder or encoder.
    ///
    /// Subclasses should override.
    #[getter]
    fn errors(slf: This<Value>) -> Value {
        let _ = slf;
        Value::None
    }
}
