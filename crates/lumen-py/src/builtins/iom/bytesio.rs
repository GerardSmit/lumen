//! `BytesIO`: a buffered binary stream over an in-memory byte buffer. The bytes live in a
//! [`ByteStore`], so `getbuffer()` views share them and pin their size.

use super::{closed_error, size_arg};
use crate::bind::{KwArgs, Py, This};
use crate::object::*;
use crate::vm::Interp;
use lumen_common::buffer::ByteStore;
use std::rc::Rc;

/// Buffered I/O implementation using an in-memory bytes buffer.
#[lumen_bind::class(module = "_io", name = "BytesIO")]
pub struct BytesIO {
    buf: Rc<ByteStore>,
    pos: usize,
    closed: bool,
}

impl BytesIO {
    fn take(&mut self, size: i64) -> Vec<u8> {
        let buf = self.buf.bytes();
        let start = self.pos.min(buf.len());
        let avail = buf.len() - start;
        let n = if size < 0 { avail } else { (size as usize).min(avail) };
        self.pos = start + n;
        buf[start..start + n].to_vec()
    }

    fn line(&mut self, size: i64) -> Vec<u8> {
        let buf = self.buf.bytes();
        let start = self.pos.min(buf.len());
        let rest = &buf[start..];
        let limit = if size < 0 { rest.len() } else { (size as usize).min(rest.len()) };
        let n = rest[..limit].iter().position(|&b| b == b'\n').map_or(limit, |p| p + 1);
        self.pos = start + n;
        buf[start..start + n].to_vec()
    }

    fn write_at(&mut self, data: &[u8]) -> usize {
        let pos = self.pos;
        let _ = self.buf.edit(|v| {
            let end = pos + data.len();
            if end > v.len() {
                v.resize(end, 0);
            }
            v[pos..end].copy_from_slice(data);
        });
        self.pos = pos + data.len();
        data.len()
    }

    fn reset(&mut self, bytes: Vec<u8>) {
        self.buf = Rc::new(ByteStore::new(bytes).growable());
    }
}

fn new_store() -> Rc<ByteStore> {
    Rc::new(ByteStore::new(Vec::new()).growable())
}

/// `check_exports`: a `BufferError` while a `getbuffer()` view is alive.
fn check_exports(it: &mut Interp, slf: &Py<BytesIO>) -> R<()> {
    let pinned = slf.with(it, |s| s.buf.is_pinned())?;
    if pinned {
        return Err(it.new_exc_str("BufferError", "Existing exports of data: object cannot be re-sized"));
    }
    Ok(())
}

/// Runs `f` on the open state; `ValueError` when closed.
fn st<X>(it: &mut Interp, slf: &Py<BytesIO>, f: impl FnOnce(&mut BytesIO) -> X) -> R<X> {
    let r = slf.with(it, |s| (!s.closed).then(|| f(s)))?;
    r.ok_or_else(|| closed_error(it))
}

#[lumen_bind::methods]
impl BytesIO {
    #[constructor(hint(py(text_signature = "(initial_bytes=b'')")))]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> BytesIO {
        let _ = (args, kwargs);
        BytesIO { buf: new_store(), pos: 0, closed: false }
    }

    #[proto(init)]
    fn __init__(slf: This<Py<Self>>, it: &mut Interp, #[kw] initial_bytes: Option<&[u8]>) -> R<()> {
        check_exports(it, &slf.0)?;
        let mut s = slf.0.borrow_mut(it)?;
        s.closed = false;
        s.reset(initial_bytes.map(|b| b.to_vec()).unwrap_or_default());
        s.pos = 0;
        Ok(())
    }

    /// Retrieve the entire contents of the BytesIO object.
    fn getvalue(slf: This<Py<Self>>, it: &mut Interp) -> R<Vec<u8>> {
        st(it, &slf.0, |s| s.buf.to_vec())
    }

    /// Get a read-write view over the contents of the BytesIO object.
    fn getbuffer(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let store = st(it, &slf.0, |s| s.buf.clone())?;
        let exporter = Py::new(it, BytesIOBuffer { _source: slf.0.value().clone() });
        crate::builtins::memview::view_of_store(it, exporter.into_value(), &store)
    }

    /// Read at most size bytes, returned as a bytes object.
    ///
    /// If the size argument is negative, read until EOF is reached.
    /// Return an empty bytes object at EOF.
    #[method(hint(py(text_signature = "($self, size=-1, /)")))]
    fn read(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Vec<u8>> {
        let n = size_arg(it, size)?;
        st(it, &slf.0, |s| s.take(n))
    }

    /// Read at most size bytes, returned as a bytes object.
    ///
    /// If the size argument is negative or omitted, read until EOF is reached.
    /// Return an empty bytes object at EOF.
    #[method(hint(py(text_signature = "($self, size=-1, /)")))]
    fn read1(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Vec<u8>> {
        let n = size_arg(it, size)?;
        st(it, &slf.0, |s| s.take(n))
    }

    /// Next line from the file, as a bytes object.
    ///
    /// Retain newline.  A non-negative size argument limits the maximum
    /// number of bytes to return (an incomplete line may be returned then).
    /// Return an empty bytes object at EOF.
    #[method(hint(py(text_signature = "($self, size=-1, /)")))]
    fn readline(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Vec<u8>> {
        let n = size_arg(it, size)?;
        st(it, &slf.0, |s| s.line(n))
    }

    /// List of bytes objects, each a line from the file.
    ///
    /// Call readline() repeatedly and return a list of the lines so read.
    /// The optional size argument, if given, is an approximate bound on the
    /// total number of bytes in the lines returned.
    fn readlines(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Value> {
        let hint = size_arg(it, size)?;
        let out = st(it, &slf.0, |s| {
            let mut out = Vec::new();
            let mut total = 0i64;
            loop {
                let l = s.line(-1);
                if l.is_empty() {
                    break;
                }
                total += l.len() as i64;
                out.push(Value::bytes(l));
                if hint > 0 && total >= hint {
                    break;
                }
            }
            out
        })?;
        Ok(Value::list(out))
    }

    /// Read bytes into buffer.
    ///
    /// Returns number of bytes read (0 for EOF), or None if the object
    /// is set not to block and has no data to read.
    fn readinto(slf: This<Py<Self>>, it: &mut Interp, buffer: &mut [u8]) -> R<usize> {
        let data = st(it, &slf.0, |s| s.take(buffer.len() as i64))?;
        buffer[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }

    /// Write bytes to file.
    ///
    /// Return the number of bytes written.
    fn write(slf: This<Py<Self>>, it: &mut Interp, b: &[u8]) -> R<usize> {
        st(it, &slf.0, |_| ())?;
        check_exports(it, &slf.0)?;
        st(it, &slf.0, |s| if b.is_empty() { 0 } else { s.write_at(b) })
    }

    /// Write lines to the file.
    ///
    /// Note that newlines are not added.  lines can be any iterable object
    /// producing bytes-like objects. This is equivalent to calling write() for
    /// each element.
    fn writelines(slf: This<Py<Self>>, it: &mut Interp, lines: &Value) -> R<()> {
        st(it, &slf.0, |_| ())?;
        check_exports(it, &slf.0)?;
        let iter = it.get_iter(lines)?;
        while let Some(line) = it.iter_next(&iter)? {
            it.call_method(slf.0.value(), "write", vec![line])?;
        }
        Ok(())
    }

    /// Change stream position.
    ///
    /// Seek to byte offset pos relative to position indicated by whence:
    ///      0  Start of stream (the default).  pos should be >= 0;
    ///      1  Current position - pos may be negative;
    ///      2  End of stream - pos usually negative.
    /// Returns the new absolute position.
    fn seek(slf: This<Py<Self>>, it: &mut Interp, pos: i64, #[default(0)] whence: i32) -> R<i64> {
        let (cur, len) = st(it, &slf.0, |s| (s.pos as i64, s.buf.len() as i64))?;
        let target = match whence {
            0 if pos < 0 => return Err(it.value_error(&format!("negative seek value {}", pos))),
            0 => Some(pos),
            1 => pos.checked_add(cur),
            2 => pos.checked_add(len),
            _ => return Err(it.value_error(&format!("invalid whence ({}, should be 0, 1 or 2)", whence))),
        };
        let Some(target) = target else { return Err(it.overflow_err("new position too large")) };
        let target = target.max(0);
        st(it, &slf.0, |s| s.pos = target as usize)?;
        Ok(target)
    }

    /// Current file position, an integer.
    fn tell(slf: This<Py<Self>>, it: &mut Interp) -> R<usize> {
        st(it, &slf.0, |s| s.pos)
    }

    /// Truncate the file to at most size bytes.
    ///
    /// Size defaults to the current file position, as returned by tell().
    /// The current file position is unchanged.  Returns the new size.
    fn truncate(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<i64> {
        let cur = st(it, &slf.0, |s| s.pos as i64)?;
        check_exports(it, &slf.0)?;
        let size = match size {
            None | Some(Value::None) => cur,
            Some(v) => it.index_of(v)?,
        };
        if size < 0 {
            return Err(it.value_error(&format!("negative size value {}", size)));
        }
        st(it, &slf.0, |s| {
            let _ = s.buf.edit(|v| v.truncate(size as usize));
        })?;
        Ok(size)
    }

    /// Returns True if the IO object can be read.
    fn readable(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        st(it, &slf.0, |_| ())?;
        Ok(true)
    }

    /// Returns True if the IO object can be written.
    fn writable(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        st(it, &slf.0, |_| ())?;
        Ok(true)
    }

    /// Returns True if the IO object can be seeked.
    fn seekable(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        st(it, &slf.0, |_| ())?;
        Ok(true)
    }

    /// Does nothing.
    fn flush(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
        st(it, &slf.0, |_| ())?;
        Ok(())
    }

    /// Always returns False.
    ///
    /// BytesIO objects are not connected to a TTY-like device.
    fn isatty(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        st(it, &slf.0, |_| ())?;
        Ok(false)
    }

    /// Disable all I/O operations.
    fn close(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
        check_exports(it, &slf.0)?;
        let mut s = slf.0.borrow_mut(it)?;
        s.closed = true;
        s.reset(Vec::new());
        Ok(())
    }

    /// True if the file is closed.
    #[getter]
    fn closed(&self) -> bool {
        self.closed
    }

    #[proto(iter)]
    fn __iter__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        st(it, &slf.0, |_| ())?;
        Ok(slf.0.value().clone())
    }

    #[proto(next)]
    fn __next__(slf: This<Py<Self>>, it: &mut Interp) -> R<Option<Value>> {
        let l = st(it, &slf.0, |s| s.line(-1))?;
        Ok((!l.is_empty()).then(|| Value::bytes(l)))
    }

    #[method(hint(py(text_signature = "")))]
    fn __getstate__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let (buf, pos) = st(it, &slf.0, |s| (s.buf.to_vec(), s.pos))?;
        let Value::Obj(o) = slf.0.value() else { unreachable!() };
        let d = o.dict.borrow().clone().map_or(Value::None, |d| it.call_method(&Value::Obj(d), "copy", Vec::new()).unwrap_or(Value::None));
        Ok(Value::tuple(vec![Value::bytes(buf), Value::Int(pos as i64), d]))
    }

    #[method(hint(py(text_signature = "")))]
    fn __setstate__(slf: This<Py<Self>>, it: &mut Interp, state: &Value) -> R<()> {
        let items = match state.tuple_items() {
            Some(t) if t.len() >= 3 => t.to_vec(),
            _ => {
                let t = it.type_name_of(slf.0.value());
                return Err(it.type_error(&format!("{}.__setstate__ argument should be 3-tuple", t)));
            }
        };
        let buf = it.bytes_of(&items[0])?;
        let pos = it.index_of(&items[1])?;
        if pos < 0 {
            return Err(it.value_error("position value cannot be negative"));
        }
        check_exports(it, &slf.0)?;
        st(it, &slf.0, |s| {
            s.reset(buf);
            s.pos = pos as usize;
        })?;
        if let Value::Obj(d) = &items[2] {
            let Value::Obj(o) = slf.0.value() else { unreachable!() };
            let dd = it.instance_dict(o);
            it.call_method(&Value::Obj(dd), "update", vec![Value::Obj(d.clone())])?;
        }
        Ok(())
    }
}

/// The exporter behind `BytesIO.getbuffer()`: the `obj` of the memoryview it returns.
#[lumen_bind::class(module = "_io", name = "_BytesIOBuffer", hint(py(final)))]
pub struct BytesIOBuffer {
    _source: Value,
}

#[lumen_bind::methods]
impl BytesIOBuffer {
    #[constructor]
    fn new(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<BytesIOBuffer> {
        let _ = (args, kwargs);
        Err(it.type_error("cannot create '_io._BytesIOBuffer' instances"))
    }
}
