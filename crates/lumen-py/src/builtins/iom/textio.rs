//! `TextIOWrapper` and `IncrementalNewlineDecoder`: text over a buffered binary stream, with
//! the codec machinery of [`crate::codecs`] and CPython's newline and `tell()` cookie rules.

use super::stringio::find_line_ending;
use super::{chain, getattr_opt, unsupported};
use crate::bind::{KwArgs, Py, This};
use crate::codecs::{IncrementalDecoder, IncrementalEncoder};
use crate::object::*;
use crate::pyint::{BigInt, PyInt};
use crate::vm::Interp;
use lumen_common::smuggle::{code_points, push_code_point};

const SEEN_LF: u8 = 1;
const SEEN_CR: u8 = 2;
const SEEN_CRLF: u8 = 4;

/// The newline half of an incremental newline decoder: holds back a trailing `\r` until the
/// next input, records the newline kinds seen, and translates them to `\n` when asked.
pub struct NlState {
    pub translate: bool,
    pub pendingcr: bool,
    pub seennl: u8,
}

impl NlState {
    pub fn new(translate: bool) -> NlState {
        NlState {
            translate,
            pendingcr: false,
            seennl: 0,
        }
    }

    /// Newline handling of already decoded `output`.
    pub fn feed(&mut self, mut output: String, final_: bool) -> String {
        if self.pendingcr && (final_ || !output.is_empty()) {
            output.insert(0, '\r');
            self.pendingcr = false;
        }
        if !final_ && output.ends_with('\r') {
            output.pop();
            self.pendingcr = true;
        }
        if !output.contains('\r') {
            if output.contains('\n') {
                self.seennl |= SEEN_LF;
            }
            return output;
        }
        let crlf = output.matches("\r\n").count();
        let cr = output.matches('\r').count() - crlf;
        let lf = output.matches('\n').count() - crlf;
        if lf > 0 {
            self.seennl |= SEEN_LF;
        }
        if cr > 0 {
            self.seennl |= SEEN_CR;
        }
        if crlf > 0 {
            self.seennl |= SEEN_CRLF;
        }
        if self.translate {
            if crlf > 0 {
                output = output.replace("\r\n", "\n");
            }
            if cr > 0 {
                output = output.replace('\r', "\n");
            }
        }
        output
    }

    pub fn newlines(&self) -> Value {
        let s = |x: &str| Value::str(x);
        match self.seennl {
            0 => Value::None,
            1 => s("\n"),
            2 => s("\r"),
            3 => Value::tuple(vec![s("\r"), s("\n")]),
            4 => s("\r\n"),
            5 => Value::tuple(vec![s("\n"), s("\r\n")]),
            6 => Value::tuple(vec![s("\r"), s("\r\n")]),
            _ => Value::tuple(vec![s("\r"), s("\n"), s("\r\n")]),
        }
    }

    pub fn reset(&mut self) {
        self.seennl = 0;
        self.pendingcr = false;
    }
}

/// A text decoder: the codec, then the newline decoder in universal-newlines mode.
struct Dec {
    codec: IncrementalDecoder,
    nl: Option<NlState>,
}

impl Dec {
    fn decode(&mut self, it: &mut Interp, input: &[u8], final_: bool) -> R<String> {
        let out = self.codec.decode(it, input, final_)?;
        Ok(match &mut self.nl {
            Some(nl) => nl.feed(out, final_),
            None => out,
        })
    }

    fn getstate(&self, it: &mut Interp) -> R<(Vec<u8>, i64)> {
        let (b, f) = self.codec.getstate(it)?;
        Ok(match &self.nl {
            Some(nl) => (b, (f << 1) | nl.pendingcr as i64),
            None => (b, f),
        })
    }

    fn setstate(&mut self, it: &mut Interp, b: Vec<u8>, flag: i64) -> R<()> {
        match &mut self.nl {
            Some(nl) => {
                nl.pendingcr = flag & 1 != 0;
                self.codec.setstate(it, b, flag >> 1)
            }
            None => self.codec.setstate(it, b, flag),
        }
    }

    fn reset(&mut self, it: &mut Interp) -> R<()> {
        if let Some(nl) = &mut self.nl {
            nl.reset();
        }
        self.codec.reset(it)
    }
}

/// CPython's `tell()` cookie: packed little-endian as `start_pos` (64 bits), `dec_flags`,
/// `bytes_to_feed`, `chars_to_skip` (32 bits each) and `need_eof` (8 bits).
#[derive(Default, Clone, Copy)]
struct Cookie {
    start_pos: i64,
    dec_flags: i64,
    bytes_to_feed: i64,
    chars_to_skip: i64,
    need_eof: bool,
}

impl Cookie {
    fn build(&self) -> Value {
        let mut b = Vec::with_capacity(21);
        b.extend_from_slice(&self.start_pos.to_le_bytes());
        b.extend_from_slice(&(self.dec_flags as i32).to_le_bytes());
        b.extend_from_slice(&(self.bytes_to_feed as i32).to_le_bytes());
        b.extend_from_slice(&(self.chars_to_skip as i32).to_le_bytes());
        b.push(self.need_eof as u8);
        Value::big(BigInt::from_py_bytes(&b, false, false))
    }

    fn parse(it: &mut Interp, v: &Value) -> R<Cookie> {
        let n = match v.as_bigint() {
            Some(n) => n,
            None => BigInt::from_i64(it.index_of(v)?),
        };
        let Some(b) = n.to_py_bytes(21, false, false) else {
            return Err(it.overflow_err("int too big to convert"));
        };
        let i32_at = |o: usize| i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) as i64;
        Ok(Cookie {
            start_pos: i64::from_le_bytes(b[..8].try_into().unwrap_or_default()),
            dec_flags: i32_at(8),
            bytes_to_feed: i32_at(12),
            chars_to_skip: i32_at(16),
            need_eof: b[20] != 0,
        })
    }
}

/// Character and line based layer over a BufferedIOBase object, buffer.
///
/// encoding gives the name of the encoding that the stream will be
/// decoded or encoded with. It defaults to locale.getencoding().
///
/// errors determines the strictness of encoding and decoding (see
/// help(codecs.Codec) or the documentation for codecs.register) and
/// defaults to "strict".
///
/// newline controls how line endings are handled. It can be None, '',
/// '\n', '\r', and '\r\n'.  It works as follows:
///
/// * On input, if newline is None, universal newlines mode is
///   enabled. Lines in the input can end in '\n', '\r', or '\r\n', and
///   these are translated into '\n' before being returned to the
///   caller. If it is '', universal newline mode is enabled, but line
///   endings are returned to the caller untranslated. If it has any of
///   the other legal values, input lines are only terminated by the given
///   string, and the line ending is returned to the caller untranslated.
///
/// * On output, if newline is None, any '\n' characters written are
///   translated to the system default line separator, os.linesep. If
///   newline is '' or '\n', no translation takes place. If newline is any
///   of the other legal values, any '\n' characters written are translated
///   to the given string.
///
/// If line_buffering is True, a call to flush is implied when a call to
/// write contains a newline character.
#[lumen_bind::class(module = "_io", name = "TextIOWrapper")]
pub struct TextIOWrapper {
    ok: bool,
    detached: bool,
    buffer: Option<Value>,
    encoding: String,
    errors: String,
    decoder: Option<Dec>,
    encoder: Option<IncrementalEncoder>,
    readuniversal: bool,
    readtranslate: bool,
    readnl: Option<String>,
    writetranslate: bool,
    writenl: Option<String>,
    line_buffering: bool,
    write_through: bool,
    seekable: bool,
    telling: bool,
    has_read1: bool,
    chunk_size: usize,
    decoded: Option<Vec<u32>>,
    used: usize,
    snapshot: Option<(i64, Vec<u8>)>,
    b2cratio: f64,
    pending: Vec<u8>,
    finalizing: bool,
}

impl Drop for TextIOWrapper {
    fn drop(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        if let Some(b) = &self.buffer {
            super::buffered::append_pending(b, &std::mem::take(&mut self.pending));
        }
    }
}

impl TextIOWrapper {
    fn blank() -> TextIOWrapper {
        TextIOWrapper {
            ok: false,
            detached: false,
            buffer: None,
            encoding: String::new(),
            errors: String::new(),
            decoder: None,
            encoder: None,
            readuniversal: false,
            readtranslate: false,
            readnl: None,
            writetranslate: false,
            writenl: None,
            line_buffering: false,
            write_through: false,
            seekable: false,
            telling: false,
            has_read1: false,
            chunk_size: 8192,
            decoded: None,
            used: 0,
            snapshot: None,
            b2cratio: 0.0,
            pending: Vec::new(),
            finalizing: false,
        }
    }

    fn set_newline(&mut self, newline: Option<&str>) {
        self.readuniversal = newline.is_none_or(str::is_empty);
        self.readtranslate = newline.is_none();
        self.readnl = newline.map(str::to_string);
        self.writetranslate = newline != Some("");
        self.writenl = match newline {
            Some(n) if !self.readuniversal && n != "\n" => Some(n.to_string()),
            _ => None,
        };
    }

    fn take_decoded(&mut self, n: i64) -> Vec<u32> {
        let Some(d) = &self.decoded else {
            return Vec::new();
        };
        let avail = d.len() - self.used;
        let n = if n < 0 || n as usize > avail {
            avail
        } else {
            n as usize
        };
        let out = d[self.used..self.used + n].to_vec();
        self.used += n;
        out
    }

    fn set_decoded(&mut self, d: Option<Vec<u32>>) {
        self.decoded = d;
        self.used = 0;
    }
}

fn text_of(cps: &[u32]) -> String {
    let mut s = String::with_capacity(cps.len());
    for &c in cps {
        push_code_point(&mut s, c);
    }
    s
}

type Slf = Py<TextIOWrapper>;

/// CPython's `CHECK_ATTACHED`: the buffer of an initialized, attached wrapper.
fn attached(it: &mut Interp, slf: &Slf) -> R<Value> {
    let (buf, detached) = slf.with(it, |s| {
        (if s.ok { s.buffer.clone() } else { None }, s.detached)
    })?;
    match buf {
        Some(b) => Ok(b),
        None if detached => Err(it.value_error("underlying buffer has been detached")),
        None => Err(it.value_error("I/O operation on uninitialized object")),
    }
}

fn buffer_closed(it: &mut Interp, buffer: &Value) -> R<bool> {
    if let Some(c) = super::buffered::native_closed(it, buffer) {
        return Ok(c);
    }
    super::attr_bool(it, buffer, "closed")
}

/// CPython's `CHECK_ATTACHED` + `CHECK_CLOSED`.
fn open_buffer(it: &mut Interp, slf: &Slf) -> R<Value> {
    let b = attached(it, slf)?;
    let closed = if super::exact::<TextIOWrapper>(it, slf.value()).is_some() {
        buffer_closed(it, &b)?
    } else {
        super::attr_bool(it, slf.value(), "closed")?
    };
    if closed {
        return Err(super::closed_error(it));
    }
    Ok(b)
}

fn with_dec<X>(it: &mut Interp, slf: &Slf, f: impl FnOnce(&mut Interp, &mut Dec) -> R<X>) -> R<X> {
    let Some(mut d) = slf.with(it, |s| s.decoder.take())? else {
        return Err(unsupported(it, "not readable"));
    };
    let r = f(it, &mut d);
    slf.with(it, |s| s.decoder = Some(d))?;
    r
}

fn with_enc<X>(
    it: &mut Interp,
    slf: &Slf,
    f: impl FnOnce(&mut Interp, &mut IncrementalEncoder) -> R<X>,
) -> R<X> {
    let Some(mut e) = slf.with(it, |s| s.encoder.take())? else {
        return Err(unsupported(it, "not writable"));
    };
    let r = f(it, &mut e);
    slf.with(it, |s| s.encoder = Some(e))?;
    r
}

fn has_decoder(it: &mut Interp, slf: &Slf) -> R<bool> {
    slf.with(it, |s| s.decoder.is_some())
}

/// Writes out the encoded text held back for the buffer.
fn writeflush(it: &mut Interp, slf: &Slf) -> R<()> {
    let (buffer, data) = slf.with(it, |s| (s.buffer.clone(), std::mem::take(&mut s.pending)))?;
    let Some(buffer) = buffer else { return Ok(()) };
    if data.is_empty() {
        return Ok(());
    }
    if super::buffered::is_native(it, &buffer) {
        super::buffered::write_bytes(it, &buffer, &data)?;
    } else {
        it.call_method(&buffer, "write", vec![Value::bytes(data)])?;
    }
    Ok(())
}

fn bytes_from(it: &mut Interp, v: &Value, method: &str) -> R<Vec<u8>> {
    match v {
        Value::Obj(o) if matches!(o.kind, Kind::Bytes(_) | Kind::ByteArray(_)) => it.bytes_of(v),
        Value::Obj(o)
            if matches!(o.kind, Kind::Opaque(_))
                && crate::builtins::memview::is_buffer_object(it, v) =>
        {
            it.bytes_of(v)
        }
        _ => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!(
                "underlying {}() should have returned a bytes-like object, not '{}'",
                method, t
            )))
        }
    }
}

fn buffer_read(it: &mut Interp, buffer: &Value, n: i64, one: bool) -> R<Vec<u8>> {
    let method = if one { "read1" } else { "read" };
    let r = if super::buffered::is_native(it, buffer) {
        super::buffered::read_bytes(it, buffer, n, one)?
    } else {
        it.call_method(buffer, method, vec![Value::Int(n)])?
    };
    bytes_from(it, &r, method)
}

/// CPython's `textiowrapper_read_chunk`: false at end of file.
fn read_chunk(it: &mut Interp, slf: &Slf, size_hint: usize) -> R<bool> {
    let (buffer, telling, ratio, chunk, read1) = slf.with(it, |s| {
        (
            s.buffer.clone(),
            s.telling,
            s.b2cratio,
            s.chunk_size,
            s.has_read1,
        )
    })?;
    let Some(buffer) = buffer else {
        return Ok(false);
    };
    if !has_decoder(it, slf)? {
        return Err(unsupported(it, "not readable"));
    }
    let snap = if telling {
        Some(with_dec(it, slf, |it, d| d.getstate(it))?)
    } else {
        None
    };
    let hint = if size_hint > 0 {
        (ratio.max(1.0) * size_hint as f64) as usize
    } else {
        0
    };
    let input = buffer_read(it, &buffer, chunk.max(hint) as i64, read1)?;
    let eof = input.is_empty();
    let decoded = with_dec(it, slf, |it, d| d.decode(it, &input, eof))?;
    let cps: Vec<u32> = code_points(&decoded).collect();
    let nchars = cps.len();
    slf.with(it, |s| {
        s.set_decoded(Some(cps));
        s.b2cratio = if nchars > 0 {
            input.len() as f64 / nchars as f64
        } else {
            0.0
        };
        if let Some((mut buf, flags)) = snap {
            buf.extend_from_slice(&input);
            s.snapshot = Some((flags, buf));
        }
    })?;
    Ok(!eof || nchars > 0)
}

fn read(it: &mut Interp, slf: &Slf, n: i64) -> R<String> {
    let buffer = open_buffer(it, slf)?;
    if !has_decoder(it, slf)? {
        return Err(unsupported(it, "not readable"));
    }
    writeflush(it, slf)?;
    if n < 0 {
        let r = if super::buffered::is_native(it, &buffer) {
            super::buffered::read_bytes(it, &buffer, -1, false)?
        } else {
            it.call_method(&buffer, "read", Vec::new())?
        };
        let input = if r.is_none() {
            Vec::new()
        } else {
            bytes_from(it, &r, "read")?
        };
        let decoded = with_dec(it, slf, |it, d| d.decode(it, &input, true))?;
        let head = slf.with(it, |s| {
            let h = s.take_decoded(-1);
            s.set_decoded(None);
            s.snapshot = None;
            h
        })?;
        let mut out = text_of(&head);
        out.push_str(&decoded);
        return Ok(out);
    }
    let mut out = slf.with(it, |s| s.take_decoded(n))?;
    while (out.len() as i64) < n {
        let remaining = n as usize - out.len();
        if !read_chunk(it, slf, remaining)? {
            break;
        }
        let more = slf.with(it, |s| s.take_decoded(remaining as i64))?;
        out.extend(more);
    }
    Ok(text_of(&out))
}

/// CPython's `_textiowrapper_readline`.
fn readline(it: &mut Interp, slf: &Slf, limit: i64) -> R<String> {
    open_buffer(it, slf)?;
    writeflush(it, slf)?;
    let (translate, universal, readnl) =
        slf.with(it, |s| (s.readtranslate, s.readuniversal, s.readnl.clone()))?;
    let readnl: Vec<u32> = readnl
        .as_deref()
        .map(|s| code_points(s).collect())
        .unwrap_or_default();
    let mut chunks: Vec<u32> = Vec::new();
    let mut remaining: Option<Vec<u32>> = None;
    loop {
        let mut more = true;
        while slf.with(it, |s| s.decoded.as_ref().is_none_or(|d| d.len() <= s.used))? {
            if !read_chunk(it, slf, 0)? {
                more = false;
                break;
            }
        }
        if !more {
            slf.with(it, |s| {
                s.set_decoded(None);
                s.snapshot = None;
            })?;
            if let Some(r) = remaining.take() {
                chunks.extend(r);
            }
            return Ok(text_of(&chunks));
        }
        let (line, start, offset) = slf.with(it, |s| {
            let d = s.decoded.as_ref().map(|d| &d[s.used..]).unwrap_or(&[]);
            match remaining.take() {
                None => (d.to_vec(), 0usize, s.used as i64),
                Some(mut r) => {
                    let off = r.len() as i64;
                    r.extend_from_slice(d);
                    (r, 0usize, s.used as i64 - off)
                }
            }
        })?;
        let found = find_line_ending(translate, universal, &readnl, &line[start..]);
        let budget = |chunked: usize| {
            if limit >= 0 {
                Some((limit as usize).saturating_sub(chunked))
            } else {
                None
            }
        };
        let (endpos, done) = match found {
            Some(e) => {
                let e = match budget(chunks.len()) {
                    Some(b) if e > b => b,
                    _ => e,
                };
                (e, true)
            }
            None => {
                let consumed = if translate || universal || readnl.len() <= 1 {
                    line.len()
                } else {
                    line.len().saturating_sub(readnl.len() - 1)
                };
                match budget(chunks.len()) {
                    Some(b) if consumed >= b => (b, true),
                    _ => (consumed, false),
                }
            }
        };
        if done {
            chunks.extend_from_slice(&line[..endpos]);
            slf.with(it, |s| s.used = (offset + endpos as i64).max(0) as usize)?;
            return Ok(text_of(&chunks));
        }
        chunks.extend_from_slice(&line[..endpos]);
        if endpos < line.len() {
            remaining = Some(line[endpos..].to_vec());
        }
        slf.with(it, |s| s.set_decoded(None))?;
    }
}

fn write(it: &mut Interp, slf: &Slf, text: &str) -> R<usize> {
    open_buffer(it, slf)?;
    if !slf.with(it, |s| s.encoder.is_some())? {
        return Err(unsupported(it, "not writable"));
    }
    let n = lumen_common::smuggle::count_code_points(text);
    let (writetranslate, writenl, line_buffering, write_through, chunk) = slf.with(it, |s| {
        (
            s.writetranslate,
            s.writenl.clone(),
            s.line_buffering,
            s.write_through,
            s.chunk_size,
        )
    })?;
    let haslf = ((writetranslate && writenl.is_some()) || line_buffering) && text.contains('\n');
    let text = match &writenl {
        Some(nl) if haslf && writetranslate => std::borrow::Cow::Owned(text.replace('\n', nl)),
        _ => std::borrow::Cow::Borrowed(text),
    };
    let needflush = line_buffering && (haslf || text.contains('\r'));
    let bytes = with_enc(it, slf, |it, e| e.encode(it, &text, false))?;
    let full = slf.with(it, |s| {
        s.pending.extend_from_slice(&bytes);
        s.pending.len() >= chunk
    })?;
    if full || needflush || write_through {
        writeflush(it, slf)?;
    }
    if needflush {
        let b = attached(it, slf)?;
        it.call_method(&b, "flush", Vec::new())?;
    }
    slf.with(it, |s| {
        s.set_decoded(None);
        s.snapshot = None;
    })?;
    if has_decoder(it, slf)? {
        with_dec(it, slf, |it, d| d.reset(it))?;
    }
    Ok(n)
}

fn decoder_setstate(it: &mut Interp, slf: &Slf, c: &Cookie) -> R<()> {
    with_dec(it, slf, |it, d| {
        if c.start_pos == 0 && c.dec_flags == 0 {
            d.reset(it)
        } else {
            d.setstate(it, Vec::new(), c.dec_flags)
        }
    })
}

fn encoder_reset(it: &mut Interp, slf: &Slf, start_of_stream: bool) -> R<()> {
    if !slf.with(it, |s| s.encoder.is_some())? {
        return Ok(());
    }
    with_enc(it, slf, |it, e| {
        if start_of_stream {
            e.reset(it)
        } else {
            e.setstate(it, 0)
        }
    })
}

fn tell(it: &mut Interp, slf: &Slf) -> R<Value> {
    let buffer = open_buffer(it, slf)?;
    let (seekable, telling) = slf.with(it, |s| (s.seekable, s.telling))?;
    if !seekable {
        return Err(unsupported(it, "underlying stream is not seekable"));
    }
    if !telling {
        return Err(it.new_exc_str("OSError", "telling position disabled by next() call"));
    }
    writeflush(it, slf)?;
    it.call_method(slf.value(), "flush", Vec::new())?;
    let posobj = it.call_method(&buffer, "tell", Vec::new())?;
    let snapshot = slf.with(it, |s| {
        if s.decoder.is_some() {
            s.snapshot.clone()
        } else {
            None
        }
    })?;
    let Some((dec_flags, next_input)) = snapshot else {
        return Ok(posobj);
    };
    let position = it.index_of(&posobj)?;
    let mut cookie = Cookie {
        start_pos: position - next_input.len() as i64,
        dec_flags,
        ..Default::default()
    };
    let (used, ratio) = slf.with(it, |s| (s.used, s.b2cratio))?;
    if used == 0 {
        return Ok(cookie.build());
    }
    let mut chars_to_skip = used as i64;
    let saved = with_dec(it, slf, |it, d| d.getstate(it))?;
    let r = (|| -> R<Value> {
        let mut skip_bytes = (ratio * chars_to_skip as f64) as i64;
        let mut skip_back = 1i64;
        while skip_bytes > 0 {
            decoder_setstate(it, slf, &cookie)?;
            let input = &next_input[..(skip_bytes as usize).min(next_input.len())];
            let n = with_dec(it, slf, |it, d| d.decode(it, input, false))?;
            let n = lumen_common::smuggle::count_code_points(&n) as i64;
            if n <= chars_to_skip {
                let (b, flags) = with_dec(it, slf, |it, d| d.getstate(it))?;
                if b.is_empty() {
                    cookie.dec_flags = flags;
                    chars_to_skip -= n;
                    break;
                }
                skip_bytes -= b.len() as i64;
                skip_back = 1;
            } else {
                skip_bytes -= skip_back;
                skip_back *= 2;
            }
        }
        if skip_bytes <= 0 {
            skip_bytes = 0;
            decoder_setstate(it, slf, &cookie)?;
        }
        cookie.start_pos += skip_bytes;
        cookie.chars_to_skip = chars_to_skip;
        if chars_to_skip == 0 {
            return Ok(cookie.build());
        }
        let mut chars_decoded = 0i64;
        let mut i = skip_bytes as usize;
        while i < next_input.len() {
            let byte = [next_input[i]];
            let n = with_dec(it, slf, |it, d| d.decode(it, &byte, false))?;
            chars_decoded += lumen_common::smuggle::count_code_points(&n) as i64;
            cookie.bytes_to_feed += 1;
            let (b, flags) = with_dec(it, slf, |it, d| d.getstate(it))?;
            if b.is_empty() && chars_decoded <= chars_to_skip {
                cookie.start_pos += cookie.bytes_to_feed;
                chars_to_skip -= chars_decoded;
                cookie.dec_flags = flags;
                cookie.bytes_to_feed = 0;
                chars_decoded = 0;
            }
            if chars_decoded >= chars_to_skip {
                break;
            }
            i += 1;
        }
        if i >= next_input.len() {
            let n = with_dec(it, slf, |it, d| d.decode(it, &[], true))?;
            chars_decoded += lumen_common::smuggle::count_code_points(&n) as i64;
            cookie.need_eof = true;
            if chars_decoded < chars_to_skip {
                return Err(it.new_exc_str("OSError", "can't reconstruct logical file position"));
            }
        }
        cookie.chars_to_skip = chars_to_skip;
        Ok(cookie.build())
    })();
    let restore = with_dec(it, slf, |it, d| d.setstate(it, saved.0, saved.1));
    let v = r?;
    restore?;
    Ok(v)
}

fn seek(it: &mut Interp, slf: &Slf, cookie_obj: &Value, whence: i32) -> R<Value> {
    let buffer = open_buffer(it, slf)?;
    if !slf.with(it, |s| s.seekable)? {
        return Err(unsupported(it, "underlying stream is not seekable"));
    }
    let is_zero = |it: &mut Interp, v: &Value| -> R<bool> { it.values_eq(v, &Value::Int(0)) };
    let mut cookie_obj = cookie_obj.clone();
    match whence {
        1 => {
            if !is_zero(it, &cookie_obj)? {
                return Err(unsupported(it, "can't do nonzero cur-relative seeks"));
            }
            cookie_obj = it.call_method(slf.value(), "tell", Vec::new())?;
        }
        2 => {
            if !is_zero(it, &cookie_obj)? {
                return Err(unsupported(it, "can't do nonzero end-relative seeks"));
            }
            it.call_method(slf.value(), "flush", Vec::new())?;
            slf.with(it, |s| {
                s.set_decoded(None);
                s.snapshot = None;
            })?;
            if has_decoder(it, slf)? {
                with_dec(it, slf, |it, d| d.reset(it))?;
            }
            let res = it.call_method(&buffer, "seek", vec![Value::Int(0), Value::Int(2)])?;
            let start = is_zero(it, &res)?;
            encoder_reset(it, slf, start)?;
            return Ok(res);
        }
        0 => {}
        _ => {
            return Err(
                it.value_error(&format!("invalid whence ({}, should be 0, 1 or 2)", whence))
            );
        }
    }
    let lt = it.compare_op(crate::ast::CmpOp::Lt, &cookie_obj, &Value::Int(0))?;
    if it.truthy(&lt)? {
        let r = it.repr_of(&cookie_obj)?;
        return Err(it.value_error(&format!("negative seek position {}", r)));
    }
    it.call_method(slf.value(), "flush", Vec::new())?;
    let cookie = Cookie::parse(it, &cookie_obj)?;
    it.call_method(&buffer, "seek", vec![Value::Int(cookie.start_pos)])?;
    slf.with(it, |s| {
        s.set_decoded(None);
        s.snapshot = None;
    })?;
    if has_decoder(it, slf)? {
        decoder_setstate(it, slf, &cookie)?;
    }
    if cookie.chars_to_skip != 0 {
        let r = it.call_method(&buffer, "read", vec![Value::Int(cookie.bytes_to_feed)])?;
        if !matches!(&r, Value::Obj(o) if matches!(o.kind, Kind::Bytes(_))) {
            let t = it.type_name_of(&r);
            return Err(it.type_error(&format!(
                "underlying read() should have returned a bytes object, not '{}'",
                t
            )));
        }
        let input = it.bytes_of(&r)?;
        let decoded = with_dec(it, slf, |it, d| d.decode(it, &input, cookie.need_eof))?;
        let cps: Vec<u32> = code_points(&decoded).collect();
        if (cps.len() as i64) < cookie.chars_to_skip {
            return Err(it.new_exc_str("OSError", "can't restore logical file position"));
        }
        slf.with(it, |s| {
            s.snapshot = Some((cookie.dec_flags, input));
            s.set_decoded(Some(cps));
            s.used = cookie.chars_to_skip as usize;
        })?;
    } else {
        slf.with(it, |s| s.snapshot = Some((cookie.dec_flags, Vec::new())))?;
    }
    encoder_reset(it, slf, cookie.start_pos == 0 && cookie.dec_flags == 0)?;
    Ok(cookie_obj)
}

fn make_codecs(it: &mut Interp, slf: &Slf, buffer: &Value) -> R<()> {
    let (encoding, errors, universal, translate) = slf.with(it, |s| {
        (
            s.encoding.clone(),
            s.errors.clone(),
            s.readuniversal,
            s.readtranslate,
        )
    })?;
    let norm = crate::codecs::normalize_encoding(&encoding);
    if crate::codecs::native_codec(&norm).is_none() {
        crate::codecs::lookup_text(it, &encoding, "codecs.open()")?;
    }
    let decoder = if super::call_bool(it, buffer, "readable")? {
        let codec = IncrementalDecoder::new(it, &encoding, &errors)?;
        Some(Dec {
            codec,
            nl: universal.then(|| NlState::new(translate)),
        })
    } else {
        None
    };
    let encoder = if super::call_bool(it, buffer, "writable")? {
        Some(IncrementalEncoder::new(it, &encoding, &errors)?)
    } else {
        None
    };
    slf.with(it, |s| {
        s.decoder = decoder;
        s.encoder = encoder;
    })
}

/// CPython's `_textiowrapper_fix_encoder_state`: no BOM in the middle of a file.
fn fix_encoder_state(it: &mut Interp, slf: &Slf, buffer: &Value) -> R<()> {
    let (seekable, has_enc) = slf.with(it, |s| (s.seekable, s.encoder.is_some()))?;
    if !seekable || !has_enc {
        return Ok(());
    }
    let pos = it.call_method(buffer, "tell", Vec::new())?;
    if !it.values_eq(&pos, &Value::Int(0))? {
        with_enc(it, slf, |it, e| e.setstate(it, 0))?;
    }
    Ok(())
}

fn check_newline(it: &mut Interp, newline: Option<&str>) -> R<()> {
    match newline {
        None | Some("" | "\n" | "\r" | "\r\n") => Ok(()),
        Some(n) => Err(it.value_error(&format!("illegal newline value: {}", n))),
    }
}

#[allow(clippy::too_many_arguments)]
fn init(
    it: &mut Interp,
    slf: &Slf,
    buffer: &Value,
    encoding: Option<&str>,
    errors: Option<&str>,
    newline: Option<&str>,
    line_buffering: bool,
    write_through: bool,
) -> R<()> {
    slf.with(it, |s| {
        s.ok = false;
        s.detached = false;
    })?;
    check_newline(it, newline)?;
    let encoding = match encoding {
        None | Some("locale") => super::locale_encoding(it).to_string(),
        Some(e) => e.to_string(),
    };
    let errors = errors.unwrap_or("strict").to_string();
    let mut st = TextIOWrapper::blank();
    st.encoding = encoding;
    st.errors = errors;
    st.set_newline(newline);
    st.line_buffering = line_buffering;
    st.write_through = write_through;
    st.buffer = Some(buffer.clone());
    *slf.borrow_mut(it)? = st;
    make_codecs(it, slf, buffer)?;
    let seekable = super::call_bool(it, buffer, "seekable")?;
    let has_read1 = getattr_opt(it, buffer, "read1")?.is_some();
    slf.with(it, |s| {
        s.seekable = seekable;
        s.telling = seekable;
        s.has_read1 = has_read1;
    })?;
    fix_encoder_state(it, slf, buffer)?;
    slf.with(it, |s| s.ok = true)
}

/// A new `TextIOWrapper` of exactly the native class.
pub fn new_textio(
    it: &mut Interp,
    buffer: Value,
    encoding: Option<&str>,
    errors: Option<&str>,
    newline: Option<&str>,
    line_buffering: bool,
    write_through: bool,
) -> R<Value> {
    let py = Py::new(it, TextIOWrapper::blank());
    init(
        it,
        &py,
        &buffer,
        encoding,
        errors,
        newline,
        line_buffering,
        write_through,
    )?;
    Ok(py.into_value())
}

pub fn is_native_textio(it: &mut Interp, v: &Value) -> bool {
    super::exact::<TextIOWrapper>(it, v).is_some()
}

/// `file.write(s)` for an exact native `TextIOWrapper` (the `print()` path); `None` for any
/// other object.
pub fn write_native(it: &mut Interp, file: &Value, s: &str) -> Option<R<usize>> {
    let slf = super::exact::<TextIOWrapper>(it, file)?;
    Some(write(it, &slf, s))
}

fn opt_str<'a>(it: &mut Interp, v: Option<&'a Value>, func: &str, arg: &str) -> R<Option<&'a str>> {
    match v {
        None | Some(Value::None) => Ok(None),
        Some(v) => match v.as_str() {
            Some(s) => Ok(Some(s)),
            None => {
                let t = it.type_name_of(v);
                Err(it.type_error(&format!(
                    "{}() argument '{}' must be str or None, not {}",
                    func, arg, t
                )))
            }
        },
    }
}

#[lumen_bind::methods]
impl TextIOWrapper {
    #[constructor(hint(py(
        text_signature = "(buffer, encoding=None, errors=None, newline=None,\n              line_buffering=False, write_through=False)"
    )))]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> TextIOWrapper {
        let _ = (args, kwargs);
        TextIOWrapper::blank()
    }

    #[proto(init)]
    #[allow(clippy::too_many_arguments)]
    fn __init__(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] buffer: &Value,
        #[kw] encoding: Option<&Value>,
        #[kw] errors: Option<&Value>,
        #[kw] newline: Option<&Value>,
        #[kw]
        #[default(false)]
        line_buffering: bool,
        #[kw]
        #[default(false)]
        write_through: bool,
    ) -> R<()> {
        let encoding = opt_str(it, encoding, "TextIOWrapper", "encoding")?;
        let errors = opt_str(it, errors, "TextIOWrapper", "errors")?;
        let newline = opt_str(it, newline, "TextIOWrapper", "newline")?;
        init(
            it,
            &slf.0,
            buffer,
            encoding,
            errors,
            newline,
            line_buffering,
            write_through,
        )
    }

    /// Write string s to stream.
    fn write(slf: This<Py<Self>>, it: &mut Interp, text: &Value) -> R<usize> {
        let Some(s) = text.as_str() else {
            let t = it.type_name_of(text);
            return Err(it.type_error(&format!("write() argument must be str, not {}", t)));
        };
        write(it, &slf.0, s)
    }

    /// Read at most size characters from stream.
    #[method(hint(py(text_signature = "($self, size=-1, /)")))]
    fn read(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<String> {
        attached(it, &slf.0)?;
        let n = super::size_arg(it, size)?;
        read(it, &slf.0, n)
    }

    /// Read until newline or EOF.
    #[method(hint(py(text_signature = "($self, size=-1, /)")))]
    fn readline(slf: This<Py<Self>>, it: &mut Interp, size: Option<&Value>) -> R<String> {
        attached(it, &slf.0)?;
        let n = match size {
            None | Some(Value::None) => -1,
            Some(v) => it.index_of(v)?,
        };
        readline(it, &slf.0, n)
    }

    /// Flush write buffers, if applicable.
    fn flush(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let buffer = open_buffer(it, &slf.0)?;
        slf.0.with(it, |s| s.telling = s.seekable)?;
        writeflush(it, &slf.0)?;
        it.call_method(&buffer, "flush", Vec::new())
    }

    /// Flush and close the IO object.
    fn close(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let buffer = attached(it, &slf.0)?;
        if buffer_closed(it, &buffer)? {
            return Ok(Value::None);
        }
        if slf.0.with(it, |s| s.finalizing)? {
            super::dealloc_warn(it, &buffer, slf.0.value());
        }
        let r = it.call_method(slf.0.value(), "flush", Vec::new());
        let c = it.call_method(&buffer, "close", Vec::new());
        match (r, c) {
            (Err(e), Err(e2)) => {
                chain(&e2, &e);
                Err(e2)
            }
            (Err(e), Ok(_)) => Err(e),
            (Ok(_), c) => c,
        }
    }

    /// Return the stream position as an opaque number.
    ///
    /// The return value of tell() can be given as input to seek(), to restore a
    /// previous stream position.
    fn tell(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        tell(it, &slf.0)
    }

    /// Set the stream position, and return the new stream position.
    ///
    ///   cookie
    ///     Zero or an opaque number returned by tell().
    ///   whence
    ///     The relative position to seek from.
    ///
    /// Four operations are supported, given by the following argument
    /// combinations:
    ///
    /// - seek(0, SEEK_SET): Rewind to the start of the stream.
    /// - seek(cookie, SEEK_SET): Restore a previous position;
    ///   'cookie' must be a number returned by tell().
    /// - seek(0, SEEK_END): Fast-forward to the end of the stream.
    /// - seek(0, SEEK_CUR): Leave the current stream position unchanged.
    ///
    /// Any other argument combinations are invalid,
    /// and may raise exceptions.
    #[method(hint(py(text_signature = "($self, cookie, whence=os.SEEK_SET, /)")))]
    fn seek(
        slf: This<Py<Self>>,
        it: &mut Interp,
        cookie: &Value,
        #[default(0)] whence: i32,
    ) -> R<Value> {
        seek(it, &slf.0, cookie, whence)
    }

    fn truncate(slf: This<Py<Self>>, it: &mut Interp, pos: Option<&Value>) -> R<Value> {
        let buffer = attached(it, &slf.0)?;
        it.call_method(slf.0.value(), "flush", Vec::new())?;
        it.call_method(
            &buffer,
            "truncate",
            vec![pos.cloned().unwrap_or(Value::None)],
        )
    }

    /// Separate the underlying buffer from the TextIOBase and return it.
    fn detach(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let buffer = attached(it, &slf.0)?;
        it.call_method(slf.0.value(), "flush", Vec::new())?;
        slf.0.with(it, |s| {
            s.buffer = None;
            s.detached = true;
            s.ok = false;
        })?;
        Ok(buffer)
    }

    /// Reconfigure the text stream with new parameters.
    ///
    /// This also does an implicit stream flush.
    #[allow(clippy::too_many_arguments)]
    fn reconfigure(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kwonly] encoding: Option<&Value>,
        #[kwonly] errors: Option<&Value>,
        #[kwonly] newline: Option<&Value>,
        #[kwonly] line_buffering: Option<&Value>,
        #[kwonly] write_through: Option<&Value>,
    ) -> R<()> {
        let buffer = attached(it, &slf.0)?;
        let enc = opt_str(it, encoding, "reconfigure", "encoding")?;
        let errs = opt_str(it, errors, "reconfigure", "errors")?;
        let newline_given = newline.is_some();
        let nl = opt_str(it, newline, "reconfigure", "newline")?;
        if slf.0.with(it, |s| s.decoded.is_some())?
            && (enc.is_some() || errs.is_some() || newline_given)
        {
            return Err(unsupported(
                it,
                "It is not possible to set the encoding or newline of stream after the first read",
            ));
        }
        if newline_given {
            check_newline(it, nl)?;
        }
        let lb = match line_buffering {
            None | Some(Value::None) => None,
            Some(v) => Some(it.truthy(v)?),
        };
        let wt = match write_through {
            None | Some(Value::None) => None,
            Some(v) => Some(it.truthy(v)?),
        };
        it.call_method(slf.0.value(), "flush", Vec::new())?;
        slf.0.with(it, |s| {
            s.b2cratio = 0.0;
            if newline_given {
                s.set_newline(nl);
            }
        })?;
        if enc.is_some() || errs.is_some() || newline_given {
            let (cur_enc, cur_err) = slf.0.with(it, |s| (s.encoding.clone(), s.errors.clone()))?;
            let (new_enc, new_err) = match enc {
                None => (cur_enc, errs.map_or(cur_err, str::to_string)),
                Some("locale") => (
                    super::locale_encoding(it).to_string(),
                    errs.unwrap_or("strict").to_string(),
                ),
                Some(e) => (e.to_string(), errs.unwrap_or("strict").to_string()),
            };
            let old = slf.0.with(it, |s| {
                (
                    std::mem::replace(&mut s.encoding, new_enc),
                    std::mem::replace(&mut s.errors, new_err),
                )
            })?;
            if let Err(e) = make_codecs(it, &slf.0, &buffer) {
                slf.0.with(it, |s| (s.encoding, s.errors) = old)?;
                return Err(e);
            }
            fix_encoder_state(it, &slf.0, &buffer)?;
        }
        slf.0.with(it, |s| {
            if let Some(lb) = lb {
                s.line_buffering = lb;
            }
            if let Some(wt) = wt {
                s.write_through = wt;
            }
        })
    }

    fn fileno(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let b = attached(it, &slf.0)?;
        it.call_method(&b, "fileno", Vec::new())
    }

    fn seekable(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let b = attached(it, &slf.0)?;
        it.call_method(&b, "seekable", Vec::new())
    }

    fn readable(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let b = attached(it, &slf.0)?;
        it.call_method(&b, "readable", Vec::new())
    }

    fn writable(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let b = attached(it, &slf.0)?;
        it.call_method(&b, "writable", Vec::new())
    }

    fn isatty(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let b = attached(it, &slf.0)?;
        it.call_method(&b, "isatty", Vec::new())
    }

    #[getter]
    fn closed(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let b = attached(it, &slf.0)?;
        it.get_attr_str(&b, "closed")
    }

    #[getter]
    fn name(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let b = attached(it, &slf.0)?;
        it.get_attr_str(&b, "name")
    }

    #[getter]
    fn buffer(&self) -> Value {
        self.buffer.clone().unwrap_or(Value::None)
    }

    #[getter]
    fn encoding(&self) -> Value {
        if self.ok || self.detached {
            Value::str(&self.encoding)
        } else {
            Value::None
        }
    }

    #[getter]
    fn errors(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        attached(it, &slf.0)?;
        slf.0.with(it, |s| s.errors.clone())
    }

    #[getter]
    fn line_buffering(&self) -> bool {
        self.line_buffering
    }

    #[getter]
    fn write_through(&self) -> bool {
        self.write_through
    }

    #[getter]
    fn newlines(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        attached(it, &slf.0)?;
        slf.0.with(it, |s| {
            s.decoder
                .as_ref()
                .and_then(|d| d.nl.as_ref())
                .map_or(Value::None, NlState::newlines)
        })
    }

    #[getter(name = "_CHUNK_SIZE")]
    fn chunk_size(slf: This<Py<Self>>, it: &mut Interp) -> R<usize> {
        attached(it, &slf.0)?;
        slf.0.with(it, |s| s.chunk_size)
    }

    #[setter(name = "_CHUNK_SIZE")]
    fn set_chunk_size(slf: This<Py<Self>>, it: &mut Interp, v: &Value) -> R<()> {
        attached(it, &slf.0)?;
        let n = it.index_of(v)?;
        if n <= 0 {
            return Err(it.value_error("a strictly positive integer is required"));
        }
        slf.0.with(it, |s| s.chunk_size = n as usize)
    }

    #[getter]
    fn _finalizing(&self) -> bool {
        self.finalizing
    }

    #[setter(name = "_finalizing")]
    fn set_finalizing(slf: This<Py<Self>>, it: &mut Interp, v: bool) -> R<()> {
        slf.0.with(it, |s| s.finalizing = v)
    }

    #[proto(next)]
    fn __next__(slf: This<Py<Self>>, it: &mut Interp) -> R<Option<Value>> {
        attached(it, &slf.0)?;
        slf.0.with(it, |s| s.telling = false)?;
        let line = if super::exact::<TextIOWrapper>(it, slf.0.value()).is_some() {
            Value::string(readline(it, &slf.0, -1)?)
        } else {
            let l = it.call_method(slf.0.value(), "readline", Vec::new())?;
            if l.as_str().is_none() {
                let t = it.type_name_of(&l);
                return Err(it.new_exc_str(
                    "OSError",
                    &format!("readline() should have returned a str object, not '{}'", t),
                ));
            }
            l
        };
        if line.as_str() == Some("") {
            slf.0.with(it, |s| {
                s.snapshot = None;
                s.telling = s.seekable;
            })?;
            return Ok(None);
        }
        Ok(Some(line))
    }

    #[proto(repr)]
    fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        let tn = it.tp_name_of(slf.0.value());
        let mut out = format!("<{}", tn);
        match it.get_attr_str(slf.0.value(), "name") {
            Ok(n) => {
                let r = it.repr_of(&n)?;
                out.push_str(&format!(" name={}", r));
            }
            Err(e) if it.exc_is(&e, "AttributeError") || it.exc_is(&e, "ValueError") => {}
            Err(e) => return Err(e),
        }
        if let Some(m) = getattr_opt(it, slf.0.value(), "mode")? {
            let r = it.repr_of(&m)?;
            out.push_str(&format!(" mode={}", r));
        }
        let enc = slf.0.with(it, |s| s.encoding.clone())?;
        let r = it.repr_of(&Value::string(enc))?;
        out.push_str(&format!(" encoding={}>", r));
        Ok(out)
    }
}

/// Codec used when reading a file in universal newlines mode.
///
/// It wraps another incremental decoder, translating \r\n and \r into \n.
/// It also records the types of newlines encountered.  When used with
/// translate=False, it ensures that the newline sequence is returned in
/// one piece. When used with decoder=None, it expects unicode strings as
/// decode input and translates newlines without first invoking an external
/// decoder.
#[lumen_bind::class(module = "_io", name = "IncrementalNewlineDecoder")]
pub struct IncrementalNewlineDecoder {
    decoder: Option<Value>,
    nl: NlState,
    errors: Value,
}

fn nld_decoder(it: &mut Interp, slf: &Py<IncrementalNewlineDecoder>) -> R<Value> {
    match slf.with(it, |s| s.decoder.clone())? {
        Some(d) => Ok(d),
        None => Err(it.value_error("IncrementalNewlineDecoder.__init__() not called")),
    }
}

#[lumen_bind::methods]
impl IncrementalNewlineDecoder {
    #[constructor(hint(py(text_signature = "(decoder, translate, errors='strict')")))]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> IncrementalNewlineDecoder {
        let _ = (args, kwargs);
        IncrementalNewlineDecoder {
            decoder: None,
            nl: NlState::new(false),
            errors: Value::None,
        }
    }

    #[proto(init)]
    fn __init__(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] decoder: &Value,
        #[kw] translate: bool,
        #[kw]
        #[default("strict")]
        errors: Value,
    ) -> R<()> {
        slf.0.with(it, |s| {
            s.decoder = Some(decoder.clone());
            s.nl = NlState::new(translate);
            s.errors = errors;
        })
    }

    fn decode(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] input: &Value,
        #[kw]
        #[default(false)]
        r#final: bool,
    ) -> R<String> {
        let d = nld_decoder(it, &slf.0)?;
        let out = if d.is_none() {
            input.clone()
        } else {
            let f = it.get_attr_str(&d, "decode")?;
            let k = it.str_obj("final");
            it.call(&f, vec![input.clone()], vec![(k, Value::Bool(r#final))])?
        };
        let Some(s) = out.as_str() else {
            let t = it.type_name_of(&out);
            return Err(it.type_error(&format!(
                "decoder should return a string result, not '{}'",
                t
            )));
        };
        let s = s.to_string();
        slf.0.with(it, |st| st.nl.feed(s, r#final))
    }

    fn getstate(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let d = nld_decoder(it, &slf.0)?;
        let (buf, flag) = if d.is_none() {
            (Value::bytes(Vec::new()), 0i64)
        } else {
            let st = it.call_method(&d, "getstate", Vec::new())?;
            match st.tuple_items() {
                Some([b, f]) if f.is_int_like() => (b.clone(), it.index_of(f)?),
                _ => return Err(it.type_error("illegal decoder state")),
            }
        };
        let pend = slf.0.with(it, |s| s.nl.pendingcr)?;
        Ok(Value::tuple(vec![
            buf,
            Value::Int((flag << 1) | pend as i64),
        ]))
    }

    fn setstate(slf: This<Py<Self>>, it: &mut Interp, state: &Value) -> R<()> {
        let d = nld_decoder(it, &slf.0)?;
        let (buf, flag) = match state.tuple_items() {
            Some([b, f]) if f.is_int_like() => (b.clone(), it.index_of(f)?),
            _ => return Err(it.type_error("state argument must be a tuple")),
        };
        slf.0.with(it, |s| s.nl.pendingcr = flag & 1 != 0)?;
        if !d.is_none() {
            it.call_method(
                &d,
                "setstate",
                vec![Value::tuple(vec![buf, Value::Int(flag >> 1)])],
            )?;
        }
        Ok(())
    }

    fn reset(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
        let d = nld_decoder(it, &slf.0)?;
        slf.0.with(it, |s| s.nl.reset())?;
        if !d.is_none() {
            it.call_method(&d, "reset", Vec::new())?;
        }
        Ok(())
    }

    #[getter]
    fn newlines(&self) -> Value {
        self.nl.newlines()
    }
}
