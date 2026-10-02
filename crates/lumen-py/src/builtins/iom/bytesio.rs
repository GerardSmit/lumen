//! `BytesIO`: a buffered binary stream over an in-memory byte buffer.

use super::{closed_error, size_arg};
use crate::bind::{KwArgs, Py, This};
use crate::object::*;
use crate::vm::Interp;

/// Buffered I/O implementation using an in-memory bytes buffer.
#[lumen_bind::class(module = "_io", name = "BytesIO")]
pub struct BytesIO {
    buf: Vec<u8>,
    pos: usize,
    closed: bool,
}

impl BytesIO {
    fn take(&mut self, size: i64) -> Vec<u8> {
        let start = self.pos.min(self.buf.len());
        let avail = self.buf.len() - start;
        let n = if size < 0 { avail } else { (size as usize).min(avail) };
        self.pos = start + n;
        self.buf[start..start + n].to_vec()
    }

    fn line(&mut self, size: i64) -> Vec<u8> {
        let start = self.pos.min(self.buf.len());
        let rest = &self.buf[start..];
        let limit = if size < 0 { rest.len() } else { (size as usize).min(rest.len()) };
        let n = rest[..limit].iter().position(|&b| b == b'\n').map_or(limit, |p| p + 1);
        self.pos = start + n;
        self.buf[start..start + n].to_vec()
    }

    fn write_at(&mut self, data: &[u8]) -> usize {
        let end = self.pos + data.len();
        if self.buf.len() < self.pos {
            self.buf.resize(self.pos, 0);
        }
        if end > self.buf.len() {
            self.buf.resize(end, 0);
        }
        self.buf[self.pos..end].copy_from_slice(data);
        self.pos = end;
        data.len()
    }
}

/// Runs `f` on the open state; `ValueError` when closed.
fn st<X>(it: &mut Interp, slf: &Py<BytesIO>, f: impl FnOnce(&mut BytesIO) -> X) -> R<X> {
    let r = slf.with(it, |s| (!s.closed).then(|| f(s)))?;
    r.ok_or_else(|| closed_error(it))
}

#[lumen_bind::methods]
impl BytesIO {
    #[constructor]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> BytesIO {
        let _ = (args, kwargs);
        BytesIO { buf: Vec::new(), pos: 0, closed: false }
    }

    #[proto(init)]
    fn __init__(slf: This<Py<Self>>, it: &mut Interp, #[kw] initial_bytes: Option<&[u8]>) -> R<()> {
        let mut s = slf.0.borrow_mut(it)?;
        s.closed = false;
        s.buf = initial_bytes.map(|b| b.to_vec()).unwrap_or_default();
        s.pos = 0;
        Ok(())
    }

    /// Retrieve the entire contents of the BytesIO object.
    fn getvalue(slf: This<Py<Self>>, it: &mut Interp) -> R<Vec<u8>> {
        st(it, &slf.0, |s| s.buf.clone())
    }

    /// Read at most size bytes, returned as a bytes object.
    fn read(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Vec<u8>> {
        let n = size_arg(it, size)?;
        st(it, &slf.0, |s| s.take(n))
    }

    /// Read at most size bytes, returned as a bytes object.
    fn read1(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Vec<u8>> {
        let n = size_arg(it, size)?;
        st(it, &slf.0, |s| s.take(n))
    }

    /// Next line from the file, as a bytes object.
    fn readline(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Vec<u8>> {
        let n = size_arg(it, size)?;
        st(it, &slf.0, |s| s.line(n))
    }

    /// List of bytes objects, each a line from the file.
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
    fn readinto(slf: This<Py<Self>>, it: &mut Interp, buffer: &mut [u8]) -> R<usize> {
        let data = st(it, &slf.0, |s| s.take(buffer.len() as i64))?;
        buffer[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }

    /// Write bytes to file.
    fn write(slf: This<Py<Self>>, it: &mut Interp, b: &[u8]) -> R<usize> {
        st(it, &slf.0, |s| if b.is_empty() { 0 } else { s.write_at(b) })
    }

    /// Write lines to the file.
    fn writelines(slf: This<Py<Self>>, it: &mut Interp, lines: &Value) -> R<()> {
        st(it, &slf.0, |_| ())?;
        let iter = it.get_iter(lines)?;
        while let Some(line) = it.iter_next(&iter)? {
            it.call_method(slf.0.value(), "write", vec![line])?;
        }
        Ok(())
    }

    /// Change stream position.
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
    fn truncate(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<i64> {
        let cur = st(it, &slf.0, |s| s.pos as i64)?;
        let size = match size {
            None | Some(Value::None) => cur,
            Some(v) => it.index_of(v)?,
        };
        if size < 0 {
            return Err(it.value_error(&format!("negative size value {}", size)));
        }
        st(it, &slf.0, |s| s.buf.truncate(size as usize))?;
        Ok(size)
    }

    fn readable(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        st(it, &slf.0, |_| ())?;
        Ok(true)
    }

    fn writable(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        st(it, &slf.0, |_| ())?;
        Ok(true)
    }

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
    fn isatty(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        st(it, &slf.0, |_| ())?;
        Ok(false)
    }

    /// Disable all I/O operations.
    fn close(&mut self) {
        self.closed = true;
        self.buf = Vec::new();
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

    fn __getstate__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let (buf, pos) = st(it, &slf.0, |s| (s.buf.clone(), s.pos))?;
        let Value::Obj(o) = slf.0.value() else { unreachable!() };
        let d = o.dict.borrow().clone().map_or(Value::None, |d| it.call_method(&Value::Obj(d), "copy", Vec::new()).unwrap_or(Value::None));
        Ok(Value::tuple(vec![Value::bytes(buf), Value::Int(pos as i64), d]))
    }

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
        st(it, &slf.0, |s| {
            s.buf = buf;
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
