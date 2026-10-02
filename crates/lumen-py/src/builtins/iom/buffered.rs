//! `BufferedReader`, `BufferedWriter`, `BufferedRandom` and `BufferedRWPair`. The three
//! single-stream classes share one state ([`Buffered`]) and one implementation; the raw stream
//! is driven through its methods, with direct descriptor access when it is a native `FileIO`.

use super::fileio::{native_fd, read_fd};
use super::{chain, getattr_opt, is_eagain, size_arg, unsupported};
use crate::bind::{KwArgs, Py, This};
use crate::object::*;
use crate::vm::Interp;

pub enum Mode {
    Reader,
    Writer,
    Random,
}

#[derive(Default)]
pub struct Buffered {
    raw: Option<Value>,
    ok: bool,
    detached: bool,
    readable: bool,
    writable: bool,
    buffer_size: usize,
    read_buf: Vec<u8>,
    read_pos: usize,
    write_buf: Vec<u8>,
}

impl Buffered {
    fn readahead(&self) -> usize {
        self.read_buf.len() - self.read_pos
    }

    fn reset_read(&mut self) {
        self.read_buf.clear();
        self.read_pos = 0;
    }

    fn take(&mut self, n: usize) -> Vec<u8> {
        let n = n.min(self.readahead());
        let out = self.read_buf[self.read_pos..self.read_pos + n].to_vec();
        self.read_pos += n;
        out
    }
}

impl Drop for Buffered {
    fn drop(&mut self) {
        if self.write_buf.is_empty() {
            return;
        }
        let Some(raw) = &self.raw else { return };
        let data = std::mem::take(&mut self.write_buf);
        let target = crate::builtins::native::with_opaque::<super::fileio::FileIO, _>(raw, |f| f.drop_target());
        if let Some(Some((fd, platform))) = target {
            if let Ok(mut p) = platform.try_borrow_mut() {
                let mut rest = &data[..];
                while !rest.is_empty() {
                    match p.fd_write(fd, rest, None) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => rest = &rest[n..],
                    }
                }
            }
        }
    }
}

/// A buffered reader for a readable, sequential raw stream.
#[lumen_bind::class(module = "_io", name = "BufferedReader")]
pub struct BufferedReader(Buffered);

/// A buffer for a writeable sequential raw stream.
#[lumen_bind::class(module = "_io", name = "BufferedWriter")]
pub struct BufferedWriter(Buffered);

/// A buffered interface to random access streams.
#[lumen_bind::class(module = "_io", name = "BufferedRandom")]
pub struct BufferedRandom(Buffered);

/// The shared state of any of the three classes; `RuntimeError` on reentrant use.
fn with_b<X>(it: &mut Interp, v: &Value, f: impl FnOnce(&mut Buffered) -> X) -> R<X> {
    let Value::Obj(o) = v else { return Err(it.type_error("expected a buffered stream")) };
    let Kind::Opaque(cell) = &o.kind else { return Err(it.type_error("expected a buffered stream")) };
    let Ok(mut b) = cell.try_borrow_mut() else {
        let r = it.repr_of(v).unwrap_or_default();
        return Err(it.new_exc_str("RuntimeError", &format!("reentrant call inside {}", r)));
    };
    let any: &mut dyn std::any::Any = &mut **b;
    if let Some(x) = any.downcast_mut::<BufferedReader>() {
        return Ok(f(&mut x.0));
    }
    if let Some(x) = any.downcast_mut::<BufferedWriter>() {
        return Ok(f(&mut x.0));
    }
    if let Some(x) = any.downcast_mut::<BufferedRandom>() {
        return Ok(f(&mut x.0));
    }
    drop(b);
    Err(it.type_error("expected a buffered stream"))
}

/// The raw stream of an initialized, attached object.
fn raw_of(it: &mut Interp, v: &Value) -> R<Value> {
    let (raw, detached) = with_b(it, v, |b| (if b.ok { b.raw.clone() } else { None }, b.detached))?;
    match raw {
        Some(r) => Ok(r),
        None if detached => Err(it.value_error("raw stream has been detached")),
        None => Err(it.value_error("I/O operation on uninitialized object")),
    }
}

fn raw_closed(it: &mut Interp, raw: &Value) -> R<bool> {
    if let Some(f) = super::exact::<super::fileio::FileIO>(it, raw) {
        return Ok(f.borrow(it)?.fd < 0);
    }
    super::attr_bool(it, raw, "closed")
}

/// CPython's `CHECK_CLOSED`: closed and nothing left to read.
fn check_closed(it: &mut Interp, v: &Value, msg: &str) -> R<Value> {
    let raw = raw_of(it, v)?;
    if raw_closed(it, &raw)? && with_b(it, v, |b| b.readahead())? == 0 {
        return Err(it.value_error(msg));
    }
    Ok(raw)
}

/// One raw read of up to `n` bytes; `None` when it would block.
fn raw_read(it: &mut Interp, raw: &Value, n: usize) -> R<Option<Vec<u8>>> {
    if let Some((fd, true, _)) = native_fd(it, raw) {
        let mut buf = vec![0u8; n];
        return match read_fd(it, fd, &mut buf)? {
            Ok(k) => {
                buf.truncate(k);
                Ok(Some(buf))
            }
            Err(e) if is_eagain(&e) => Ok(None),
            Err(e) => Err(it.os_error_io(&e, None)),
        };
    }
    let ba = Value::bytearray(vec![0; n]);
    let r = it.call_method(raw, "readinto", vec![ba.clone()])?;
    if r.is_none() {
        return Ok(None);
    }
    let k = it.index_of(&r)?;
    if k < 0 || k as usize > n {
        return Err(it.new_exc_str("OSError", &format!("raw readinto() returned invalid length {} (should have been between 0 and {})", k, n)));
    }
    let mut data = it.bytes_of(&ba)?;
    data.truncate(k as usize);
    Ok(Some(data))
}

/// One raw write; `None` when it would block.
fn raw_write(it: &mut Interp, raw: &Value, data: &[u8]) -> R<Option<usize>> {
    if let Some((fd, _, true)) = native_fd(it, raw) {
        return match it.fd_write(fd, data) {
            Ok(n) => Ok(Some(n)),
            Err(e) if is_eagain(&e) => Ok(None),
            Err(e) => Err(it.os_error_io(&e, None)),
        };
    }
    let r = it.call_method(raw, "write", vec![Value::bytes(data.to_vec())])?;
    if r.is_none() {
        return Ok(None);
    }
    let n = it.index_of(&r)?;
    if n < 0 || n as usize > data.len() {
        return Err(it.new_exc_str("OSError", &format!("raw write() returned invalid length {} (should have been between 0 and {})", n, data.len())));
    }
    Ok(Some(n as usize))
}

fn raw_seek(it: &mut Interp, raw: &Value, pos: i64, whence: i32) -> R<i64> {
    let n = if let Some((fd, _, _)) = native_fd(it, raw) {
        let r = it.platform.borrow_mut().fd_seek(fd, pos, whence);
        r.map_err(|e| it.os_error_io(&e, None))? as i64
    } else {
        let r = it.call_method(raw, "seek", vec![Value::Int(pos), Value::Int(whence as i64)])?;
        it.index_of(&r)?
    };
    if n < 0 {
        return Err(it.new_exc_str("OSError", &format!("Raw stream returned invalid position {}", n)));
    }
    Ok(n)
}

fn raw_tell(it: &mut Interp, raw: &Value) -> R<i64> {
    let n = if let Some((fd, _, _)) = native_fd(it, raw) {
        let r = it.platform.borrow_mut().fd_seek(fd, 0, 1);
        r.map_err(|e| it.os_error_io(&e, None))? as i64
    } else {
        let r = it.call_method(raw, "tell", Vec::new())?;
        it.index_of(&r)?
    };
    if n < 0 {
        return Err(it.new_exc_str("OSError", &format!("Raw stream returned invalid position {}", n)));
    }
    Ok(n)
}

fn check_raw(it: &mut Interp, raw: &Value, method: &str, msg: &str) -> R<()> {
    if !super::call_bool(it, raw, method)? {
        return Err(unsupported(it, msg));
    }
    Ok(())
}

fn blocking_error(it: &mut Interp, msg: &str, written: usize) -> Obj {
    let t = it.exc_type("BlockingIOError");
    let errno = lumen_os::errno::errno_of_code("EAGAIN").unwrap_or(35) as i64;
    it.new_exc(&t, vec![Value::Int(errno), Value::str(msg), Value::Int(written as i64)])
}

// ---- shared implementation ---------------------------------------------------------------

fn init(it: &mut Interp, v: &Value, kind: Mode, raw: &Value, buffer_size: i64) -> R<()> {
    with_b(it, v, |b| {
        b.ok = false;
        b.detached = false;
    })?;
    let (readable, writable) = match kind {
        Mode::Reader => (true, false),
        Mode::Writer => (false, true),
        Mode::Random => (true, true),
    };
    if readable && writable {
        check_raw(it, raw, "seekable", "File or stream is not seekable.")?;
    }
    if readable {
        check_raw(it, raw, "readable", "File or stream is not readable.")?;
    }
    if writable {
        check_raw(it, raw, "writable", "File or stream is not writable.")?;
    }
    if buffer_size <= 0 {
        return Err(it.value_error("buffer size must be strictly positive"));
    }
    with_b(it, v, |b| {
        b.raw = Some(raw.clone());
        b.ok = true;
        b.detached = false;
        b.readable = readable;
        b.writable = writable;
        b.buffer_size = buffer_size as usize;
        b.read_buf = Vec::new();
        b.read_pos = 0;
        b.write_buf = Vec::new();
    })
}

/// Writes out the write buffer.
fn flush_unlocked(it: &mut Interp, v: &Value, raw: &Value) -> R<()> {
    loop {
        let data = with_b(it, v, |b| std::mem::take(&mut b.write_buf))?;
        if data.is_empty() {
            return Ok(());
        }
        let r = raw_write(it, raw, &data);
        let n = match r {
            Ok(Some(n)) => n,
            Ok(None) => {
                with_b(it, v, |b| b.write_buf = data)?;
                return Err(blocking_error(it, "write could not complete without blocking", 0));
            }
            Err(e) => {
                with_b(it, v, |b| {
                    let mut d = data;
                    d.append(&mut b.write_buf);
                    b.write_buf = d;
                })?;
                return Err(e);
            }
        };
        with_b(it, v, |b| {
            let mut rest = data[n..].to_vec();
            rest.append(&mut b.write_buf);
            b.write_buf = rest;
        })?;
    }
}

/// CPython's `buffered_flush_and_rewind_unlocked`: flush the writes, then undo the read-ahead.
fn flush_and_rewind(it: &mut Interp, v: &Value, raw: &Value) -> R<()> {
    let (writable, readable) = with_b(it, v, |b| (b.writable, b.readable))?;
    if writable {
        flush_unlocked(it, v, raw)?;
    }
    if readable {
        let ahead = with_b(it, v, |b| b.readahead())?;
        if ahead > 0 {
            raw_seek(it, raw, -(ahead as i64), 1)?;
        }
        with_b(it, v, Buffered::reset_read)?;
    }
    Ok(())
}

/// Before a read on a random-access stream: write out pending data at the logical position.
fn prepare_read(it: &mut Interp, v: &Value, raw: &Value) -> R<()> {
    let pending = with_b(it, v, |b| b.writable && !b.write_buf.is_empty())?;
    if pending {
        flush_and_rewind(it, v, raw)?;
    }
    Ok(())
}

fn read(it: &mut Interp, v: &Value, size: Option<&Value>) -> R<Value> {
    raw_of(it, v)?;
    let n = size_arg(it, size)?;
    if n < -1 {
        return Err(it.value_error("read length must be non-negative or -1"));
    }
    let raw = check_closed(it, v, "read of closed file")?;
    if n == -1 {
        return read_all(it, v, &raw);
    }
    let n = n as usize;
    if let Some(d) = with_b(it, v, |b| (n <= b.readahead()).then(|| b.take(n)))? {
        return Ok(Value::bytes(d));
    }
    prepare_read(it, v, &raw)?;
    let (mut out, bs) = with_b(it, v, |b| {
        let have = b.readahead();
        (b.take(have), b.buffer_size)
    })?;
    with_b(it, v, Buffered::reset_read)?;
    let mut blocked = false;
    while out.len() < n {
        let want = bs.max(n - out.len());
        match raw_read(it, &raw, want)? {
            None => {
                blocked = true;
                break;
            }
            Some(c) if c.is_empty() => break,
            Some(c) => out.extend_from_slice(&c),
        }
    }
    if out.len() > n {
        let extra = out.split_off(n);
        with_b(it, v, |b| {
            b.read_buf = extra;
            b.read_pos = 0;
        })?;
    }
    if out.is_empty() && blocked {
        return Ok(Value::None);
    }
    Ok(Value::bytes(out))
}

fn read_all(it: &mut Interp, v: &Value, raw: &Value) -> R<Value> {
    prepare_read(it, v, raw)?;
    let mut data = with_b(it, v, |b| {
        let d = b.read_buf[b.read_pos..].to_vec();
        b.reset_read();
        d
    })?;
    if getattr_opt(it, raw, "readall")?.is_some() {
        let chunk = it.call_method(raw, "readall", Vec::new())?;
        match super::bytes_result(it, &chunk, "readall")? {
            None if data.is_empty() => return Ok(Value::None),
            None => {}
            Some(c) => data.extend_from_slice(&c),
        }
        return Ok(Value::bytes(data));
    }
    loop {
        let chunk = it.call_method(raw, "read", Vec::new())?;
        match super::bytes_result(it, &chunk, "read")? {
            None => {
                if data.is_empty() {
                    return Ok(Value::None);
                }
                break;
            }
            Some(c) if c.is_empty() => break,
            Some(c) => data.extend_from_slice(&c),
        }
    }
    Ok(Value::bytes(data))
}

/// Fills the read buffer with one raw read; false at end of file (or when it would block).
fn fill(it: &mut Interp, v: &Value, raw: &Value) -> R<bool> {
    let bs = with_b(it, v, |b| b.buffer_size)?;
    match raw_read(it, raw, bs)? {
        Some(c) if !c.is_empty() => {
            with_b(it, v, |b| {
                b.read_buf.drain(..b.read_pos);
                b.read_pos = 0;
                b.read_buf.extend_from_slice(&c);
            })?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn peek(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
    let raw = check_closed(it, v, "peek of closed file")?;
    prepare_read(it, v, &raw)?;
    if with_b(it, v, |b| b.readahead())? == 0 {
        with_b(it, v, Buffered::reset_read)?;
        fill(it, v, &raw)?;
    }
    with_b(it, v, |b| b.read_buf[b.read_pos..].to_vec())
}

fn read1(it: &mut Interp, v: &Value, size: i64) -> R<Vec<u8>> {
    raw_of(it, v)?;
    let bs = with_b(it, v, |b| b.buffer_size)?;
    let n = if size < 0 { bs } else { size as usize };
    let raw = check_closed(it, v, "read of closed file")?;
    if n == 0 {
        return Ok(Vec::new());
    }
    if let Some(d) = with_b(it, v, |b| (b.readahead() > 0).then(|| b.take(n)))? {
        return Ok(d);
    }
    prepare_read(it, v, &raw)?;
    with_b(it, v, Buffered::reset_read)?;
    Ok(raw_read(it, &raw, n)?.unwrap_or_default())
}

fn readinto(it: &mut Interp, v: &Value, buf: &mut [u8], one: bool) -> R<usize> {
    let raw = check_closed(it, v, "readinto of closed file")?;
    let mut written = with_b(it, v, |b| {
        let d = b.take(buf.len());
        buf[..d.len()].copy_from_slice(&d);
        d.len()
    })?;
    if written == buf.len() {
        return Ok(written);
    }
    prepare_read(it, v, &raw)?;
    with_b(it, v, Buffered::reset_read)?;
    let bs = with_b(it, v, |b| b.buffer_size)?;
    while written < buf.len() {
        if one && written > 0 {
            break;
        }
        let remaining = buf.len() - written;
        if remaining > bs {
            match raw_read(it, &raw, remaining)? {
                Some(c) if !c.is_empty() => {
                    buf[written..written + c.len()].copy_from_slice(&c);
                    written += c.len();
                }
                _ => break,
            }
        } else {
            if !fill(it, v, &raw)? {
                break;
            }
            written += with_b(it, v, |b| {
                let d = b.take(remaining);
                buf[written..written + d.len()].copy_from_slice(&d);
                d.len()
            })?;
        }
    }
    Ok(written)
}

/// CPython's `_buffered_readline`.
fn readline(it: &mut Interp, v: &Value, limit: i64) -> R<Vec<u8>> {
    let raw = check_closed(it, v, "readline of closed file")?;
    let mut out: Vec<u8> = Vec::new();
    let mut first = true;
    loop {
        let done = with_b(it, v, |b| {
            let avail = &b.read_buf[b.read_pos..];
            let room = if limit < 0 { avail.len() } else { (limit as usize - out.len()).min(avail.len()) };
            let n = avail[..room].iter().position(|&c| c == b'\n').map_or(room, |p| p + 1);
            out.extend_from_slice(&avail[..n]);
            b.read_pos += n;
            out.last() == Some(&b'\n') || (limit >= 0 && out.len() >= limit as usize)
        })?;
        if done {
            return Ok(out);
        }
        if first {
            prepare_read(it, v, &raw)?;
            first = false;
        }
        with_b(it, v, Buffered::reset_read)?;
        if !fill(it, v, &raw)? {
            return Ok(out);
        }
    }
}

fn write(it: &mut Interp, v: &Value, data: &[u8]) -> R<usize> {
    let raw = raw_of(it, v)?;
    if raw_closed(it, &raw)? {
        return Err(it.value_error("write to closed file"));
    }
    let (ahead, bs) = with_b(it, v, |b| (if b.readable { b.readahead() } else { 0 }, b.buffer_size))?;
    if ahead > 0 {
        raw_seek(it, &raw, -(ahead as i64), 1)?;
    }
    with_b(it, v, |b| {
        if b.readable {
            b.reset_read();
        }
    })?;
    let fits = with_b(it, v, |b| b.write_buf.len() + data.len() <= bs)?;
    if fits {
        with_b(it, v, |b| b.write_buf.extend_from_slice(data))?;
        return Ok(data.len());
    }
    match flush_unlocked(it, v, &raw) {
        Ok(()) => {}
        Err(e) if it.exc_is(&e, "BlockingIOError") => {
            let room = with_b(it, v, |b| bs.saturating_sub(b.write_buf.len()))?;
            let n = room.min(data.len());
            with_b(it, v, |b| b.write_buf.extend_from_slice(&data[..n]))?;
            return Err(blocking_error(it, "write could not complete without blocking", n));
        }
        Err(e) => return Err(e),
    }
    if data.len() < bs {
        with_b(it, v, |b| b.write_buf.extend_from_slice(data))?;
        return Ok(data.len());
    }
    let mut written = 0;
    while data.len() - written >= bs {
        match raw_write(it, &raw, &data[written..])? {
            Some(n) => written += n,
            None => {
                let n = (data.len() - written).min(bs);
                with_b(it, v, |b| b.write_buf.extend_from_slice(&data[written..written + n]))?;
                return Err(blocking_error(it, "write could not complete without blocking", written + n));
            }
        }
    }
    with_b(it, v, |b| b.write_buf.extend_from_slice(&data[written..]))?;
    Ok(data.len())
}

fn flush(it: &mut Interp, v: &Value) -> R<()> {
    let raw = raw_of(it, v)?;
    if raw_closed(it, &raw)? {
        return Err(it.value_error("flush of closed file"));
    }
    flush_and_rewind(it, v, &raw)
}

fn tell(it: &mut Interp, v: &Value) -> R<i64> {
    let raw = raw_of(it, v)?;
    let pos = raw_tell(it, &raw)?;
    let (ahead, pending) = with_b(it, v, |b| (b.readahead() as i64, b.write_buf.len() as i64))?;
    Ok((pos - ahead + pending).max(0))
}

fn seek(it: &mut Interp, v: &Value, target: &Value, whence: i32) -> R<i64> {
    raw_of(it, v)?;
    if !(0..=2).contains(&whence) {
        return Err(it.value_error(&format!("whence value {} unsupported", whence)));
    }
    let raw = check_closed(it, v, "seek of closed file")?;
    check_raw(it, &raw, "seekable", "File or stream is not seekable.")?;
    if !it.has_index(target) {
        let t = it.type_name_of(target);
        return Err(it.type_error(&format!("'{}' object cannot be interpreted as an integer", t)));
    }
    let target = it.index_of(target)?;
    let (readable, ahead, pending) = with_b(it, v, |b| (b.readable, b.readahead() as i64, !b.write_buf.is_empty()))?;
    if whence != 2 && readable && ahead > 0 && !pending {
        let current = raw_tell(it, &raw)?;
        let logical = current - ahead;
        let offset = if whence == 0 { target - logical } else { target };
        let done = with_b(it, v, |b| {
            if offset >= -(b.read_pos as i64) && offset <= ahead {
                b.read_pos = (b.read_pos as i64 + offset) as usize;
                true
            } else {
                false
            }
        })?;
        if done {
            return Ok(logical + offset);
        }
    }
    let writable = with_b(it, v, |b| b.writable)?;
    if writable {
        flush_unlocked(it, v, &raw)?;
    }
    let ahead = with_b(it, v, |b| b.readahead() as i64)?;
    let target = if whence == 1 { target - ahead } else { target };
    let n = raw_seek(it, &raw, target, whence)?;
    with_b(it, v, Buffered::reset_read)?;
    Ok(n)
}

fn truncate(it: &mut Interp, v: &Value, pos: Option<&Value>) -> R<Value> {
    let raw = check_closed(it, v, "truncate of closed file")?;
    if !with_b(it, v, |b| b.writable)? {
        return Err(unsupported(it, "truncate"));
    }
    flush_and_rewind(it, v, &raw)?;
    let pos = pos.cloned().unwrap_or(Value::None);
    it.call_method(&raw, "truncate", vec![pos])
}

fn close(it: &mut Interp, v: &Value) -> R<()> {
    let raw = raw_of(it, v)?;
    if raw_closed(it, &raw)? {
        return Ok(());
    }
    let r = it.call_method(v, "flush", Vec::new());
    let c = it.call_method(&raw, "close", Vec::new());
    with_b(it, v, Buffered::reset_read)?;
    match (r, c) {
        (Err(e), Err(e2)) => {
            chain(&e2, &e);
            Err(e2)
        }
        (Err(e), Ok(_)) | (Ok(_), Err(e)) => Err(e),
        _ => Ok(()),
    }
}

fn detach(it: &mut Interp, v: &Value) -> R<Value> {
    let raw = raw_of(it, v)?;
    it.call_method(v, "flush", Vec::new())?;
    with_b(it, v, |b| {
        b.raw = None;
        b.detached = true;
        b.ok = false;
    })?;
    Ok(raw)
}

fn raw_call(it: &mut Interp, v: &Value, name: &str) -> R<Value> {
    let raw = raw_of(it, v)?;
    it.call_method(&raw, name, Vec::new())
}

fn raw_attr(it: &mut Interp, v: &Value, name: &str) -> R<Value> {
    let raw = raw_of(it, v)?;
    it.get_attr_str(&raw, name)
}

fn repr(it: &mut Interp, v: &Value) -> R<String> {
    let tn = it.tp_name_of(v);
    match it.get_attr_str(v, "name") {
        Ok(n) => {
            let r = it.repr_of(&n)?;
            Ok(format!("<{} name={}>", tn, r))
        }
        Err(e) if it.exc_is(&e, "AttributeError") || it.exc_is(&e, "ValueError") => Ok(format!("<{}>", tn)),
        Err(e) => Err(e),
    }
}

fn next_line<T: lumen_bind::Class + lumen_bind::Methods<super::PyHost>>(it: &mut Interp, v: &Value) -> R<Option<Value>> {
    let line = if super::exact::<T>(it, v).is_some() {
        Value::bytes(readline(it, v, -1)?)
    } else {
        let l = it.call_method(v, "readline", Vec::new())?;
        if !matches!(&l, Value::Obj(o) if matches!(o.kind, Kind::Bytes(_))) {
            let t = it.type_name_of(&l);
            return Err(it.new_exc_str("OSError", &format!("readline() should have returned a bytes object, not '{}'", t)));
        }
        l
    };
    Ok((it.len_of(&line)? > 0).then_some(line))
}

fn getstate(it: &mut Interp, v: &Value) -> R<Value> {
    let t = it.type_name_of(v);
    Err(it.type_error(&format!("cannot pickle '{}' instances", t)))
}

fn closed_attr(it: &mut Interp, v: &Value) -> R<Value> {
    let raw = raw_of(it, v)?;
    if let Some(f) = super::exact::<super::fileio::FileIO>(it, &raw) {
        return Ok(Value::Bool(f.borrow(it)?.fd < 0));
    }
    it.get_attr_str(&raw, "closed")
}

/// A new buffered object of exactly the native class for `kind`.
pub fn new_buffered(it: &mut Interp, kind: Mode, raw: Value, size: usize) -> R<Value> {
    let v = match kind {
        Mode::Reader => Py::new(it, BufferedReader(Buffered::default())).into_value(),
        Mode::Writer => Py::new(it, BufferedWriter(Buffered::default())).into_value(),
        Mode::Random => Py::new(it, BufferedRandom(Buffered::default())).into_value(),
    };
    init(it, &v, kind, &raw, size as i64)?;
    Ok(v)
}

/// Writes `data` through a buffered object (the direct path of `TextIOWrapper`).
pub fn write_bytes(it: &mut Interp, v: &Value, data: &[u8]) -> R<usize> {
    write(it, v, data)
}

/// Whether `v` is one of the native buffered classes (not a Python subclass).
pub fn is_native(it: &mut Interp, v: &Value) -> bool {
    super::exact::<BufferedReader>(it, v).is_some() || super::exact::<BufferedWriter>(it, v).is_some() || super::exact::<BufferedRandom>(it, v).is_some()
}

/// `v.closed` of a native buffered object over a native `FileIO`, without method calls.
pub fn native_closed(it: &mut Interp, v: &Value) -> Option<bool> {
    if !is_native(it, v) {
        return None;
    }
    let raw = with_b(it, v, |b| b.raw.clone()).ok()??;
    let f = super::exact::<super::fileio::FileIO>(it, &raw)?;
    let fd = f.borrow(it).ok()?.fd;
    Some(fd < 0)
}

/// Reads through a native buffered object: `read1(size)` when `one`, else `read(size)`.
pub fn read_bytes(it: &mut Interp, v: &Value, size: i64, one: bool) -> R<Value> {
    if one {
        return read1(it, v, size).map(Value::bytes);
    }
    read(it, v, Some(&Value::Int(size)))
}

/// Appends `data` to the write buffer of a native buffered object without writing it out (for
/// a text layer that is being dropped); its own drop then writes it.
pub fn append_pending(v: &Value, data: &[u8]) {
    use crate::builtins::native::with_opaque;
    let _ = with_opaque::<BufferedWriter, _>(v, |b| b.0.write_buf.extend_from_slice(data))
        .or_else(|| with_opaque::<BufferedRandom, _>(v, |b| b.0.write_buf.extend_from_slice(data)));
}

pub fn flush_native(it: &mut Interp, v: &Value) -> R<()> {
    flush(it, v)
}

// ---- the classes ---------------------------------------------------------------------------

#[lumen_bind::methods]
impl BufferedReader {
    #[constructor]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> BufferedReader {
        let _ = (args, kwargs);
        BufferedReader(Buffered::default())
    }

    #[proto(init)]
    fn __init__(slf: This<Py<Self>>, it: &mut Interp, #[kw] raw: &Value, #[kw] #[default(8192)] buffer_size: i64) -> R<()> {
        init(it, slf.0.value(), Mode::Reader, raw, buffer_size)
    }

    fn read(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Value> {
        read(it, slf.0.value(), size)
    }

    fn peek(slf: This<Py<Self>>, it: &mut Interp, #[default(0)] size: i64) -> R<Vec<u8>> {
        let _ = size;
        peek(it, slf.0.value())
    }

    fn read1(slf: This<Py<Self>>, it: &mut Interp, #[default(-1)] size: i64) -> R<Vec<u8>> {
        read1(it, slf.0.value(), size)
    }

    fn readinto(slf: This<Py<Self>>, it: &mut Interp, buffer: &mut [u8]) -> R<usize> {
        readinto(it, slf.0.value(), buffer, false)
    }

    fn readinto1(slf: This<Py<Self>>, it: &mut Interp, buffer: &mut [u8]) -> R<usize> {
        readinto(it, slf.0.value(), buffer, true)
    }

    fn readline(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Vec<u8>> {
        let n = size_arg(it, size)?;
        readline(it, slf.0.value(), n)
    }

    fn seek(slf: This<Py<Self>>, it: &mut Interp, target: &Value, #[default(0)] whence: i32) -> R<i64> {
        seek(it, slf.0.value(), target, whence)
    }

    fn tell(slf: This<Py<Self>>, it: &mut Interp) -> R<i64> {
        tell(it, slf.0.value())
    }

    fn truncate(slf: This<Py<Self>>, it: &mut Interp, pos: Option<&Value>) -> R<Value> {
        truncate(it, slf.0.value(), pos)
    }

    fn flush(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "flush")
    }

    fn close(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
        close(it, slf.0.value())
    }

    fn detach(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        detach(it, slf.0.value())
    }

    fn seekable(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "seekable")
    }

    fn readable(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "readable")
    }

    fn fileno(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "fileno")
    }

    fn isatty(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "isatty")
    }

    #[getter]
    fn raw(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_of(it, slf.0.value())
    }

    #[getter]
    fn closed(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        closed_attr(it, slf.0.value())
    }

    #[getter]
    fn name(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_attr(it, slf.0.value(), "name")
    }

    #[getter]
    fn mode(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_attr(it, slf.0.value(), "mode")
    }

    fn __getstate__(slf: This<Value>, it: &mut Interp) -> R<Value> {
        getstate(it, &slf.0)
    }

    #[proto(next)]
    fn __next__(slf: This<Py<Self>>, it: &mut Interp) -> R<Option<Value>> {
        next_line::<BufferedReader>(it, slf.0.value())
    }

    #[proto(repr)]
    fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        repr(it, slf.0.value())
    }
}

#[lumen_bind::methods]
impl BufferedWriter {
    #[constructor]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> BufferedWriter {
        let _ = (args, kwargs);
        BufferedWriter(Buffered::default())
    }

    #[proto(init)]
    fn __init__(slf: This<Py<Self>>, it: &mut Interp, #[kw] raw: &Value, #[kw] #[default(8192)] buffer_size: i64) -> R<()> {
        init(it, slf.0.value(), Mode::Writer, raw, buffer_size)
    }

    fn write(slf: This<Py<Self>>, it: &mut Interp, buffer: &[u8]) -> R<usize> {
        write(it, slf.0.value(), buffer)
    }

    fn seek(slf: This<Py<Self>>, it: &mut Interp, target: &Value, #[default(0)] whence: i32) -> R<i64> {
        seek(it, slf.0.value(), target, whence)
    }

    fn tell(slf: This<Py<Self>>, it: &mut Interp) -> R<i64> {
        tell(it, slf.0.value())
    }

    fn truncate(slf: This<Py<Self>>, it: &mut Interp, pos: Option<&Value>) -> R<Value> {
        truncate(it, slf.0.value(), pos)
    }

    fn flush(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
        flush(it, slf.0.value())
    }

    fn close(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
        close(it, slf.0.value())
    }

    fn detach(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        detach(it, slf.0.value())
    }

    fn seekable(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "seekable")
    }

    fn writable(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "writable")
    }

    fn fileno(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "fileno")
    }

    fn isatty(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "isatty")
    }

    #[getter]
    fn raw(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_of(it, slf.0.value())
    }

    #[getter]
    fn closed(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        closed_attr(it, slf.0.value())
    }

    #[getter]
    fn name(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_attr(it, slf.0.value(), "name")
    }

    #[getter]
    fn mode(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_attr(it, slf.0.value(), "mode")
    }

    fn __getstate__(slf: This<Value>, it: &mut Interp) -> R<Value> {
        getstate(it, &slf.0)
    }

    #[proto(repr)]
    fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        repr(it, slf.0.value())
    }
}

#[lumen_bind::methods]
impl BufferedRandom {
    #[constructor]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> BufferedRandom {
        let _ = (args, kwargs);
        BufferedRandom(Buffered::default())
    }

    #[proto(init)]
    fn __init__(slf: This<Py<Self>>, it: &mut Interp, #[kw] raw: &Value, #[kw] #[default(8192)] buffer_size: i64) -> R<()> {
        init(it, slf.0.value(), Mode::Random, raw, buffer_size)
    }

    fn read(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Value> {
        read(it, slf.0.value(), size)
    }

    fn peek(slf: This<Py<Self>>, it: &mut Interp, #[default(0)] size: i64) -> R<Vec<u8>> {
        let _ = size;
        peek(it, slf.0.value())
    }

    fn read1(slf: This<Py<Self>>, it: &mut Interp, #[default(-1)] size: i64) -> R<Vec<u8>> {
        read1(it, slf.0.value(), size)
    }

    fn readinto(slf: This<Py<Self>>, it: &mut Interp, buffer: &mut [u8]) -> R<usize> {
        readinto(it, slf.0.value(), buffer, false)
    }

    fn readinto1(slf: This<Py<Self>>, it: &mut Interp, buffer: &mut [u8]) -> R<usize> {
        readinto(it, slf.0.value(), buffer, true)
    }

    fn readline(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Vec<u8>> {
        let n = size_arg(it, size)?;
        readline(it, slf.0.value(), n)
    }

    fn write(slf: This<Py<Self>>, it: &mut Interp, buffer: &[u8]) -> R<usize> {
        write(it, slf.0.value(), buffer)
    }

    fn seek(slf: This<Py<Self>>, it: &mut Interp, target: &Value, #[default(0)] whence: i32) -> R<i64> {
        seek(it, slf.0.value(), target, whence)
    }

    fn tell(slf: This<Py<Self>>, it: &mut Interp) -> R<i64> {
        tell(it, slf.0.value())
    }

    fn truncate(slf: This<Py<Self>>, it: &mut Interp, pos: Option<&Value>) -> R<Value> {
        truncate(it, slf.0.value(), pos)
    }

    fn flush(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
        flush(it, slf.0.value())
    }

    fn close(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
        close(it, slf.0.value())
    }

    fn detach(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        detach(it, slf.0.value())
    }

    fn seekable(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "seekable")
    }

    fn readable(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "readable")
    }

    fn writable(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "writable")
    }

    fn fileno(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "fileno")
    }

    fn isatty(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_call(it, slf.0.value(), "isatty")
    }

    #[getter]
    fn raw(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_of(it, slf.0.value())
    }

    #[getter]
    fn closed(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        closed_attr(it, slf.0.value())
    }

    #[getter]
    fn name(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_attr(it, slf.0.value(), "name")
    }

    #[getter]
    fn mode(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        raw_attr(it, slf.0.value(), "mode")
    }

    fn __getstate__(slf: This<Value>, it: &mut Interp) -> R<Value> {
        getstate(it, &slf.0)
    }

    #[proto(next)]
    fn __next__(slf: This<Py<Self>>, it: &mut Interp) -> R<Option<Value>> {
        next_line::<BufferedRandom>(it, slf.0.value())
    }

    #[proto(repr)]
    fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        repr(it, slf.0.value())
    }
}

/// A buffered reader and writer object together.
#[lumen_bind::class(module = "_io", name = "BufferedRWPair")]
pub struct BufferedRWPair {
    reader: Option<Value>,
    writer: Option<Value>,
}

fn pair(it: &mut Interp, slf: &Py<BufferedRWPair>) -> R<(Value, Value)> {
    let s = slf.borrow(it)?;
    match (&s.reader, &s.writer) {
        (Some(r), Some(w)) => Ok((r.clone(), w.clone())),
        _ => {
            drop(s);
            Err(it.value_error("I/O operation on uninitialized object"))
        }
    }
}

fn forward(it: &mut Interp, slf: &Py<BufferedRWPair>, writer: bool, name: &str, args: Vec<Value>) -> R<Value> {
    let (r, w) = pair(it, slf)?;
    it.call_method(if writer { &w } else { &r }, name, args)
}

#[lumen_bind::methods]
impl BufferedRWPair {
    #[constructor]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> BufferedRWPair {
        let _ = (args, kwargs);
        BufferedRWPair { reader: None, writer: None }
    }

    #[proto(init)]
    fn __init__(slf: This<Py<Self>>, it: &mut Interp, reader: &Value, writer: &Value, #[default(8192)] buffer_size: i64) -> R<()> {
        check_raw(it, reader, "readable", "File or stream is not readable.")?;
        check_raw(it, writer, "writable", "File or stream is not writable.")?;
        let size = if buffer_size <= 0 { 0 } else { buffer_size as usize };
        let r = Py::new(it, BufferedReader(Buffered::default())).into_value();
        init(it, &r, Mode::Reader, reader, buffer_size)?;
        let w = Py::new(it, BufferedWriter(Buffered::default())).into_value();
        init(it, &w, Mode::Writer, writer, size as i64)?;
        let mut s = slf.0.borrow_mut(it)?;
        s.reader = Some(r);
        s.writer = Some(w);
        Ok(())
    }

    #[method(hint(py(text_signature = "")))]
    fn read(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<Value> {
        let a = size.cloned().unwrap_or(Value::None);
        forward(it, &slf.0, false, "read", vec![a])
    }

    #[method(hint(py(text_signature = "")))]
    fn peek(slf: This<Py<Self>>, it: &mut Interp, #[default(0)] size: i64) -> R<Value> {
        forward(it, &slf.0, false, "peek", vec![Value::Int(size)])
    }

    #[method(hint(py(text_signature = "")))]
    fn read1(slf: This<Py<Self>>, it: &mut Interp, #[default(-1)] size: i64) -> R<Value> {
        forward(it, &slf.0, false, "read1", vec![Value::Int(size)])
    }

    #[method(hint(py(text_signature = "")))]
    fn readinto(slf: This<Py<Self>>, it: &mut Interp, buffer: &Value) -> R<Value> {
        forward(it, &slf.0, false, "readinto", vec![buffer.clone()])
    }

    #[method(hint(py(text_signature = "")))]
    fn readinto1(slf: This<Py<Self>>, it: &mut Interp, buffer: &Value) -> R<Value> {
        forward(it, &slf.0, false, "readinto1", vec![buffer.clone()])
    }

    #[method(hint(py(text_signature = "")))]
    fn write(slf: This<Py<Self>>, it: &mut Interp, buffer: &Value) -> R<Value> {
        forward(it, &slf.0, true, "write", vec![buffer.clone()])
    }

    #[method(hint(py(text_signature = "")))]
    fn flush(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        forward(it, &slf.0, true, "flush", Vec::new())
    }

    #[method(hint(py(text_signature = "")))]
    fn readable(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        forward(it, &slf.0, false, "readable", Vec::new())
    }

    #[method(hint(py(text_signature = "")))]
    fn writable(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        forward(it, &slf.0, true, "writable", Vec::new())
    }

    #[method(hint(py(text_signature = "")))]
    fn close(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
        let w = forward(it, &slf.0, true, "close", Vec::new());
        let r = forward(it, &slf.0, false, "close", Vec::new());
        match (w, r) {
            (Err(e), Err(e2)) => {
                chain(&e2, &e);
                Err(e2)
            }
            (Err(e), _) | (_, Err(e)) => Err(e),
            _ => Ok(()),
        }
    }

    #[method(hint(py(text_signature = "")))]
    fn isatty(slf: This<Py<Self>>, it: &mut Interp) -> R<bool> {
        let w = forward(it, &slf.0, true, "isatty", Vec::new())?;
        if it.truthy(&w)? {
            return Ok(true);
        }
        let r = forward(it, &slf.0, false, "isatty", Vec::new())?;
        it.truthy(&r)
    }

    #[getter]
    fn closed(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let (_, w) = pair(it, &slf.0)?;
        it.get_attr_str(&w, "closed")
    }
}
