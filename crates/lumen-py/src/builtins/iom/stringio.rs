//! `StringIO`: a text stream over an in-memory buffer of code points.

use super::textio::NlState;
use crate::bind::{KwArgs, Py, This};
use crate::object::*;
use crate::vm::Interp;
use lumen_common::smuggle::{code_points, push_code_point};

/// Text I/O implementation using an in-memory buffer.
#[lumen_bind::class(module = "_io", name = "StringIO")]
pub struct StringIO {
    buf: Vec<u32>,
    pos: usize,
    ok: bool,
    closed: bool,
    nl: Option<NlState>,
    readnl: Option<String>,
    writenl: Option<String>,
    readuniversal: bool,
    readtranslate: bool,
}

fn text_of(cps: &[u32]) -> String {
    let mut s = String::with_capacity(cps.len());
    for &c in cps {
        push_code_point(&mut s, c);
    }
    s
}

/// CPython's `_PyIO_find_line_ending`: the length of the first line of `s` including its
/// ending, or `None` when it has no complete ending.
pub fn find_line_ending(translated: bool, universal: bool, readnl: &[u32], s: &[u32]) -> Option<usize> {
    if translated {
        return s.iter().position(|&c| c == '\n' as u32).map(|p| p + 1);
    }
    if universal {
        let mut i = 0;
        while i < s.len() {
            match s[i] {
                0x0a => return Some(i + 1),
                0x0d => {
                    if i + 1 < s.len() && s[i + 1] == 0x0a {
                        return Some(i + 2);
                    }
                    return Some(i + 1);
                }
                _ => i += 1,
            }
        }
        return None;
    }
    if readnl.is_empty() {
        return None;
    }
    s.windows(readnl.len()).position(|w| w == readnl).map(|p| p + readnl.len())
}

impl StringIO {
    fn blank() -> StringIO {
        StringIO {
            buf: Vec::new(),
            pos: 0,
            ok: false,
            closed: false,
            nl: None,
            readnl: None,
            writenl: None,
            readuniversal: false,
            readtranslate: false,
        }
    }

    fn write_str(&mut self, s: &str) -> usize {
        let n = code_points(s).count();
        let decoded = match &mut self.nl {
            Some(nl) => nl.feed(s.to_string(), true),
            None => s.to_string(),
        };
        let translated = match &self.writenl {
            Some(w) if w != "\n" => decoded.replace('\n', w),
            _ => decoded,
        };
        let cps: Vec<u32> = code_points(&translated).collect();
        if cps.is_empty() {
            return n;
        }
        if self.pos > self.buf.len() {
            self.buf.resize(self.pos, 0);
        }
        let end = self.pos + cps.len();
        if end > self.buf.len() {
            self.buf.resize(end, 0);
        }
        self.buf[self.pos..end].copy_from_slice(&cps);
        self.pos = end;
        n
    }

    fn read_line(&mut self, limit: i64) -> String {
        if self.pos >= self.buf.len() {
            return String::new();
        }
        let start = self.pos;
        let avail = self.buf.len() - start;
        let limit = if limit < 0 || limit as usize > avail { avail } else { limit as usize };
        let readnl: Vec<u32> = self.readnl.as_deref().map(|s| code_points(s).collect()).unwrap_or_default();
        let line = &self.buf[start..start + limit];
        let n = find_line_ending(self.readtranslate, self.readuniversal, &readnl, line).unwrap_or(limit);
        self.pos += n;
        text_of(&self.buf[start..start + n])
    }
}

/// Runs `f` on the initialized, open state.
fn st<X>(it: &mut Interp, slf: &Py<StringIO>, f: impl FnOnce(&mut StringIO) -> X) -> R<X> {
    let r = slf.with(it, |s| {
        if !s.ok {
            Err("I/O operation on uninitialized object")
        } else if s.closed {
            Err("I/O operation on closed file")
        } else {
            Ok(f(s))
        }
    })?;
    r.map_err(|m| it.value_error(m))
}

#[lumen_bind::methods]
impl StringIO {
    #[constructor]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> StringIO {
        let _ = (args, kwargs);
        StringIO::blank()
    }

    #[proto(init)]
    fn __init__(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw]
        #[default("")]
        initial_value: Value,
        #[kw]
        #[default("\n")]
        newline: Value,
    ) -> R<()> {
        let initial_value = Some(&initial_value);
        let newline = match &newline {
            Value::None => None,
            v => match v.as_str() {
                Some(s) => Some(s.to_string()),
                None => {
                    let t = it.type_name_of(v);
                    return Err(it.type_error(&format!("newline must be str or None, not {}", t)));
                }
            },
        };
        if let Some(n) = &newline {
            if !matches!(n.as_str(), "" | "\n" | "\r" | "\r\n") {
                let r = it.repr_of(&Value::str(n))?;
                return Err(it.value_error(&format!("illegal newline value: {}", r)));
            }
        }
        let value = match initial_value {
            None | Some(Value::None) => String::new(),
            Some(v) => match v.as_str() {
                Some(s) => s.to_string(),
                None => {
                    let t = it.type_name_of(v);
                    return Err(it.type_error(&format!("initial_value must be str or None, not {}", t)));
                }
            },
        };
        let mut s = StringIO::blank();
        s.readuniversal = newline.as_deref().is_none_or(str::is_empty);
        s.readtranslate = newline.is_none();
        if newline.as_deref().is_some_and(|n| n.starts_with('\r')) {
            s.writenl = newline.clone();
        }
        if s.readuniversal {
            s.nl = Some(NlState::new(s.readtranslate));
        }
        s.readnl = newline;
        s.ok = true;
        if !value.is_empty() {
            s.write_str(&value);
        }
        s.pos = 0;
        *slf.0.borrow_mut(it)? = s;
        Ok(())
    }

    /// Retrieve the entire contents of the object.
    fn getvalue(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        st(it, &slf.0, |s| text_of(&s.buf))
    }

    /// Read at most size characters, returned as a string.
    fn read(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<String> {
        let n = super::size_arg(it, size)?;
        st(it, &slf.0, |s| {
            let start = s.pos.min(s.buf.len());
            let avail = s.buf.len() - start;
            let n = if n < 0 || n as usize > avail { avail } else { n as usize };
            s.pos = start + n;
            text_of(&s.buf[start..start + n])
        })
    }

    /// Read until newline or EOF.
    fn readline(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<String> {
        let n = super::size_arg(it, size)?;
        st(it, &slf.0, |s| s.read_line(n))
    }

    /// Write string to file.
    fn write(slf: This<Py<Self>>, it: &mut Interp, s: &Value) -> R<usize> {
        let Some(text) = s.as_str() else {
            let t = it.type_name_of(s);
            return Err(it.type_error(&format!("string argument expected, got '{}'", t)));
        };
        st(it, &slf.0, |st| st.write_str(text))
    }

    /// Change stream position.
    fn seek(slf: This<Py<Self>>, it: &mut Interp, pos: i64, #[default(0)] whence: i32) -> R<usize> {
        let len = st(it, &slf.0, |s| s.buf.len())?;
        if !(0..=2).contains(&whence) {
            return Err(it.value_error(&format!("Invalid whence ({}, should be 0, 1 or 2)", whence)));
        }
        if pos < 0 && whence == 0 {
            return Err(it.value_error(&format!("Negative seek position {}", pos)));
        }
        if whence != 0 && pos != 0 {
            return Err(it.new_exc_str("OSError", "Can't do nonzero cur-relative seeks"));
        }
        st(it, &slf.0, |s| {
            match whence {
                0 => s.pos = pos as usize,
                2 => s.pos = len,
                _ => {}
            }
            s.pos
        })
    }

    /// Tell the current file position.
    fn tell(slf: This<Py<Self>>, it: &mut Interp) -> R<usize> {
        st(it, &slf.0, |s| s.pos)
    }

    /// Truncate size to pos.
    fn truncate(slf: This<Py<Self>>, it: &mut Interp, pos: Option<&Value>) -> R<i64> {
        let cur = st(it, &slf.0, |s| s.pos as i64)?;
        let size = match pos {
            None | Some(Value::None) => cur,
            Some(v) => it.index_of(v)?,
        };
        if size < 0 {
            return Err(it.value_error(&format!("Negative size value {}", size)));
        }
        st(it, &slf.0, |s| s.buf.truncate(size as usize))?;
        Ok(size)
    }

    /// Returns True if the IO object can be read.
    fn readable(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        st(it, &slf.0, |_| true)
    }

    /// Returns True if the IO object can be written.
    fn writable(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        st(it, &slf.0, |_| true)
    }

    /// Returns True if the IO object can be seeked.
    fn seekable(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        st(it, &slf.0, |_| true)
    }

    /// Close the IO object.
    fn close(&mut self) {
        self.closed = true;
        self.buf = Vec::new();
    }

    #[getter]
    fn closed(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        let (ok, closed) = {
            let s = slf.0.borrow(it)?;
            (s.ok, s.closed)
        };
        if !ok {
            return Err(it.value_error("I/O operation on uninitialized object"));
        }
        Ok(closed)
    }

    #[getter]
    fn line_buffering(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        st(it, &slf.0, |_| false)
    }

    #[getter]
    fn newlines(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        st(it, &slf.0, |s| s.nl.as_ref().map_or(Value::None, NlState::newlines))
    }

    #[proto(next)]
    fn __next__(slf: This<Py<Self>>, it: &mut Interp) -> R<Option<Value>> {
        st(it, &slf.0, |_| ())?;
        let line = if super::exact::<StringIO>(it, slf.0.value()).is_some() {
            Value::string(st(it, &slf.0, |s| s.read_line(-1))?)
        } else {
            let l = it.call_method(slf.0.value(), "readline", Vec::new())?;
            if l.as_str().is_none() {
                let t = it.type_name_of(&l);
                return Err(it.new_exc_str("OSError", &format!("readline() should have returned a str object, not '{}'", t)));
            }
            l
        };
        Ok((line.as_str() != Some("")).then_some(line))
    }

    #[method(hint(py(text_signature = "")))]
    fn __getstate__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let (value, readnl, pos) = st(it, &slf.0, |s| (text_of(&s.buf), s.readnl.clone(), s.pos))?;
        let Value::Obj(o) = slf.0.value() else { unreachable!() };
        let d = o.dict.borrow().clone();
        let d = match d {
            Some(d) => it.call_method(&Value::Obj(d), "copy", Vec::new())?,
            None => Value::None,
        };
        let nl = readnl.map_or(Value::None, Value::string);
        Ok(Value::tuple(vec![Value::string(value), nl, Value::Int(pos as i64), d]))
    }

    #[method(hint(py(text_signature = "")))]
    fn __setstate__(slf: This<Py<Self>>, it: &mut Interp, state: &Value) -> R<()> {
        let items = match state.tuple_items() {
            Some(t) if t.len() >= 4 => t.to_vec(),
            _ => {
                let (t, g) = (it.type_name_of(slf.0.value()), it.type_name_of(state));
                return Err(it.type_error(&format!("{}.__setstate__ argument should be 4-tuple, got {}", t, g)));
            }
        };
        let init = it.get_attr_str(slf.0.value(), "__init__")?;
        it.call(&init, vec![Value::None, items[1].clone()], Vec::new())?;
        let value = items[0].as_str().unwrap_or("").to_string();
        let pos = it.index_of(&items[2])?;
        if pos < 0 {
            return Err(it.value_error("position value cannot be negative"));
        }
        st(it, &slf.0, |s| {
            s.buf = code_points(&value).collect();
            s.pos = pos as usize;
        })?;
        if let Value::Obj(d) = &items[3] {
            let Value::Obj(o) = slf.0.value() else { unreachable!() };
            let dd = it.instance_dict(o);
            it.call_method(&Value::Obj(dd), "update", vec![Value::Obj(d.clone())])?;
        }
        Ok(())
    }
}
