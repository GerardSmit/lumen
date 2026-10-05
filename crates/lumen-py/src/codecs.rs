//! Text codecs: the codec registry and error handlers behind `_codecs`, the native UTF-8/16/32,
//! Latin-1, ASCII, charmap, UTF-7 and (raw-)unicode-escape codecs, and the Rust API used by
//! `str.encode`, `bytes.decode` and text I/O ([`encode`], [`decode`], [`IncrementalDecoder`],
//! [`IncrementalEncoder`]).
//!
//! A Python `str` is a Rust `str` whose lone surrogates are smuggled (`lumen_common::smuggle`);
//! every position here counts code points, as CPython's do.

use crate::object::*;
use crate::vm::*;
use lumen_common::smuggle::{code_points, escape_text, may_contain, push_code_point, Spelling};
use lumen_common::utf;
use std::borrow::Cow;
use std::collections::HashMap;

/// Interpreter-wide codec registry: search functions, the lookup cache and error handlers.
#[derive(Default)]
pub struct CodecState {
    search_path: Vec<Value>,
    cache: HashMap<String, Value>,
    errors: HashMap<String, Value>,
    builtin_errors_ready: bool,
    encodings_imported: bool,
}

/// CPython's `_Py_normalize_encoding`: lower-case ASCII alphanumerics and dots, every other run
/// of characters collapsed to one `_` (dropped at either end).
pub fn normalize_encoding(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut punct = false;
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '.' {
            if punct && !out.is_empty() {
                out.push('_');
            }
            punct = false;
            out.push(c.to_ascii_lowercase());
        } else {
            punct = true;
        }
    }
    out
}

/// The codecs implemented natively; the byte order is 0 (BOM, native little-endian), -1 (little)
/// or 1 (big), as in `_codecs.utf_16_encode`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Native {
    Utf8,
    Utf8Sig,
    Utf16(i32),
    Utf32(i32),
    Latin1,
    Ascii,
}

/// The native codec for a normalized encoding name (CPython's fast paths plus the aliases
/// `encodings.aliases` maps to the same codecs).
pub fn native_codec(norm: &str) -> Option<Native> {
    Some(match norm {
        "utf_8" | "utf8" | "u8" | "utf" | "utf8_ucs2" | "utf8_ucs4" | "cp65001" => Native::Utf8,
        "utf_8_sig" => Native::Utf8Sig,
        "utf_16" | "utf16" | "u16" => Native::Utf16(0),
        "utf_16_le" | "utf_16le" | "unicodelittleunmarked" => Native::Utf16(-1),
        "utf_16_be" | "utf_16be" | "unicodebigunmarked" => Native::Utf16(1),
        "utf_32" | "utf32" | "u32" => Native::Utf32(0),
        "utf_32_le" | "utf_32le" => Native::Utf32(-1),
        "utf_32_be" | "utf_32be" => Native::Utf32(1),
        "latin_1" | "latin1" | "iso_8859_1" | "iso8859_1" | "8859" | "cp819" | "latin" | "l1"
        | "iso_ir_100" | "csisolatin1" | "ibm819" | "iso8859" | "iso_8859_1_1987" => Native::Latin1,
        "ascii" | "us_ascii" | "646" | "us" | "cp367" | "csascii" | "ibm367" | "iso646_us"
        | "iso_ir_6" | "ansi_x3.4_1968" | "ansi_x3_4_1968" | "ansi_x3.4_1986"
        | "iso_646.irv_1991" => Native::Ascii,
        _ => return None,
    })
}

// ---- error handlers ------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorMode {
    Strict,
    Ignore,
    Replace,
    BackslashReplace,
    XmlCharRefReplace,
    NameReplace,
    SurrogateEscape,
    SurrogatePass,
    Other(String),
}

impl ErrorMode {
    pub fn parse(name: &str) -> ErrorMode {
        match name {
            "strict" => ErrorMode::Strict,
            "ignore" => ErrorMode::Ignore,
            "replace" => ErrorMode::Replace,
            "backslashreplace" => ErrorMode::BackslashReplace,
            "xmlcharrefreplace" => ErrorMode::XmlCharRefReplace,
            "namereplace" => ErrorMode::NameReplace,
            "surrogateescape" => ErrorMode::SurrogateEscape,
            "surrogatepass" => ErrorMode::SurrogatePass,
            other => ErrorMode::Other(other.to_string()),
        }
    }
}

/// How a UTF codec writes a lone surrogate under `surrogatepass`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SurrogateForm {
    Utf8,
    Utf16(bool),
    Utf32(bool),
}

impl SurrogateForm {
    fn len(self) -> usize {
        match self {
            SurrogateForm::Utf8 => 3,
            SurrogateForm::Utf16(_) => 2,
            SurrogateForm::Utf32(_) => 4,
        }
    }

    fn put(self, out: &mut Vec<u8>, cp: u32) {
        match self {
            SurrogateForm::Utf8 => out.extend([
                0xe0 | (cp >> 12) as u8,
                0x80 | ((cp >> 6) & 0x3f) as u8,
                0x80 | (cp & 0x3f) as u8,
            ]),
            SurrogateForm::Utf16(be) => out.extend(if be {
                (cp as u16).to_be_bytes()
            } else {
                (cp as u16).to_le_bytes()
            }),
            SurrogateForm::Utf32(be) => out.extend(if be {
                cp.to_be_bytes()
            } else {
                cp.to_le_bytes()
            }),
        }
    }

    fn read(self, p: &[u8]) -> Option<u32> {
        let cp = match self {
            SurrogateForm::Utf8 => {
                if p.len() < 3 || p[0] & 0xf0 != 0xe0 || p[1] & 0xc0 != 0x80 || p[2] & 0xc0 != 0x80
                {
                    return None;
                }
                ((p[0] as u32 & 0x0f) << 12) | ((p[1] as u32 & 0x3f) << 6) | (p[2] as u32 & 0x3f)
            }
            SurrogateForm::Utf16(be) => {
                let b: [u8; 2] = p.get(..2)?.try_into().ok()?;
                (if be {
                    u16::from_be_bytes(b)
                } else {
                    u16::from_le_bytes(b)
                }) as u32
            }
            SurrogateForm::Utf32(be) => {
                let b: [u8; 4] = p.get(..4)?.try_into().ok()?;
                if be {
                    u32::from_be_bytes(b)
                } else {
                    u32::from_le_bytes(b)
                }
            }
        };
        is_surrogate(cp).then_some(cp)
    }

    /// CPython's `get_standard_encoding`, for the `surrogatepass` handler called from Python.
    fn from_name(name: &str) -> Option<SurrogateForm> {
        let lower = name.to_ascii_lowercase();
        if name == "CP_UTF8" {
            return Some(SurrogateForm::Utf8);
        }
        let rest = lower.strip_prefix("utf")?;
        let rest = rest.strip_prefix(['-', '_']).unwrap_or(rest);
        if rest == "8" {
            return Some(SurrogateForm::Utf8);
        }
        let (wide, rest) = if let Some(r) = rest.strip_prefix("16") {
            (false, r)
        } else {
            (true, rest.strip_prefix("32")?)
        };
        let be = if rest.is_empty() {
            false
        } else {
            match rest.strip_prefix(['-', '_']).unwrap_or(rest) {
                "be" => true,
                "le" => false,
                _ => return None,
            }
        };
        Some(if wide {
            SurrogateForm::Utf32(be)
        } else {
            SurrogateForm::Utf16(be)
        })
    }
}

fn is_surrogate(cp: u32) -> bool {
    (0xD800..0xE000).contains(&cp)
}

fn push_cp(out: &mut String, cp: u32) {
    if !push_code_point(out, cp) {
        out.push('\u{fffd}');
    }
}

fn push_hex_escape(out: &mut String, cp: u32) {
    use std::fmt::Write;
    let _ = if cp < 0x100 {
        write!(out, "\\x{cp:02x}")
    } else if cp < 0x10000 {
        write!(out, "\\u{cp:04x}")
    } else {
        write!(out, "\\U{cp:08x}")
    };
}

fn backslash_chars(chars: &[u32]) -> String {
    let mut s = String::new();
    for &c in chars {
        push_hex_escape(&mut s, c);
    }
    s
}

fn xmlcharref_chars(chars: &[u32]) -> String {
    chars.iter().map(|&c| format!("&#{};", c)).collect()
}

fn namereplace_chars(chars: &[u32]) -> String {
    let mut s = String::new();
    for &c in chars {
        let name = if is_surrogate(c) {
            None
        } else {
            char::from_u32(c).and_then(crate::lexer::string::char_name)
        };
        match name {
            Some(n) => {
                s.push_str("\\N{");
                s.push_str(&n);
                s.push('}');
            }
            None => push_hex_escape(&mut s, c),
        }
    }
    s
}

fn surrogateescape_encode(chars: &[u32]) -> Option<Vec<u8>> {
    chars
        .iter()
        .map(|&cp| (0xDC80..0xDD00).contains(&cp).then(|| (cp - 0xDC00) as u8))
        .collect()
}

fn surrogatepass_encode(chars: &[u32], form: SurrogateForm) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(chars.len() * form.len());
    for &cp in chars {
        if !is_surrogate(cp) {
            return None;
        }
        form.put(&mut out, cp);
    }
    Some(out)
}

fn surrogateescape_decode(data: &[u8], start: usize, end: usize) -> Option<(String, usize)> {
    let mut s = String::new();
    let mut n = 0;
    while n < 4 && start + n < end.min(data.len()) && data[start + n] >= 128 {
        push_cp(&mut s, 0xDC00 + data[start + n] as u32);
        n += 1;
    }
    (n > 0).then_some((s, start + n))
}

fn backslash_bytes(bytes: &[u8]) -> String {
    let mut s = String::new();
    for &b in bytes {
        push_hex_escape(&mut s, b as u32);
    }
    s
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ExcKind {
    Encode,
    Decode,
    Translate,
}

impl ExcKind {
    fn class(self) -> &'static str {
        match self {
            ExcKind::Encode => "UnicodeEncodeError",
            ExcKind::Decode => "UnicodeDecodeError",
            ExcKind::Translate => "UnicodeTranslateError",
        }
    }
}

fn new_unicode_exc(
    it: &mut Interp,
    kind: ExcKind,
    encoding: &str,
    object: Value,
    start: usize,
    end: usize,
    reason: &str,
) -> Obj {
    let cls = it.exc_type(kind.class());
    let mut args = Vec::with_capacity(5);
    if kind != ExcKind::Translate {
        args.push(Value::str(encoding));
    }
    args.extend([
        object.clone(),
        Value::Int(start as i64),
        Value::Int(end as i64),
        Value::str(reason),
    ]);
    let e = it.new_exc(&cls, args);
    let d = it.instance_dict(&e);
    dict_set_str(
        &d,
        "encoding",
        if kind == ExcKind::Translate {
            Value::None
        } else {
            Value::str(encoding)
        },
    );
    dict_set_str(&d, "object", object);
    dict_set_str(&d, "start", Value::Int(start as i64));
    dict_set_str(&d, "end", Value::Int(end as i64));
    dict_set_str(&d, "reason", Value::str(reason));
    e
}

pub fn encode_error(
    it: &mut Interp,
    encoding: &str,
    s: &str,
    start: usize,
    end: usize,
    reason: &str,
) -> Obj {
    new_unicode_exc(
        it,
        ExcKind::Encode,
        encoding,
        Value::str(s),
        start,
        end,
        reason,
    )
}

pub fn decode_error(
    it: &mut Interp,
    encoding: &str,
    data: &[u8],
    start: usize,
    end: usize,
    reason: &str,
) -> Obj {
    new_unicode_exc(
        it,
        ExcKind::Decode,
        encoding,
        Value::bytes(data.to_vec()),
        start,
        end,
        reason,
    )
}

/// What an encode error handler substitutes.
enum Repl {
    Str(String),
    Bytes(Vec<u8>),
}

/// Error-handling state for one encode or decode call: the resolved handler and the exception
/// object reused across calls to a Python handler, as CPython does.
struct ErrCtx<'a> {
    mode: ErrorMode,
    encoding: &'a str,
    handler: Option<Value>,
    exc: Option<Obj>,
}

fn update_exc(e: &Obj, start: usize, end: usize, reason: &str) {
    if let Some(d) = e.dict.borrow().as_ref() {
        dict_set_str(d, "start", Value::Int(start as i64));
        dict_set_str(d, "end", Value::Int(end as i64));
        dict_set_str(d, "reason", Value::str(reason));
    }
}

impl<'a> ErrCtx<'a> {
    fn new(errors: &str, encoding: &'a str) -> Self {
        ErrCtx {
            mode: ErrorMode::parse(errors),
            encoding,
            handler: None,
            exc: None,
        }
    }

    fn encode_exc(
        &mut self,
        it: &mut Interp,
        s: &str,
        start: usize,
        end: usize,
        reason: &str,
    ) -> Obj {
        match &self.exc {
            Some(e) => {
                update_exc(e, start, end, reason);
                e.clone()
            }
            None => {
                let e = encode_error(it, self.encoding, s, start, end, reason);
                self.exc = Some(e.clone());
                e
            }
        }
    }

    fn decode_exc(
        &mut self,
        it: &mut Interp,
        data: &[u8],
        start: usize,
        end: usize,
        reason: &str,
    ) -> Obj {
        match &self.exc {
            Some(e) => {
                update_exc(e, start, end, reason);
                e.clone()
            }
            None => {
                let e = decode_error(it, self.encoding, data, start, end, reason);
                self.exc = Some(e.clone());
                e
            }
        }
    }

    fn handler(&mut self, it: &mut Interp, name: &str) -> R<Value> {
        if let Some(h) = &self.handler {
            return Ok(h.clone());
        }
        let h = lookup_error(it, name)?;
        self.handler = Some(h.clone());
        Ok(h)
    }

    #[allow(clippy::too_many_arguments)]
    fn on_encode(
        &mut self,
        it: &mut Interp,
        s: &str,
        chars: &[u32],
        start: usize,
        end: usize,
        reason: &str,
        sp: Option<SurrogateForm>,
    ) -> R<(Repl, usize)> {
        let seg = &chars[start..end];
        let r = match &self.mode {
            ErrorMode::Strict => return Err(self.encode_exc(it, s, start, end, reason)),
            ErrorMode::Ignore => Repl::Str(String::new()),
            ErrorMode::Replace => Repl::Str("?".repeat(end - start)),
            ErrorMode::BackslashReplace => Repl::Str(backslash_chars(seg)),
            ErrorMode::XmlCharRefReplace => Repl::Str(xmlcharref_chars(seg)),
            ErrorMode::NameReplace => Repl::Str(namereplace_chars(seg)),
            ErrorMode::SurrogateEscape => match surrogateescape_encode(seg) {
                Some(b) => Repl::Bytes(b),
                None => return Err(self.encode_exc(it, s, start, end, reason)),
            },
            ErrorMode::SurrogatePass => match sp.and_then(|f| surrogatepass_encode(seg, f)) {
                Some(b) => Repl::Bytes(b),
                None => return Err(self.encode_exc(it, s, start, end, reason)),
            },
            ErrorMode::Other(name) => {
                let name = name.clone();
                let h = self.handler(it, &name)?;
                let e = self.encode_exc(it, s, start, end, reason);
                let r = it.call(&h, vec![Value::Obj(e)], vec![])?;
                let (rep, pos) = match r.tuple_items() {
                    Some([rep, pos])
                        if (rep.as_str().is_some() || is_bytes(rep)) && pos.is_int_like() =>
                    {
                        (rep.clone(), pos.clone())
                    }
                    _ => {
                        return Err(it.type_error(
                            "encoding error handler must return (str/bytes, int) tuple",
                        ));
                    }
                };
                let newpos = handler_pos(it, &pos, chars.len())?;
                let rep = match rep.as_str() {
                    Some(t) => Repl::Str(t.to_string()),
                    None => Repl::Bytes(it.bytes_of(&rep)?),
                };
                return Ok((rep, newpos));
            }
        };
        Ok((r, end))
    }

    /// [`Self::on_decode`] for a [`utf::Malformed`] sequence, appending the replacement.
    fn on_malformed(
        &mut self,
        it: &mut Interp,
        data: &mut Cow<'_, [u8]>,
        m: utf::Malformed,
        out: &mut String,
        sp: SurrogateForm,
    ) -> R<usize> {
        let (rep, pos) = self.on_decode(it, data, m.start, m.end, m.reason, Some(sp))?;
        out.push_str(&rep);
        Ok(pos)
    }

    fn on_decode(
        &mut self,
        it: &mut Interp,
        data: &mut Cow<'_, [u8]>,
        start: usize,
        end: usize,
        reason: &str,
        sp: Option<SurrogateForm>,
    ) -> R<(String, usize)> {
        let r = match &self.mode {
            ErrorMode::Strict => return Err(self.decode_exc(it, data, start, end, reason)),
            ErrorMode::Ignore => String::new(),
            ErrorMode::Replace => "\u{fffd}".to_string(),
            ErrorMode::BackslashReplace => backslash_bytes(&data[start..end]),
            ErrorMode::XmlCharRefReplace | ErrorMode::NameReplace => {
                return Err(
                    it.type_error("don't know how to handle UnicodeDecodeError in error callback")
                );
            }
            ErrorMode::SurrogateEscape => match surrogateescape_decode(data, start, end) {
                Some(r) => return Ok(r),
                None => return Err(self.decode_exc(it, data, start, end, reason)),
            },
            ErrorMode::SurrogatePass => {
                match sp.and_then(|f| f.read(&data[start..]).map(|cp| (cp, f.len()))) {
                    Some((cp, n)) => {
                        let mut s = String::new();
                        push_cp(&mut s, cp);
                        return Ok((s, start + n));
                    }
                    None => return Err(self.decode_exc(it, data, start, end, reason)),
                }
            }
            ErrorMode::Other(name) => {
                let name = name.clone();
                let h = self.handler(it, &name)?;
                let e = self.decode_exc(it, data, start, end, reason);
                let r = it.call(&h, vec![Value::Obj(e.clone())], vec![])?;
                let (rep, pos) = match r.tuple_items() {
                    Some([rep, pos]) if rep.as_str().is_some() && pos.is_int_like() => {
                        (rep.as_str().unwrap_or("").to_string(), pos.clone())
                    }
                    _ => {
                        return Err(
                            it.type_error("decoding error handler must return (str, int) tuple")
                        );
                    }
                };
                let object = e
                    .dict
                    .borrow()
                    .as_ref()
                    .and_then(|d| dict_get_str(d, "object"));
                if let Some(object) = object {
                    match &object {
                        Value::Obj(o) if matches!(o.kind, Kind::Bytes(_)) => {
                            if let Kind::Bytes(b) = &o.kind {
                                if b[..] != data[..] {
                                    *data = Cow::Owned(b.clone());
                                }
                            }
                        }
                        _ => return Err(it.type_error("exception attribute object must be bytes")),
                    }
                }
                let newpos = handler_pos(it, &pos, data.len())?;
                return Ok((rep, newpos));
            }
        };
        Ok((r, end))
    }
}

fn is_bytes(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Bytes(_)))
}

fn handler_pos(it: &mut Interp, pos: &Value, len: usize) -> R<usize> {
    let mut p = it.index_of(pos)?;
    if p < 0 {
        p += len as i64;
    }
    if p < 0 || p > len as i64 {
        return Err(it.new_exc_str(
            "IndexError",
            &format!("position {} from error handler out of bounds", p),
        ));
    }
    Ok(p as usize)
}

// ---- encoders --------------------------------------------------------------------------------

/// A code-point-at-a-time encoder: UTF-8/16/32, Latin-1 and ASCII.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum UnitEnc {
    Utf8,
    Utf16(bool),
    Utf32(bool),
    Latin1,
    Ascii,
}

impl UnitEnc {
    fn ok(self, cp: u32) -> bool {
        match self {
            UnitEnc::Utf8 | UnitEnc::Utf16(_) | UnitEnc::Utf32(_) => !is_surrogate(cp),
            UnitEnc::Latin1 => cp < 256,
            UnitEnc::Ascii => cp < 128,
        }
    }

    /// Writes `cp`, which `ok` accepted.
    fn put(self, out: &mut Vec<u8>, cp: u32) {
        let c = char::from_u32(cp).unwrap_or('\u{fffd}');
        match self {
            UnitEnc::Utf8 => {
                let mut b = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
            UnitEnc::Utf16(be) => {
                let mut b = [0u16; 2];
                for u in c.encode_utf16(&mut b) {
                    out.extend(if be { u.to_be_bytes() } else { u.to_le_bytes() });
                }
            }
            UnitEnc::Utf32(be) => out.extend(if be {
                (c as u32).to_be_bytes()
            } else {
                (c as u32).to_le_bytes()
            }),
            UnitEnc::Latin1 | UnitEnc::Ascii => out.push(c as u32 as u8),
        }
    }

    fn reason(self) -> &'static str {
        match self {
            UnitEnc::Utf8 | UnitEnc::Utf16(_) | UnitEnc::Utf32(_) => "surrogates not allowed",
            UnitEnc::Latin1 => "ordinal not in range(256)",
            UnitEnc::Ascii => "ordinal not in range(128)",
        }
    }

    fn unit(self) -> usize {
        match self {
            UnitEnc::Utf16(_) => 2,
            UnitEnc::Utf32(_) => 4,
            _ => 1,
        }
    }

    fn surrogate_form(self) -> Option<SurrogateForm> {
        match self {
            UnitEnc::Utf8 => Some(SurrogateForm::Utf8),
            UnitEnc::Utf16(be) => Some(SurrogateForm::Utf16(be)),
            UnitEnc::Utf32(be) => Some(SurrogateForm::Utf32(be)),
            _ => None,
        }
    }
}

fn encode_units(
    it: &mut Interp,
    enc: UnitEnc,
    name: &str,
    s: &str,
    errors: &str,
    out: &mut Vec<u8>,
) -> R<()> {
    let clean = match enc {
        UnitEnc::Utf8 => {
            if !may_contain(s) {
                out.extend_from_slice(s.as_bytes());
                return Ok(());
            }
            false
        }
        UnitEnc::Ascii => {
            if s.is_ascii() {
                out.extend_from_slice(s.as_bytes());
                return Ok(());
            }
            false
        }
        _ => !may_contain(s) && (enc != UnitEnc::Latin1 || s.chars().all(|c| (c as u32) < 256)),
    };
    if clean {
        out.reserve(s.len() * enc.unit());
        for c in s.chars() {
            enc.put(out, c as u32);
        }
        return Ok(());
    }
    let chars: Vec<u32> = code_points(s).collect();
    let mut ctx = ErrCtx::new(errors, name);
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if enc.ok(c) {
            enc.put(out, c);
            i += 1;
            continue;
        }
        let mut end = i + 1;
        if enc.unit() == 1 {
            while end < chars.len() && !enc.ok(chars[end]) {
                end += 1;
            }
        }
        let (rep, newpos) =
            ctx.on_encode(it, s, &chars, i, end, enc.reason(), enc.surrogate_form())?;
        match rep {
            Repl::Str(r) => {
                for rc in code_points(&r) {
                    if !enc.ok(rc) {
                        return Err(encode_error(it, name, s, i, end, enc.reason()));
                    }
                    enc.put(out, rc);
                }
            }
            Repl::Bytes(b) => {
                if b.len() % enc.unit() != 0 {
                    return Err(encode_error(it, name, s, i, end, enc.reason()));
                }
                out.extend_from_slice(&b);
            }
        }
        i = newpos;
    }
    Ok(())
}

fn bom16(be: bool) -> [u8; 2] {
    if be {
        [0xfe, 0xff]
    } else {
        [0xff, 0xfe]
    }
}

fn bom32(be: bool) -> [u8; 4] {
    if be {
        [0, 0, 0xfe, 0xff]
    } else {
        [0xff, 0xfe, 0, 0]
    }
}

pub fn utf16_encode(it: &mut Interp, s: &str, errors: &str, byteorder: i32) -> R<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 2 + 2);
    let name = match byteorder {
        0 => {
            out.extend(bom16(false));
            "utf-16"
        }
        b if b < 0 => "utf-16-le",
        _ => "utf-16-be",
    };
    encode_units(it, UnitEnc::Utf16(byteorder > 0), name, s, errors, &mut out)?;
    Ok(out)
}

pub fn utf32_encode(it: &mut Interp, s: &str, errors: &str, byteorder: i32) -> R<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 4 + 4);
    let name = match byteorder {
        0 => {
            out.extend(bom32(false));
            "utf-32"
        }
        b if b < 0 => "utf-32-le",
        _ => "utf-32-be",
    };
    encode_units(it, UnitEnc::Utf32(byteorder > 0), name, s, errors, &mut out)?;
    Ok(out)
}

pub fn utf8_encode(it: &mut Interp, s: &str, errors: &str) -> R<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    encode_units(it, UnitEnc::Utf8, "utf-8", s, errors, &mut out)?;
    Ok(out)
}

pub fn latin1_encode(it: &mut Interp, s: &str, errors: &str) -> R<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    encode_units(it, UnitEnc::Latin1, "latin-1", s, errors, &mut out)?;
    Ok(out)
}

pub fn ascii_encode(it: &mut Interp, s: &str, errors: &str) -> R<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    encode_units(it, UnitEnc::Ascii, "ascii", s, errors, &mut out)?;
    Ok(out)
}

pub fn encode_native(it: &mut Interp, codec: Native, s: &str, errors: &str) -> R<Vec<u8>> {
    match codec {
        Native::Utf8 => utf8_encode(it, s, errors),
        Native::Utf8Sig => {
            let mut out = vec![0xef, 0xbb, 0xbf];
            encode_units(it, UnitEnc::Utf8, "utf-8", s, errors, &mut out)?;
            Ok(out)
        }
        Native::Utf16(bo) => utf16_encode(it, s, errors, bo),
        Native::Utf32(bo) => utf32_encode(it, s, errors, bo),
        Native::Latin1 => latin1_encode(it, s, errors),
        Native::Ascii => ascii_encode(it, s, errors),
    }
}

/// `charmap_encode`: `mapping` maps code points to ints, bytes or None (a dict, or anything with
/// `__getitem__`); `None` means Latin-1.
pub fn charmap_encode(it: &mut Interp, s: &str, errors: &str, mapping: &Value) -> R<Vec<u8>> {
    if mapping.is_none() {
        return latin1_encode(it, s, errors);
    }
    let chars: Vec<u32> = code_points(s).collect();
    let mut out = Vec::with_capacity(chars.len());
    let mut ctx = ErrCtx::new(errors, "charmap");
    let reason = "character maps to <undefined>";
    let mut i = 0;
    while i < chars.len() {
        if charmap_put(it, mapping, chars[i], &mut out)? {
            i += 1;
            continue;
        }
        let mut end = i + 1;
        while end < chars.len() && !charmap_lookup_ok(it, mapping, chars[end])? {
            end += 1;
        }
        let (rep, newpos) = ctx.on_encode(it, s, &chars, i, end, reason, None)?;
        match rep {
            Repl::Str(r) => {
                for rc in code_points(&r) {
                    if !charmap_put(it, mapping, rc, &mut out)? {
                        return Err(encode_error(it, "charmap", s, i, end, reason));
                    }
                }
            }
            Repl::Bytes(b) => out.extend_from_slice(&b),
        }
        i = newpos;
    }
    Ok(out)
}

fn charmap_lookup(it: &mut Interp, mapping: &Value, cp: u32) -> R<Option<Value>> {
    match it.getitem(mapping, &Value::Int(cp as i64)) {
        Ok(Value::None) => Ok(None),
        Ok(v) => Ok(Some(v)),
        Err(e) if it.exc_is(&e, "LookupError") => Ok(None),
        Err(e) => Err(e),
    }
}

fn charmap_lookup_ok(it: &mut Interp, mapping: &Value, cp: u32) -> R<bool> {
    Ok(charmap_lookup(it, mapping, cp)?.is_some())
}

fn charmap_put(it: &mut Interp, mapping: &Value, cp: u32, out: &mut Vec<u8>) -> R<bool> {
    match charmap_lookup(it, mapping, cp)? {
        None => Ok(false),
        Some(v) if v.is_int_like() => {
            let n = it.index_of(&v)?;
            if !(0..256).contains(&n) {
                return Err(it.type_error("character mapping must be in range(256)"));
            }
            out.push(n as u8);
            Ok(true)
        }
        Some(Value::Obj(o)) if matches!(o.kind, Kind::Bytes(_)) => {
            if let Kind::Bytes(b) = &o.kind {
                out.extend_from_slice(b);
            }
            Ok(true)
        }
        Some(_) => {
            Err(it.type_error("character mapping must return integer, bytes or None, not str"))
        }
    }
}

/// `charmap_build`: the encoding table for a decoding table, as a dict (CPython builds an
/// `EncodingMap` for full 256-entry tables; lookups behave the same).
pub fn charmap_build(it: &mut Interp, table: &str) -> R<Value> {
    let d = it.new_dict();
    for (i, c) in code_points(table).enumerate() {
        if c != 0xFFFE {
            it.dict_set(&d, Value::Int(c as i64), Value::Int(i as i64))?;
        }
    }
    Ok(Value::Obj(d))
}

pub fn unicode_escape_encode(s: &str, raw: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for cp in code_points(s) {
        let c = char::from_u32(cp).unwrap_or('\u{fffd}');
        if raw {
            if cp < 0x100 {
                out.push(cp as u8);
                continue;
            }
        } else {
            match c {
                '\\' => {
                    out.extend_from_slice(b"\\\\");
                    continue;
                }
                '\t' => {
                    out.extend_from_slice(b"\\t");
                    continue;
                }
                '\n' => {
                    out.extend_from_slice(b"\\n");
                    continue;
                }
                '\r' => {
                    out.extend_from_slice(b"\\r");
                    continue;
                }
                ' '..='~' => {
                    out.push(cp as u8);
                    continue;
                }
                _ => {}
            }
        }
        let mut e = String::new();
        push_hex_escape(&mut e, cp);
        out.extend_from_slice(e.as_bytes());
    }
    out
}

/// `escape_encode`: bytes as the body of a bytes literal.
pub fn escape_encode(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for &b in data {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\'' => out.extend_from_slice(b"\\'"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0x20..=0x7e => out.push(b),
            _ => out.extend_from_slice(format!("\\x{:02x}", b).as_bytes()),
        }
    }
    out
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// RFC 2152 classes of ASCII for the UTF-7 encoder: 0 = set D, 1 = set O, 2 = whitespace,
/// 3 = must be base64 encoded.
const UTF7_CATEGORY: [u8; 128] = [
    3, 3, 3, 3, 3, 3, 3, 3, 3, 2, 2, 3, 3, 2, 3, 3, //
    3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, //
    2, 1, 1, 1, 1, 1, 1, 0, 0, 0, 1, 3, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 0, //
    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 3, 1, 1, 1, //
    1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, //
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 3, 3, //
];

fn is_b64(c: u32) -> bool {
    c < 128 && (c as u8).is_ascii_alphanumeric() || c == '+' as u32 || c == '/' as u32
}

fn from_b64(c: u8) -> u32 {
    match c {
        b'A'..=b'Z' => (c - b'A') as u32,
        b'a'..=b'z' => (c - b'a' + 26) as u32,
        b'0'..=b'9' => (c - b'0' + 52) as u32,
        b'+' => 62,
        _ => 63,
    }
}

fn utf7_direct(c: u32) -> bool {
    c > 0 && c < 128 && UTF7_CATEGORY[c as usize] != 3
}

pub fn utf7_encode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut in_shift = false;
    let mut bits = 0u32;
    let mut buf = 0u64;
    let push_unit = |out: &mut Vec<u8>, bits: &mut u32, buf: &mut u64, u: u32| {
        *bits += 16;
        *buf = (*buf << 16) | u as u64;
        while *bits >= 6 {
            out.push(B64[((*buf >> (*bits - 6)) & 0x3f) as usize]);
            *bits -= 6;
        }
        *buf &= (1u64 << *bits) - 1;
    };
    for ch in code_points(s) {
        if in_shift {
            if utf7_direct(ch) {
                if bits > 0 {
                    out.push(B64[((buf << (6 - bits)) & 0x3f) as usize]);
                    buf = 0;
                    bits = 0;
                }
                in_shift = false;
                if is_b64(ch) || ch == '-' as u32 {
                    out.push(b'-');
                }
                out.push(ch as u8);
                continue;
            }
        } else if ch == '+' as u32 {
            out.extend_from_slice(b"+-");
            continue;
        } else if utf7_direct(ch) {
            out.push(ch as u8);
            continue;
        } else {
            out.push(b'+');
            in_shift = true;
        }
        if ch >= 0x10000 {
            let v = ch - 0x10000;
            push_unit(&mut out, &mut bits, &mut buf, 0xD800 + (v >> 10));
            push_unit(&mut out, &mut bits, &mut buf, 0xDC00 + (v & 0x3ff));
        } else {
            push_unit(&mut out, &mut bits, &mut buf, ch);
        }
    }
    if bits > 0 {
        out.push(B64[((buf << (6 - bits)) & 0x3f) as usize]);
    }
    if in_shift {
        out.push(b'-');
    }
    out
}

// ---- decoders --------------------------------------------------------------------------------

pub fn utf8_decode(
    it: &mut Interp,
    input: &[u8],
    errors: &str,
    final_: bool,
) -> R<(String, usize)> {
    if let Ok(s) = std::str::from_utf8(input) {
        return Ok((escape_text(s).into_owned(), input.len()));
    }
    let mut out = String::with_capacity(input.len());
    let mut ctx = ErrCtx::new(errors, "utf-8");
    let pos = utf::decode_utf8(
        &mut Cow::Borrowed(input),
        final_,
        Spelling::CodePoints,
        &mut out,
        |data, m, out| ctx.on_malformed(it, data, m, out, SurrogateForm::Utf8),
    )?;
    Ok((out, pos))
}

/// UTF-16 with byte order `bo` (0 = read a BOM, else little-endian; updated when a BOM is read).
pub fn utf16_decode(
    it: &mut Interp,
    input: &[u8],
    errors: &str,
    bo: &mut i32,
    final_: bool,
) -> R<(String, usize)> {
    let mut pos = 0;
    if *bo == 0 && input.len() >= 2 {
        match (input[0], input[1]) {
            (0xff, 0xfe) => {
                *bo = -1;
                pos = 2;
            }
            (0xfe, 0xff) => {
                *bo = 1;
                pos = 2;
            }
            _ => {}
        }
    }
    let be = *bo > 0;
    let mut out = String::with_capacity(input.len() / 2);
    let mut ctx = ErrCtx::new(errors, if be { "utf-16-be" } else { "utf-16-le" });
    let pos = utf::decode_utf16(
        &mut Cow::Borrowed(input),
        pos,
        be,
        final_,
        Spelling::CodePoints,
        &mut out,
        |data, m, out| ctx.on_malformed(it, data, m, out, SurrogateForm::Utf16(be)),
    )?;
    Ok((out, pos))
}

/// UTF-32 with byte order `bo`, as [`utf16_decode`].
pub fn utf32_decode(
    it: &mut Interp,
    input: &[u8],
    errors: &str,
    bo: &mut i32,
    final_: bool,
) -> R<(String, usize)> {
    let mut pos = 0;
    if *bo == 0 && input.len() >= 4 {
        match input[..4] {
            [0xff, 0xfe, 0, 0] => {
                *bo = -1;
                pos = 4;
            }
            [0, 0, 0xfe, 0xff] => {
                *bo = 1;
                pos = 4;
            }
            _ => {}
        }
    }
    let be = *bo > 0;
    let name = if be { "utf-32-be" } else { "utf-32-le" };
    let mut data = Cow::Borrowed(input);
    let mut out = String::with_capacity(input.len() / 4);
    let mut ctx = ErrCtx::new(errors, name);
    let sp = Some(SurrogateForm::Utf32(be));
    while pos < data.len() {
        let (start, end, reason) = if pos + 3 >= data.len() {
            if !final_ {
                break;
            }
            (pos, data.len(), "truncated data")
        } else {
            let b = [data[pos], data[pos + 1], data[pos + 2], data[pos + 3]];
            let cp = if be {
                u32::from_be_bytes(b)
            } else {
                u32::from_le_bytes(b)
            };
            if cp > 0x10FFFF {
                (pos, pos + 4, "code point not in range(0x110000)")
            } else if is_surrogate(cp) {
                (
                    pos,
                    pos + 4,
                    "code point in surrogate code point range(0xd800, 0xe000)",
                )
            } else {
                push_cp(&mut out, cp);
                pos += 4;
                continue;
            }
        };
        let (rep, newpos) = ctx.on_decode(it, &mut data, start, end, reason, sp)?;
        out.push_str(&rep);
        pos = newpos;
    }
    Ok((out, pos))
}

pub fn latin1_decode(data: &[u8]) -> String {
    data.iter().map(|&b| b as char).collect()
}

pub fn ascii_decode(it: &mut Interp, input: &[u8], errors: &str) -> R<String> {
    if input.is_ascii() {
        return Ok(std::str::from_utf8(input).unwrap_or("").to_string());
    }
    let mut data = Cow::Borrowed(input);
    let mut out = String::with_capacity(input.len());
    let mut ctx = ErrCtx::new(errors, "ascii");
    let mut pos = 0;
    while pos < data.len() {
        let b = data[pos];
        if b < 128 {
            out.push(b as char);
            pos += 1;
            continue;
        }
        let (rep, newpos) = ctx.on_decode(
            it,
            &mut data,
            pos,
            pos + 1,
            "ordinal not in range(128)",
            None,
        )?;
        out.push_str(&rep);
        pos = newpos;
    }
    Ok(out)
}

/// `charmap_decode`: `mapping` is a decoding-table str, or maps byte values to code points, str
/// or None; `None` means Latin-1.
pub fn charmap_decode(it: &mut Interp, input: &[u8], errors: &str, mapping: &Value) -> R<String> {
    if mapping.is_none() {
        return Ok(latin1_decode(input));
    }
    let table: Option<Vec<u32>> = mapping.as_str().map(|t| code_points(t).collect());
    let mut data = Cow::Borrowed(input);
    let mut out = String::with_capacity(input.len());
    let mut ctx = ErrCtx::new(errors, "charmap");
    let mut pos = 0;
    while pos < data.len() {
        let b = data[pos];
        let ok = match &table {
            Some(t) => match t.get(b as usize) {
                Some(&c) if c != 0xFFFE => {
                    push_cp(&mut out, c);
                    true
                }
                _ => false,
            },
            None => match charmap_lookup(it, mapping, b as u32)? {
                None => false,
                Some(v) if v.is_int_like() => {
                    let n = it.index_of(&v)?;
                    if !(0..=0x10FFFF).contains(&n) {
                        return Err(it.type_error("character mapping must be in range(0x110000)"));
                    }
                    if n == 0xFFFE {
                        false
                    } else {
                        push_cp(&mut out, n as u32);
                        true
                    }
                }
                Some(v) => match v.as_str() {
                    Some("\u{fffe}") => false,
                    Some(s) => {
                        out.push_str(s);
                        true
                    }
                    None => {
                        return Err(
                            it.type_error("character mapping must return integer, None or str")
                        );
                    }
                },
            },
        };
        if ok {
            pos += 1;
            continue;
        }
        let (rep, newpos) = ctx.on_decode(
            it,
            &mut data,
            pos,
            pos + 1,
            "character maps to <undefined>",
            None,
        )?;
        out.push_str(&rep);
        pos = newpos;
    }
    Ok(out)
}

fn hex_val(b: u8) -> Option<u32> {
    (b as char).to_digit(16)
}

/// `unicode_escape_decode` (or `raw_unicode_escape_decode` when `raw`).
pub fn unicode_escape_decode(
    it: &mut Interp,
    input: &[u8],
    errors: &str,
    final_: bool,
    raw: bool,
) -> R<(String, usize)> {
    let name = if raw {
        "rawunicodeescape"
    } else {
        "unicodeescape"
    };
    let mut data = Cow::Borrowed(input);
    let mut out = String::with_capacity(input.len());
    let mut ctx = ErrCtx::new(errors, name);
    let mut pos = 0;
    while pos < data.len() {
        let b = data[pos];
        if b != b'\\' {
            out.push(b as char);
            pos += 1;
            continue;
        }
        let start = pos;
        if pos + 1 >= data.len() {
            if !final_ {
                return Ok((out, start));
            }
            if raw {
                out.push('\\');
                pos += 1;
                continue;
            }
            let dlen = data.len();
            let (rep, newpos) =
                ctx.on_decode(it, &mut data, start, dlen, "\\ at end of string", None)?;
            out.push_str(&rep);
            pos = newpos;
            continue;
        }
        let e = data[pos + 1];
        pos += 2;
        let count = match e {
            b'x' if !raw => 2,
            b'u' => 4,
            b'U' => 8,
            _ if raw => {
                out.push('\\');
                out.push(e as char);
                continue;
            }
            b'\n' => continue,
            b'\\' | b'\'' | b'"' => {
                out.push(e as char);
                continue;
            }
            b'b' => {
                out.push('\x08');
                continue;
            }
            b'f' => {
                out.push('\x0c');
                continue;
            }
            b't' => {
                out.push('\t');
                continue;
            }
            b'n' => {
                out.push('\n');
                continue;
            }
            b'r' => {
                out.push('\r');
                continue;
            }
            b'v' => {
                out.push('\x0b');
                continue;
            }
            b'a' => {
                out.push('\x07');
                continue;
            }
            b'0'..=b'7' => {
                let mut v = (e - b'0') as u32;
                for _ in 0..2 {
                    match data.get(pos) {
                        Some(&d @ b'0'..=b'7') => {
                            v = v * 8 + (d - b'0') as u32;
                            pos += 1;
                        }
                        _ => break,
                    }
                }
                push_cp(&mut out, v);
                continue;
            }
            b'N' => {
                // \N{name}
                let mut p = pos;
                let err: Option<(usize, &str)>;
                if p >= data.len() {
                    if !final_ {
                        return Ok((out, start));
                    }
                    err = Some((p, "malformed \\N character escape"));
                } else if data[p] == b'{' {
                    p += 1;
                    let name_start = p;
                    while p < data.len() && data[p] != b'}' {
                        p += 1;
                    }
                    if p >= data.len() {
                        if !final_ {
                            return Ok((out, start));
                        }
                        err = Some((p, "malformed \\N character escape"));
                    } else if p == name_start {
                        err = Some((p, "malformed \\N character escape"));
                    } else {
                        let nm = String::from_utf8_lossy(&data[name_start..p]).into_owned();
                        p += 1;
                        match crate::lexer::string::lookup_name(&nm) {
                            Some(c) => {
                                out.push(c);
                                pos = p;
                                continue;
                            }
                            None => err = Some((p, "unknown Unicode character name")),
                        }
                    }
                } else {
                    err = Some((p, "malformed \\N character escape"));
                }
                let (end, reason) = err.unwrap_or((p, "malformed \\N character escape"));
                let (rep, newpos) = ctx.on_decode(it, &mut data, start, end, reason, None)?;
                out.push_str(&rep);
                pos = newpos;
                continue;
            }
            other => {
                out.push('\\');
                out.push(other as char);
                continue;
            }
        };
        let what = match count {
            2 => "truncated \\xXX escape",
            4 => "truncated \\uXXXX escape",
            _ => "truncated \\UXXXXXXXX escape",
        };
        let mut v = 0u32;
        let mut n = 0;
        let mut err: Option<(usize, &str)> = None;
        while n < count {
            match data.get(pos) {
                None => {
                    if !final_ {
                        return Ok((out, start));
                    }
                    err = Some((pos, what));
                    break;
                }
                Some(&d) => match hex_val(d) {
                    Some(h) => {
                        v = v * 16 + h;
                        pos += 1;
                        n += 1;
                    }
                    None => {
                        err = Some((pos, what));
                        break;
                    }
                },
            }
        }
        if err.is_none() && v > 0x10FFFF {
            err = Some((
                pos,
                if raw {
                    "\\Uxxxxxxxx out of range"
                } else {
                    "illegal Unicode character"
                },
            ));
        }
        match err {
            None => push_cp(&mut out, v),
            Some((end, reason)) => {
                let (rep, newpos) = ctx.on_decode(it, &mut data, start, end, reason, None)?;
                out.push_str(&rep);
                pos = newpos;
            }
        }
    }
    Ok((out, pos))
}

/// `escape_decode`: the body of a bytes literal; `Err` carries the ValueError message.
pub fn escape_decode(data: &[u8], errors: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        i += 1;
        if b != b'\\' {
            out.push(b);
            continue;
        }
        let Some(&e) = data.get(i) else {
            return Err("Trailing \\ in string".into());
        };
        i += 1;
        match e {
            b'\n' => {}
            b'\\' | b'\'' | b'"' => out.push(e),
            b'b' => out.push(8),
            b'f' => out.push(12),
            b't' => out.push(b'\t'),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b'v' => out.push(11),
            b'a' => out.push(7),
            b'0'..=b'7' => {
                let mut v = (e - b'0') as u32;
                for _ in 0..2 {
                    match data.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            v = v * 8 + (d - b'0') as u32;
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(v as u8);
            }
            b'x' => {
                let hi = data.get(i).copied().and_then(hex_val);
                let lo = data.get(i + 1).copied().and_then(hex_val);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 2;
                    }
                    _ => match errors {
                        "strict" => {
                            return Err(format!("invalid \\x escape at position {}", i - 2));
                        }
                        "replace" => {
                            out.push(b'?');
                            i += hi.is_some() as usize;
                        }
                        "ignore" => i += hi.is_some() as usize,
                        other => {
                            return Err(format!(
                                "decoding error; unknown error handling code: {}",
                                other
                            ));
                        }
                    },
                }
            }
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
    }
    Ok(out)
}

pub fn utf7_decode(
    it: &mut Interp,
    input: &[u8],
    errors: &str,
    final_: bool,
) -> R<(String, usize)> {
    let mut data = Cow::Borrowed(input);
    let mut out = String::with_capacity(input.len());
    let mut ctx = ErrCtx::new(errors, "utf7");
    let mut pos = 0;
    let mut in_shift = false;
    let mut bits = 0u32;
    let mut buf = 0u64;
    let mut surrogate = 0u32;
    let mut start = 0;
    let mut shift_out_start = 0;
    loop {
        while pos < data.len() {
            let ch = data[pos];
            let mut err: Option<&str> = None;
            if in_shift {
                if is_b64(ch as u32) {
                    buf = (buf << 6) | from_b64(ch) as u64;
                    bits += 6;
                    pos += 1;
                    if bits >= 16 {
                        let out_ch = ((buf >> (bits - 16)) & 0xffff) as u32;
                        bits -= 16;
                        buf &= (1u64 << bits) - 1;
                        if surrogate != 0 {
                            if (0xDC00..0xE000).contains(&out_ch) {
                                push_cp(
                                    &mut out,
                                    0x10000 + ((surrogate - 0xD800) << 10) + (out_ch - 0xDC00),
                                );
                                surrogate = 0;
                                continue;
                            }
                            push_cp(&mut out, surrogate);
                            surrogate = 0;
                        }
                        if (0xD800..0xDC00).contains(&out_ch) {
                            surrogate = out_ch;
                        } else {
                            push_cp(&mut out, out_ch);
                        }
                    }
                    continue;
                }
                in_shift = false;
                if bits > 6 {
                    pos += 1;
                    err = Some("partial character in shift sequence");
                } else if buf != 0 {
                    pos += 1;
                    err = Some("non-zero padding bits in shift sequence");
                } else {
                    if surrogate != 0 && ch < 128 && ch != b'+' {
                        push_cp(&mut out, surrogate);
                    }
                    surrogate = 0;
                    if ch == b'-' {
                        pos += 1;
                    }
                }
            } else if ch == b'+' {
                start = pos;
                pos += 1;
                if pos < data.len() && data[pos] == b'-' {
                    pos += 1;
                    out.push('+');
                } else if pos < data.len() && !is_b64(data[pos] as u32) {
                    pos += 1;
                    err = Some("ill-formed sequence");
                } else {
                    in_shift = true;
                    surrogate = 0;
                    shift_out_start = out.len();
                    bits = 0;
                    buf = 0;
                }
            } else if ch < 128 {
                pos += 1;
                out.push(ch as char);
            } else {
                start = pos;
                pos += 1;
                err = Some("unexpected special character");
            }
            if let Some(reason) = err {
                let (rep, newpos) = ctx.on_decode(it, &mut data, start, pos, reason, None)?;
                out.push_str(&rep);
                pos = newpos;
            }
        }
        if in_shift && final_ {
            in_shift = false;
            if surrogate != 0 || bits >= 6 || (bits > 0 && buf != 0) {
                let end = data.len();
                let (rep, newpos) = ctx.on_decode(
                    it,
                    &mut data,
                    start,
                    end,
                    "unterminated shift sequence",
                    None,
                )?;
                out.push_str(&rep);
                pos = newpos;
                if pos < data.len() {
                    continue;
                }
            }
        }
        break;
    }
    if !final_ && in_shift {
        out.truncate(shift_out_start);
        return Ok((out, start));
    }
    Ok((out, pos))
}

pub fn decode_native(it: &mut Interp, codec: Native, data: &[u8], errors: &str) -> R<String> {
    Ok(match codec {
        Native::Utf8 => utf8_decode(it, data, errors, true)?.0,
        Native::Utf8Sig => {
            let body = data.strip_prefix(&[0xef, 0xbb, 0xbf][..]).unwrap_or(data);
            utf8_decode(it, body, errors, true)?.0
        }
        Native::Utf16(mut bo) => utf16_decode(it, data, errors, &mut bo, true)?.0,
        Native::Utf32(mut bo) => utf32_decode(it, data, errors, &mut bo, true)?.0,
        Native::Latin1 => latin1_decode(data),
        Native::Ascii => ascii_decode(it, data, errors)?,
    })
}

// ---- registry ---------------------------------------------------------------------------------

fn lookup_err(it: &mut Interp, encoding: &str) -> Obj {
    it.new_exc_str("LookupError", &format!("unknown encoding: {}", encoding))
}

/// `codecs.lookup`: imports `encodings` on first use so its search function is registered,
/// then asks every search function in order; results are cached by normalized name.
pub fn lookup(it: &mut Interp, encoding: &str) -> R<Value> {
    if !it.codecs.encodings_imported {
        it.codecs.encodings_imported = true;
        it.import_module("encodings")?;
    }
    let norm = normalize_encoding(encoding);
    if let Some(v) = it.codecs.cache.get(&norm) {
        return Ok(v.clone());
    }
    let path = it.codecs.search_path.clone();
    for f in path {
        let r = it.call(&f, vec![Value::str(&norm)], vec![])?;
        if r.is_none() {
            continue;
        }
        if !matches!(r.tuple_items(), Some(t) if t.len() == 4) {
            return Err(it.type_error("codec search functions must return 4-tuples"));
        }
        it.codecs.cache.insert(norm, r.clone());
        return Ok(r);
    }
    Err(lookup_err(it, encoding))
}

pub fn register(it: &mut Interp, f: Value) -> R<()> {
    if !it.is_callable(&f) {
        return Err(it.type_error("argument must be callable"));
    }
    it.codecs.search_path.push(f);
    Ok(())
}

pub fn unregister(it: &mut Interp, f: &Value) {
    if let Some(i) = it.codecs.search_path.iter().position(|v| v.is(f)) {
        it.codecs.search_path.remove(i);
        it.codecs.cache.clear();
    }
}

/// `lookup()` that refuses codecs marked `_is_text_encoding = False` (CPython's
/// `_PyCodec_LookupTextEncoding`).
pub fn lookup_text(it: &mut Interp, encoding: &str, alternate: &str) -> R<Value> {
    let codec = lookup(it, encoding)?;
    let exact_tuple = std::rc::Rc::ptr_eq(&it.type_of(&codec), &it.types.tuple);
    if !exact_tuple {
        if let Ok(flag) = it.get_attr_str(&codec, "_is_text_encoding") {
            if !it.truthy(&flag)? {
                return Err(it.new_exc_str(
                    "LookupError",
                    &format!(
                        "'{}' is not a text encoding; use {} to handle arbitrary codecs",
                        encoding, alternate
                    ),
                ));
            }
        }
    }
    Ok(codec)
}

fn codec_item(codec: &Value, i: usize) -> Value {
    codec
        .tuple_items()
        .and_then(|t| t.get(i).cloned())
        .unwrap_or(Value::None)
}

fn pair_first(it: &mut Interp, r: &Value, what: &str) -> R<Value> {
    match r.tuple_items() {
        Some([v, _]) => Ok(v.clone()),
        _ => Err(it.type_error(&format!("{} must return a tuple (object, integer)", what))),
    }
}

/// `codecs.encode(obj, encoding, errors)`: any codec, any object types.
pub fn encode_obj(it: &mut Interp, obj: &Value, encoding: &str, errors: &str) -> R<Value> {
    let codec = lookup(it, encoding)?;
    let f = codec_item(&codec, 0);
    let r = it.call(&f, vec![obj.clone(), Value::str(errors)], vec![])?;
    pair_first(it, &r, "encoder")
}

/// `codecs.decode(obj, encoding, errors)`.
pub fn decode_obj(it: &mut Interp, obj: &Value, encoding: &str, errors: &str) -> R<Value> {
    let codec = lookup(it, encoding)?;
    let f = codec_item(&codec, 1);
    let r = it.call(&f, vec![obj.clone(), Value::str(errors)], vec![])?;
    match r.tuple_items() {
        Some([v, _]) => Ok(v.clone()),
        _ => Err(it.type_error("decoder must return a tuple (object,integer)")),
    }
}

/// `str.encode`: native codecs directly, anything else through the registry (text codecs only).
pub fn encode(it: &mut Interp, s: &str, encoding: &str, errors: &str) -> R<Vec<u8>> {
    if let Some(n) = native_codec(&normalize_encoding(encoding)) {
        return encode_native(it, n, s, errors);
    }
    let codec = lookup_text(it, encoding, "codecs.encode()")?;
    let f = codec_item(&codec, 0);
    let r = it.call(&f, vec![Value::str(s), Value::str(errors)], vec![])?;
    let v = pair_first(it, &r, "encoder")?;
    if let Value::Obj(o) = &v {
        match &o.kind {
            Kind::Bytes(b) => return Ok(b.clone()),
            Kind::ByteArray(b) => return Ok(b.to_vec()),
            _ => {}
        }
    }
    let t = it.type_name_of(&v);
    Err(it.type_error(&format!(
        "'{}' encoder returned '{}' instead of 'bytes'; use codecs.encode() to encode to arbitrary types",
        encoding, t
    )))
}

/// `bytes.decode`: native codecs directly, anything else through the registry (text codecs only).
pub fn decode(it: &mut Interp, b: &[u8], encoding: &str, errors: &str) -> R<String> {
    if let Some(n) = native_codec(&normalize_encoding(encoding)) {
        return decode_native(it, n, b, errors);
    }
    let codec = lookup_text(it, encoding, "codecs.decode()")?;
    let f = codec_item(&codec, 1);
    let r = it.call(
        &f,
        vec![Value::bytes(b.to_vec()), Value::str(errors)],
        vec![],
    )?;
    let v = match r.tuple_items() {
        Some([v, _]) => v.clone(),
        _ => return Err(it.type_error("decoder must return a tuple (object,integer)")),
    };
    match v.as_str() {
        Some(s) => Ok(s.to_string()),
        None => {
            let t = it.type_name_of(&v);
            Err(it.type_error(&format!(
                "'{}' decoder returned '{}' instead of 'str'; use codecs.decode() to decode to arbitrary types",
                encoding, t
            )))
        }
    }
}

// ---- error handler registry ------------------------------------------------------------------

const BUILTIN_ERRORS: [(&str, &str); 8] = [
    ("strict", "strict_errors"),
    ("ignore", "ignore_errors"),
    ("replace", "replace_errors"),
    ("xmlcharrefreplace", "xmlcharrefreplace_errors"),
    ("backslashreplace", "backslashreplace_errors"),
    ("namereplace", "namereplace_errors"),
    ("surrogateescape", "surrogateescape"),
    ("surrogatepass", "surrogatepass"),
];

fn ensure_errors(it: &mut Interp) {
    if it.codecs.builtin_errors_ready {
        return;
    }
    it.codecs.builtin_errors_ready = true;
    for (fname, v) in crate::bind::function_values::<error_handlers::Module>() {
        if let Some((name, _)) = BUILTIN_ERRORS.iter().find(|(_, f)| *f == fname) {
            it.codecs.errors.insert(name.to_string(), v);
        }
    }
}

pub fn lookup_error(it: &mut Interp, name: &str) -> R<Value> {
    ensure_errors(it);
    match it.codecs.errors.get(name) {
        Some(v) => Ok(v.clone()),
        None => Err(it.new_exc_str(
            "LookupError",
            &format!("unknown error handler name '{}'", name),
        )),
    }
}

pub fn register_error(it: &mut Interp, name: &str, handler: Value) -> R<()> {
    if !it.is_callable(&handler) {
        return Err(it.type_error("handler must be callable"));
    }
    ensure_errors(it);
    it.codecs.errors.insert(name.to_string(), handler);
    Ok(())
}

pub fn unregister_error(it: &mut Interp, name: &str) -> R<bool> {
    ensure_errors(it);
    if BUILTIN_ERRORS.iter().any(|(n, _)| *n == name) {
        return Err(it.value_error(&format!(
            "cannot un-register built-in error handler '{name}'"
        )));
    }
    Ok(it.codecs.errors.remove(name).is_some())
}

// The built-in handlers as Python callables (`codecs.strict_errors`, ...), working from the
// exception's attributes as CPython's `PyCodec_*Errors` do.

struct ExcView {
    kind: ExcKind,
    object: Value,
    start: usize,
    end: usize,
}

fn exc_kind_of(it: &mut Interp, v: &Value) -> Option<ExcKind> {
    let t = it.type_of(v);
    [ExcKind::Encode, ExcKind::Decode, ExcKind::Translate]
        .into_iter()
        .find(|k| {
            let c = it.exc_type(k.class());
            it.is_subtype(&t, &c)
        })
}

fn exc_view(it: &mut Interp, exc: &Value) -> R<ExcView> {
    let Some(kind) = exc_kind_of(it, exc) else {
        return Err(unhandled(it, exc));
    };
    let object = it.get_attr_str(exc, "object")?;
    let len = match kind {
        ExcKind::Decode => it.bytes_of(&object)?.len(),
        _ => match object.as_pystr() {
            Some(s) => s.nchars,
            None => return Err(it.type_error("object attribute must be unicode")),
        },
    };
    let sv = it.get_attr_str(exc, "start")?;
    let ev = it.get_attr_str(exc, "end")?;
    let start = it.index_of(&sv)?;
    let end = it.index_of(&ev)?;
    let start = if start < 0 {
        0
    } else if start as usize >= len {
        len.saturating_sub(1)
    } else {
        start as usize
    };
    let end = if end < 1 {
        1
    } else if end as usize > len {
        len
    } else {
        end as usize
    };
    Ok(ExcView {
        kind,
        object,
        start,
        end,
    })
}

fn view_chars(v: &ExcView) -> Vec<u32> {
    let s = v.object.as_str().unwrap_or("");
    code_points(s)
        .skip(v.start)
        .take(v.end.saturating_sub(v.start))
        .collect()
}

fn unhandled(it: &mut Interp, exc: &Value) -> Obj {
    let t = it.type_name_of(exc);
    it.type_error(&format!("don't know how to handle {} in error callback", t))
}

fn raise_exc(it: &mut Interp, v: &Value) -> Obj {
    match v {
        Value::Obj(o) => o.clone(),
        _ => it.type_error("codec must pass exception instance"),
    }
}

// The built-in error handlers (`codecs.lookup_error("strict")`, ...).
#[lumen_bind::module(name = "builtins")]
mod error_handlers {
    use super::*;

    /// Implements the 'strict' error handling, which raises a UnicodeError on coding errors.
    #[op(hint(py(text_signature = "")))]
    fn strict_errors(it: &mut Interp, exception: &Value) -> R<Value> {
        match exception {
            Value::Obj(o) if matches!(o.kind, Kind::Exception(_)) => Err(o.clone()),
            _ => Err(it.type_error("codec must pass exception instance")),
        }
    }

    /// Implements the 'ignore' error handling, which ignores malformed data and continues.
    #[op(hint(py(text_signature = "")))]
    fn ignore_errors(it: &mut Interp, exception: &Value) -> R<Value> {
        let v = exc_view(it, exception)?;
        Ok(Value::tuple(vec![Value::str(""), Value::Int(v.end as i64)]))
    }

    /// Implements the 'replace' error handling, which replaces malformed data with a replacement marker.
    #[op(hint(py(text_signature = "")))]
    fn replace_errors(it: &mut Interp, exception: &Value) -> R<Value> {
        let v = exc_view(it, exception)?;
        let n = v.end.saturating_sub(v.start);
        let r = match v.kind {
            ExcKind::Encode => "?".repeat(n),
            ExcKind::Decode => "\u{fffd}".to_string(),
            ExcKind::Translate => "\u{fffd}".repeat(n),
        };
        Ok(Value::tuple(vec![
            Value::string(r),
            Value::Int(v.end as i64),
        ]))
    }

    /// Implements the 'xmlcharrefreplace' error handling, which replaces an unencodable character with the appropriate XML character reference.
    #[op(hint(py(text_signature = "")))]
    fn xmlcharrefreplace_errors(it: &mut Interp, exception: &Value) -> R<Value> {
        let v = exc_view(it, exception)?;
        if v.kind != ExcKind::Encode {
            return Err(unhandled(it, exception));
        }
        Ok(Value::tuple(vec![
            Value::string(xmlcharref_chars(&view_chars(&v))),
            Value::Int(v.end as i64),
        ]))
    }

    /// Implements the 'backslashreplace' error handling, which replaces malformed data with a backslashed escape sequence.
    #[op(hint(py(text_signature = "")))]
    fn backslashreplace_errors(it: &mut Interp, exception: &Value) -> R<Value> {
        let v = exc_view(it, exception)?;
        let r = match v.kind {
            ExcKind::Decode => {
                let b = it.bytes_of(&v.object)?;
                backslash_bytes(&b[v.start..v.end.max(v.start)])
            }
            _ => backslash_chars(&view_chars(&v)),
        };
        Ok(Value::tuple(vec![
            Value::string(r),
            Value::Int(v.end as i64),
        ]))
    }

    /// Implements the 'namereplace' error handling, which replaces an unencodable character with a \\N{...} escape sequence.
    #[op(hint(py(text_signature = "")))]
    fn namereplace_errors(it: &mut Interp, exception: &Value) -> R<Value> {
        let v = exc_view(it, exception)?;
        if v.kind != ExcKind::Encode {
            return Err(unhandled(it, exception));
        }
        Ok(Value::tuple(vec![
            Value::string(namereplace_chars(&view_chars(&v))),
            Value::Int(v.end as i64),
        ]))
    }

    #[op(name = "surrogateescape", hint(py(text_signature = "")))]
    fn surrogateescape_errors(it: &mut Interp, exception: &Value) -> R<Value> {
        let v = exc_view(it, exception)?;
        match v.kind {
            ExcKind::Encode => match surrogateescape_encode(&view_chars(&v)) {
                Some(b) => Ok(Value::tuple(vec![
                    Value::bytes(b),
                    Value::Int(v.end as i64),
                ])),
                None => Err(raise_exc(it, exception)),
            },
            ExcKind::Decode => {
                let b = it.bytes_of(&v.object)?;
                match surrogateescape_decode(&b, v.start, v.end) {
                    Some((s, end)) => {
                        Ok(Value::tuple(vec![Value::string(s), Value::Int(end as i64)]))
                    }
                    None => Err(raise_exc(it, exception)),
                }
            }
            ExcKind::Translate => Err(unhandled(it, exception)),
        }
    }

    #[op(name = "surrogatepass", hint(py(text_signature = "")))]
    fn surrogatepass_errors(it: &mut Interp, exception: &Value) -> R<Value> {
        let v = exc_view(it, exception)?;
        if v.kind == ExcKind::Translate {
            return Err(unhandled(it, exception));
        }
        let enc = it.get_attr_str(exception, "encoding")?;
        let form = enc.as_str().and_then(SurrogateForm::from_name);
        let Some(form) = form else {
            return Err(raise_exc(it, exception));
        };
        match v.kind {
            ExcKind::Encode => match surrogatepass_encode(&view_chars(&v), form) {
                Some(b) => Ok(Value::tuple(vec![
                    Value::bytes(b),
                    Value::Int(v.end as i64),
                ])),
                None => Err(raise_exc(it, exception)),
            },
            _ => {
                let b = it.bytes_of(&v.object)?;
                match form.read(&b[v.start.min(b.len())..]) {
                    Some(cp) => {
                        let mut s = String::new();
                        push_cp(&mut s, cp);
                        Ok(Value::tuple(vec![
                            Value::string(s),
                            Value::Int((v.start + form.len()) as i64),
                        ]))
                    }
                    None => Err(raise_exc(it, exception)),
                }
            }
        }
    }
}

// ---- incremental codecs ------------------------------------------------------------------------

enum DecState {
    /// Native decoder; `bo` is the UTF-16/32 byte order (0 = no BOM read yet) and `first` whether
    /// a UTF-8 signature may still follow.
    Native {
        codec: Native,
        buf: Vec<u8>,
        bo: i32,
        first: bool,
    },
    Py(Value),
}

/// An incremental decoder for any codec: native state machines for the UTF and Latin-1/ASCII
/// codecs (mirroring `encodings.*.IncrementalDecoder`), otherwise the codec's Python object.
pub struct IncrementalDecoder {
    state: DecState,
    errors: String,
}

impl IncrementalDecoder {
    pub fn new(it: &mut Interp, encoding: &str, errors: &str) -> R<Self> {
        if let Some(codec) = native_codec(&normalize_encoding(encoding)) {
            let bo = match codec {
                Native::Utf16(b) | Native::Utf32(b) => b,
                _ => 0,
            };
            return Ok(IncrementalDecoder {
                state: DecState::Native {
                    codec,
                    buf: Vec::new(),
                    bo,
                    first: true,
                },
                errors: errors.to_string(),
            });
        }
        let codec = lookup_text(it, encoding, "codecs.decode()")?;
        let factory = it.get_attr_str(&codec, "incrementaldecoder")?;
        if factory.is_none() {
            return Err(lookup_err(it, encoding));
        }
        let obj = it.call(&factory, vec![Value::str(errors)], vec![])?;
        Ok(IncrementalDecoder {
            state: DecState::Py(obj),
            errors: errors.to_string(),
        })
    }

    pub fn decode(&mut self, it: &mut Interp, input: &[u8], final_: bool) -> R<String> {
        let errors = self.errors.clone();
        match &mut self.state {
            DecState::Py(obj) => {
                let obj = obj.clone();
                let r = it.call_method(
                    &obj,
                    "decode",
                    vec![Value::bytes(input.to_vec()), Value::Bool(final_)],
                )?;
                match r.as_str() {
                    Some(s) => Ok(s.to_string()),
                    None => {
                        let t = it.type_name_of(&r);
                        Err(it.type_error(&format!(
                            "decoder should return a string result, not '{}'",
                            t
                        )))
                    }
                }
            }
            DecState::Native {
                codec,
                buf,
                bo,
                first,
            } => {
                let codec = *codec;
                let mut data = std::mem::take(buf);
                data.extend_from_slice(input);
                let (out, consumed) = match codec {
                    Native::Latin1 => (latin1_decode(&data), data.len()),
                    Native::Ascii => (ascii_decode(it, &data, &errors)?, data.len()),
                    Native::Utf8 => utf8_decode(it, &data, &errors, final_)?,
                    Native::Utf8Sig => {
                        let bom = [0xefu8, 0xbb, 0xbf];
                        if *first {
                            if data.len() < 3 && bom.starts_with(&data) {
                                *buf = data;
                                return Ok(String::new());
                            }
                            *first = false;
                            if data.starts_with(&bom) {
                                let (s, n) = utf8_decode(it, &data[3..], &errors, final_)?;
                                *buf = data[3 + n..].to_vec();
                                return Ok(s);
                            }
                        }
                        utf8_decode(it, &data, &errors, final_)?
                    }
                    Native::Utf16(_) | Native::Utf32(_) => {
                        let wide = matches!(codec, Native::Utf32(_));
                        let had_bo = *bo != 0;
                        let r = if wide {
                            utf32_decode(it, &data, &errors, bo, final_)?
                        } else {
                            utf16_decode(it, &data, &errors, bo, final_)?
                        };
                        if !had_bo && *bo == 0 && r.1 >= if wide { 4 } else { 2 } {
                            let msg = if wide {
                                "UTF-32 stream does not start with BOM"
                            } else {
                                "UTF-16 stream does not start with BOM"
                            };
                            return Err(it.new_exc_str("UnicodeError", msg));
                        }
                        r
                    }
                };
                *buf = data[consumed..].to_vec();
                Ok(out)
            }
        }
    }

    pub fn reset(&mut self, it: &mut Interp) -> R<()> {
        match &mut self.state {
            DecState::Py(obj) => {
                let obj = obj.clone();
                it.call_method(&obj, "reset", vec![])?;
            }
            DecState::Native {
                codec,
                buf,
                bo,
                first,
            } => {
                buf.clear();
                *first = true;
                *bo = match codec {
                    Native::Utf16(b) | Native::Utf32(b) => *b,
                    _ => 0,
                };
            }
        }
        Ok(())
    }

    /// `(buffered bytes, flag)`, the flag as the matching `encodings` decoder reports it.
    pub fn getstate(&self, it: &mut Interp) -> R<(Vec<u8>, i64)> {
        match &self.state {
            DecState::Py(obj) => {
                let r = it.call_method(obj, "getstate", vec![])?;
                match r.tuple_items() {
                    Some([b, f]) => {
                        let b = it.bytes_of(b)?;
                        let f = it.index_of(f)?;
                        Ok((b, f))
                    }
                    _ => Err(it.type_error("illegal decoder state")),
                }
            }
            DecState::Native {
                codec,
                buf,
                bo,
                first,
            } => {
                let flag = match codec {
                    Native::Utf8Sig => *first as i64,
                    Native::Utf16(0) | Native::Utf32(0) => match *bo {
                        0 => 2,
                        b if b > 0 => 1,
                        _ => 0,
                    },
                    _ => 0,
                };
                Ok((buf.clone(), flag))
            }
        }
    }

    pub fn setstate(&mut self, it: &mut Interp, buf: Vec<u8>, flag: i64) -> R<()> {
        match &mut self.state {
            DecState::Py(obj) => {
                let obj = obj.clone();
                it.call_method(
                    &obj,
                    "setstate",
                    vec![Value::tuple(vec![Value::bytes(buf), Value::Int(flag)])],
                )?;
            }
            DecState::Native {
                codec,
                buf: b,
                bo,
                first,
            } => {
                *b = buf;
                match codec {
                    Native::Utf8Sig => *first = flag != 0,
                    Native::Utf16(0) | Native::Utf32(0) => {
                        *bo = match flag {
                            0 => -1,
                            1 => 1,
                            _ => 0,
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

enum EncState {
    /// `started`: the BOM/signature has been written (or `setstate` said not to write one).
    Native {
        codec: Native,
        started: bool,
    },
    Py(Value),
}

/// An incremental encoder for any codec, as [`IncrementalDecoder`].
pub struct IncrementalEncoder {
    state: EncState,
    errors: String,
}

impl IncrementalEncoder {
    pub fn new(it: &mut Interp, encoding: &str, errors: &str) -> R<Self> {
        if let Some(codec) = native_codec(&normalize_encoding(encoding)) {
            return Ok(IncrementalEncoder {
                state: EncState::Native {
                    codec,
                    started: false,
                },
                errors: errors.to_string(),
            });
        }
        let codec = lookup_text(it, encoding, "codecs.encode()")?;
        let factory = it.get_attr_str(&codec, "incrementalencoder")?;
        if factory.is_none() {
            return Err(lookup_err(it, encoding));
        }
        let obj = it.call(&factory, vec![Value::str(errors)], vec![])?;
        Ok(IncrementalEncoder {
            state: EncState::Py(obj),
            errors: errors.to_string(),
        })
    }

    pub fn encode(&mut self, it: &mut Interp, s: &str, final_: bool) -> R<Vec<u8>> {
        let errors = self.errors.clone();
        match &mut self.state {
            EncState::Py(obj) => {
                let obj = obj.clone();
                let r = it.call_method(&obj, "encode", vec![Value::str(s), Value::Bool(final_)])?;
                match &r {
                    Value::Obj(o) if matches!(o.kind, Kind::Bytes(_) | Kind::ByteArray(_)) => {
                        it.bytes_of(&r)
                    }
                    _ => {
                        let t = it.type_name_of(&r);
                        Err(it.type_error(&format!(
                            "encoder should return a bytes object, not '{}'",
                            t
                        )))
                    }
                }
            }
            EncState::Native { codec, started } => {
                let first = !*started;
                *started = true;
                match *codec {
                    Native::Utf8Sig if !first => utf8_encode(it, s, &errors),
                    Native::Utf16(0) if !first => utf16_encode(it, s, &errors, -1),
                    Native::Utf32(0) if !first => utf32_encode(it, s, &errors, -1),
                    c => encode_native(it, c, s, &errors),
                }
            }
        }
    }

    pub fn reset(&mut self, it: &mut Interp) -> R<()> {
        match &mut self.state {
            EncState::Py(obj) => {
                let obj = obj.clone();
                it.call_method(&obj, "reset", vec![])?;
            }
            EncState::Native { started, .. } => *started = false,
        }
        Ok(())
    }

    pub fn getstate(&self, it: &mut Interp) -> R<i64> {
        match &self.state {
            EncState::Py(obj) => {
                let r = it.call_method(obj, "getstate", vec![])?;
                it.index_of(&r)
            }
            EncState::Native { codec, started } => Ok(match codec {
                Native::Utf8Sig => !*started as i64,
                Native::Utf16(0) | Native::Utf32(0) => {
                    if *started {
                        0
                    } else {
                        2
                    }
                }
                _ => 0,
            }),
        }
    }

    pub fn setstate(&mut self, it: &mut Interp, state: i64) -> R<()> {
        match &mut self.state {
            EncState::Py(obj) => {
                let obj = obj.clone();
                it.call_method(&obj, "setstate", vec![Value::Int(state)])?;
            }
            EncState::Native { codec, started } => {
                if matches!(codec, Native::Utf8Sig | Native::Utf16(0) | Native::Utf32(0)) {
                    *started = state == 0;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_cpython() {
        assert_eq!(normalize_encoding("UTF-8"), "utf_8");
        assert_eq!(normalize_encoding(" Foo  Bar "), "foo_bar");
        assert_eq!(normalize_encoding("a.b-c"), "a.b_c");
        assert_eq!(normalize_encoding("é-x"), "x");
        assert_eq!(native_codec("latin_1"), Some(Native::Latin1));
    }

    #[test]
    fn surrogate_forms() {
        assert_eq!(
            SurrogateForm::from_name("utf-16"),
            Some(SurrogateForm::Utf16(false))
        );
        assert_eq!(
            SurrogateForm::from_name("UTF_32_BE"),
            Some(SurrogateForm::Utf32(true))
        );
        assert_eq!(SurrogateForm::from_name("utf-8"), Some(SurrogateForm::Utf8));
        assert_eq!(SurrogateForm::from_name("latin-1"), None);
        let mut v = Vec::new();
        SurrogateForm::Utf8.put(&mut v, 0xD800);
        assert_eq!(v, [0xed, 0xa0, 0x80]);
        assert_eq!(SurrogateForm::Utf8.read(&v), Some(0xD800));
    }

    #[test]
    fn utf7_and_escapes() {
        assert_eq!(utf7_encode("a\u{20ac}\u{1f600}"), b"a+IKzYPd4A-");
        assert_eq!(utf7_encode("1+1"), b"1+-1");
        assert_eq!(
            escape_encode(b"A\n\x00'\"\\\xff"),
            b"A\\n\\x00\\'\"\\\\\\xff"
        );
        assert_eq!(unicode_escape_encode("a\u{e9}\n", false), b"a\\xe9\\n");
        assert_eq!(
            unicode_escape_encode("a\u{e9}\u{20ac}", true),
            b"a\xe9\\u20ac"
        );
        assert_eq!(escape_decode(b"\\x41\\n\\101", "strict").unwrap(), b"A\nA");
        assert!(escape_decode(b"\\x4", "strict").is_err());
    }
}
