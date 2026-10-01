//! `open()` and file objects, including the standard streams.

use crate::object::*;
use crate::vm::*;
use std::cell::RefCell;
use crate::platform::{IoError, OpenMode};

type Kw<'a> = &'a [(Obj, Value)];

impl Drop for FileData {
    fn drop(&mut self) {
        flush_data(self);
        release_handle(self);
    }
}

fn flush_data(fd: &mut FileData) {
    if let (FileMode::Write { handle, buf, .. }, Some(p)) = (&mut fd.mode, &fd.platform) {
        if !buf.is_empty() {
            if let Ok(mut p) = p.try_borrow_mut() {
                let _ = p.write_all(*handle, buf);
            }
            buf.clear();
        }
    }
}

fn release_handle(fd: &mut FileData) {
    if let (FileMode::Write { handle, .. }, Some(p)) = (&fd.mode, &fd.platform) {
        if let Ok(mut p) = p.try_borrow_mut() {
            p.close(*handle);
        }
    }
}

pub fn new_file(it: &Interp, mode: FileMode, text: bool, name: &str) -> Value {
    let platform = Some(it.platform.clone());
    Value::Obj(Object::new(Kind::File(RefCell::new(FileData { mode, text, name: name.to_string(), platform }))))
}

fn file_of<'a>(it: &mut Interp, a: &'a [Value]) -> R<&'a Obj> {
    match a.first() {
        Some(Value::Obj(o)) if matches!(o.kind, Kind::File(_)) => Ok(o),
        _ => Err(it.type_error("descriptor requires a file object")),
    }
}

fn closed_err(it: &mut Interp) -> Obj {
    it.value_error("I/O operation on closed file.")
}

impl Interp {
    pub fn os_error(&mut self, e: &IoError, filename: &str) -> Obj {
        use crate::platform::IoErrorKind::*;
        let name = match e.kind {
            NotFound => "FileNotFoundError",
            PermissionDenied => "PermissionError",
            AlreadyExists => "FileExistsError",
            IsADirectory => "IsADirectoryError",
            NotADirectory => "NotADirectoryError",
            Other => "OSError",
        };
        let msg = e.message();
        let cls = Value::Obj(self.exc_type(name));
        let mut args = vec![Value::Int(e.errno as i64), Value::str(msg)];
        if !filename.is_empty() {
            args.push(Value::str(filename));
        }
        match self.call(&cls, args, Vec::new()) {
            Ok(Value::Obj(o)) => o,
            Ok(_) => self.new_exc_str("OSError", msg),
            Err(e) => e,
        }
    }

    pub fn file_write(&mut self, o: &Obj, s: &str) -> R<()> {
        let Kind::File(f) = &o.kind else { return Ok(()) };
        let mut fd = f.borrow_mut();
        match &mut fd.mode {
            FileMode::Stdout => {
                drop(fd);
                self.write_stdout(s);
            }
            FileMode::Stderr => {
                drop(fd);
                self.write_stderr(s);
            }
            FileMode::Write { buf, .. } => {
                buf.extend_from_slice(s.as_bytes());
                if buf.len() > 1 << 16 {
                    flush_data(&mut fd);
                }
            }
            FileMode::Closed => {
                drop(fd);
                return Err(closed_err(self));
            }
            _ => {
                drop(fd);
                return Err(self.new_exc_str("UnsupportedOperation", "not writable"));
            }
        }
        Ok(())
    }

    fn file_write_bytes(&mut self, o: &Obj, b: &[u8]) -> R<()> {
        let Kind::File(f) = &o.kind else { return Ok(()) };
        let mut fd = f.borrow_mut();
        match &mut fd.mode {
            FileMode::Write { buf, .. } => buf.extend_from_slice(b),
            FileMode::Closed => {
                drop(fd);
                return Err(closed_err(self));
            }
            _ => {
                drop(fd);
                return Err(self.new_exc_str("UnsupportedOperation", "not writable"));
            }
        }
        Ok(())
    }

    fn file_out(&self, text: bool, data: Vec<u8>) -> Value {
        if text {
            Value::string(String::from_utf8_lossy(&data).into_owned())
        } else {
            Value::bytes(data)
        }
    }

    pub fn file_readline(&mut self, o: &Obj, size: i64) -> R<Value> {
        let Kind::File(f) = &o.kind else { return Ok(Value::str("")) };
        let mut fd = f.borrow_mut();
        let text = fd.text;
        let mut out = Vec::new();
        match &mut fd.mode {
            FileMode::Read { data, pos } => {
                let rest = &data[*pos..];
                let mut n = rest.iter().position(|&b| b == b'\n').map_or(rest.len(), |i| i + 1);
                if size >= 0 {
                    n = n.min(size as usize);
                }
                out.extend_from_slice(&rest[..n]);
                *pos += n;
            }
            FileMode::Stdin => {
                drop(fd);
                self.flush_out();
                let line = self.platform.borrow_mut().read_stdin_line();
                return Ok(self.file_out(text, line));
            }
            FileMode::Closed => {
                drop(fd);
                return Err(closed_err(self));
            }
            _ => {
                drop(fd);
                return Err(self.new_exc_str("UnsupportedOperation", "not readable"));
            }
        }
        drop(fd);
        Ok(self.file_out(text, out))
    }

    fn file_read(&mut self, o: &Obj, size: i64) -> R<Value> {
        let Kind::File(f) = &o.kind else { return Ok(Value::str("")) };
        let mut fd = f.borrow_mut();
        let text = fd.text;
        let out = match &mut fd.mode {
            FileMode::Read { data, pos } => {
                let rest = &data[*pos..];
                let mut n = rest.len();
                if size >= 0 {
                    n = n.min(size as usize);
                }
                if text && size >= 0 {
                    let mut end = 0;
                    let s = String::from_utf8_lossy(rest);
                    for (chars, (i, c)) in s.char_indices().enumerate() {
                        if chars as i64 == size {
                            break;
                        }
                        end = i + c.len_utf8();
                    }
                    n = end.min(rest.len());
                }
                let v = rest[..n].to_vec();
                *pos += n;
                v
            }
            FileMode::Stdin => {
                drop(fd);
                self.flush_out();
                let v = self.platform.borrow_mut().read_stdin_to_end();
                return Ok(self.file_out(text, v));
            }
            FileMode::Closed => {
                drop(fd);
                return Err(closed_err(self));
            }
            _ => {
                drop(fd);
                return Err(self.new_exc_str("UnsupportedOperation", "not readable"));
            }
        };
        drop(fd);
        Ok(self.file_out(text, out))
    }

    fn file_close(&mut self, o: &Obj) {
        if let Kind::File(f) = &o.kind {
            let mut fd = f.borrow_mut();
            if matches!(fd.mode, FileMode::Stdout | FileMode::Stderr | FileMode::Stdin) {
                return;
            }
            flush_data(&mut fd);
            release_handle(&mut fd);
            fd.mode = FileMode::Closed;
        }
    }
}

fn open(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("open", a, kw, &["file", "mode", "buffering", "encoding", "errors", "newline", "closefd", "opener"], 1)?;
    let path = it.str_arg(b[0].as_ref().unwrap_or(&Value::None), "file")?;
    let mode = match &b[1] {
        Some(m) => it.str_arg(m, "open() argument 'mode'")?,
        None => "r".to_string(),
    };
    let binary = mode.contains('b');
    let kind = mode.chars().find(|c| matches!(c, 'r' | 'w' | 'a' | 'x'));
    let Some(kind) = kind else {
        return Err(it.value_error("Must have exactly one of create/read/write/append mode and at most one plus"));
    };
    if mode.contains('+') {
        return Err(it.new_exc_str("NotImplementedError", "read-write file modes are not supported"));
    }
    let open_mode = match kind {
        'r' => OpenMode::Read,
        'a' => OpenMode::Append,
        'x' => OpenMode::CreateNew,
        _ => OpenMode::Write,
    };
    let opened = it.platform.borrow_mut().open(&path, open_mode);
    let handle = match opened {
        Ok(h) => h,
        Err(e) => return Err(it.os_error(&e, &path)),
    };
    let fm = if kind == 'r' {
        let read = it.platform.borrow_mut().read_to_end(handle);
        it.platform.borrow_mut().close(handle);
        match read {
            Ok(data) => FileMode::Read { data, pos: 0 },
            Err(e) => return Err(it.os_error(&e, &path)),
        }
    } else {
        FileMode::Write { handle, buf: Vec::new(), append: kind == 'a' }
    };
    Ok(new_file(it, fm, !binary, &path))
}

fn size_arg(it: &mut Interp, a: &[Value], i: usize) -> R<i64> {
    match a.get(i) {
        None | Some(Value::None) => Ok(-1),
        Some(v) => it.index_of(v),
    }
}

fn f_read(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("read", a, 1, 2)?;
    let o = file_of(it, a)?.clone();
    let n = size_arg(it, a, 1)?;
    it.file_read(&o, n)
}

fn f_readline(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("readline", a, 1, 2)?;
    let o = file_of(it, a)?.clone();
    let n = size_arg(it, a, 1)?;
    it.file_readline(&o, n)
}

fn f_readlines(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("readlines", a, 1, 2)?;
    let o = file_of(it, a)?.clone();
    let mut out = Vec::new();
    loop {
        let l = it.file_readline(&o, -1)?;
        if l.as_str().map(|s| s.is_empty()).unwrap_or(l.as_bytes_empty()) {
            break;
        }
        out.push(l);
    }
    Ok(Value::list(out))
}

fn f_write(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("write", a, 2, 2)?;
    let o = file_of(it, a)?.clone();
    let text = matches!(&o.kind, Kind::File(f) if f.borrow().text);
    if text {
        match a[1].as_str() {
            Some(s) => {
                it.file_write(&o, s)?;
                Ok(Value::Int(s.chars().count() as i64))
            }
            None => {
                let t = it.type_name_of(&a[1]);
                Err(it.type_error(&format!("write() argument must be str, not {}", t)))
            }
        }
    } else {
        let b = it.bytes_of(&a[1])?;
        it.file_write_bytes(&o, &b)?;
        Ok(Value::Int(b.len() as i64))
    }
}

fn f_writelines(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("writelines", a, 2, 2)?;
    let items = it.iterate_to_vec(&a[1])?;
    for v in items {
        f_write(it, &[a[0].clone(), v], &[])?;
    }
    Ok(Value::None)
}

fn f_close(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let o = file_of(it, a)?.clone();
    it.file_close(&o);
    Ok(Value::None)
}

fn f_flush(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let o = file_of(it, a)?.clone();
    if let Kind::File(f) = &o.kind {
        let mut fd = f.borrow_mut();
        if matches!(fd.mode, FileMode::Closed) {
            drop(fd);
            return Err(closed_err(it));
        }
        if matches!(fd.mode, FileMode::Stdout) {
            drop(fd);
            it.flush_out();
        } else {
            flush_data(&mut fd);
        }
    }
    Ok(Value::None)
}

fn f_enter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let o = file_of(it, a)?.clone();
    if matches!(&o.kind, Kind::File(f) if matches!(f.borrow().mode, FileMode::Closed)) {
        return Err(closed_err(it));
    }
    Ok(a[0].clone())
}

fn f_exit(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let o = file_of(it, a)?.clone();
    it.file_close(&o);
    Ok(Value::Bool(false))
}

fn f_iter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let o = file_of(it, a)?.clone();
    if matches!(&o.kind, Kind::File(f) if matches!(f.borrow().mode, FileMode::Closed)) {
        return Err(closed_err(it));
    }
    Ok(a[0].clone())
}

fn f_next(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let o = file_of(it, a)?.clone();
    let l = it.file_readline(&o, -1)?;
    if l.as_str().map(|s| s.is_empty()).unwrap_or(l.as_bytes_empty()) {
        return Err(it.new_exc_str("StopIteration", ""));
    }
    Ok(l)
}

fn f_tell(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let o = file_of(it, a)?.clone();
    if let Kind::File(f) = &o.kind {
        match &f.borrow().mode {
            FileMode::Read { pos, .. } => return Ok(Value::Int(*pos as i64)),
            FileMode::Write { buf, .. } => return Ok(Value::Int(buf.len() as i64)),
            _ => {}
        }
    }
    Ok(Value::Int(0))
}

fn f_seek(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("seek", a, 2, 3)?;
    let o = file_of(it, a)?.clone();
    let off = it.index_of(&a[1])?;
    let whence = size_arg(it, a, 2)?.max(0);
    if let Kind::File(f) = &o.kind {
        if let FileMode::Read { data, pos } = &mut f.borrow_mut().mode {
            let base = match whence {
                1 => *pos as i64,
                2 => data.len() as i64,
                _ => 0,
            };
            *pos = (base + off).clamp(0, data.len() as i64) as usize;
            return Ok(Value::Int(*pos as i64));
        }
    }
    Ok(Value::Int(0))
}

fn f_true(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let o = file_of(it, a)?;
    Ok(Value::Bool(matches!(&o.kind, Kind::File(f) if !matches!(f.borrow().mode, FileMode::Closed))))
}

fn f_readable(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let o = file_of(it, a)?;
    Ok(Value::Bool(matches!(&o.kind, Kind::File(f) if matches!(f.borrow().mode, FileMode::Read { .. } | FileMode::Stdin))))
}

fn f_writable(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let o = file_of(it, a)?;
    Ok(Value::Bool(matches!(&o.kind, Kind::File(f) if matches!(f.borrow().mode, FileMode::Write { .. } | FileMode::Stdout | FileMode::Stderr))))
}

fn f_isatty(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Bool(false))
}

fn f_fileno(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let o = file_of(it, a)?;
    Ok(Value::Int(match &o.kind {
        Kind::File(f) => match f.borrow().mode {
            FileMode::Stdin => 0,
            FileMode::Stdout => 1,
            FileMode::Stderr => 2,
            _ => 3,
        },
        _ => 3,
    }))
}

pub fn init(it: &mut Interp) {
    let t = it.types.file.clone();
    it.reg(&t, "read", f_read);
    it.reg(&t, "readline", f_readline);
    it.reg(&t, "readlines", f_readlines);
    it.reg(&t, "write", f_write);
    it.reg(&t, "writelines", f_writelines);
    it.reg(&t, "close", f_close);
    it.reg(&t, "flush", f_flush);
    it.reg(&t, "__enter__", f_enter);
    it.reg(&t, "__exit__", f_exit);
    it.reg(&t, "__iter__", f_iter);
    it.reg(&t, "__next__", f_next);
    it.reg(&t, "tell", f_tell);
    it.reg(&t, "seek", f_seek);
    it.reg(&t, "seekable", f_true);
    it.reg(&t, "readable", f_readable);
    it.reg(&t, "writable", f_writable);
    it.reg(&t, "isatty", f_isatty);
    it.reg(&t, "fileno", f_fileno);
    let open_fn = it.new_native("open", open, false);
    dict_set_str(&it.builtins.clone(), "open", open_fn);
}
