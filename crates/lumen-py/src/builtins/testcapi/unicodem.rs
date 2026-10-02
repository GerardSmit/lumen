//! `_testcapi` wrappers of the `PyUnicode_*` API. A `None` argument stands for a `NULL` pointer.
//! Strings are immutable here, so the C functions that edit a string in place operate on a copy
//! of the argument, as CPython's wrappers do.

use super::{bad_internal_call, builtin, call_builtin, int_value, nonnull, system_error};
use crate::ast::BinOp;
use crate::object::*;
use crate::vm::Interp;
use lumen_common::smuggle::{cmp_code_points, code_points, push_code_point};
use std::cmp::Ordering;

const MAX_UNICODE: i64 = 0x10ffff;

fn cps(s: &str) -> Vec<u32> {
    code_points(s).collect()
}

fn text_of(cps: &[u32]) -> String {
    let mut out = String::with_capacity(cps.len());
    for &c in cps {
        push_code_point(&mut out, c);
    }
    out
}

fn from_cps(cps: &[u32]) -> Value {
    Value::string(text_of(cps))
}

/// `PyUnicode_MAX_CHAR_VALUE`: the limit of the narrowest storage kind that holds the string.
fn kind_max(cps: &[u32]) -> u32 {
    match cps.iter().copied().max().unwrap_or(0) {
        0..=0x7f => 0x7f,
        0x80..=0xff => 0xff,
        0x100..=0xffff => 0xffff,
        _ => 0x10ffff,
    }
}

fn width_of(maxchar: u32) -> u64 {
    match maxchar {
        0..=0xff => 1,
        0x100..=0xffff => 2,
        _ => 4,
    }
}

/// The text of a `str` argument; `None` is a NULL pointer and anything else a bad argument.
fn text_arg(it: &mut Interp, v: &Value) -> R<Vec<u32>> {
    let v = nonnull_bad(it, v)?;
    match v.as_str() {
        Some(s) => Ok(cps(s)),
        None => Err(bad_internal_call(it)),
    }
}

fn nonnull_bad<'a>(it: &mut Interp, v: &'a Value) -> R<&'a Value> {
    if v.is_none() {
        return Err(bad_internal_call(it));
    }
    Ok(v)
}

/// `PyUnicode_Check` with `PyErr_BadArgument` for a mismatch.
fn str_or_bad_argument<'a>(it: &mut Interp, v: &'a Value) -> R<&'a str> {
    let v = nonnull(it, v)?;
    match v.as_str() {
        Some(s) => Ok(s),
        None => Err(it.type_error("bad argument type for built-in operation")),
    }
}

fn str_or_type_error<'a>(it: &mut Interp, v: &'a Value, what: &str) -> R<&'a str> {
    let v = nonnull(it, v)?;
    match v.as_str() {
        Some(s) => Ok(s),
        None => {
            let t = it.tp_name_of(v);
            Err(it.type_error(&format!("{what}, not {t}")))
        }
    }
}

fn index_error(it: &mut Interp) -> Obj {
    it.new_exc_str("IndexError", "string index out of range")
}

fn char_range_error(it: &mut Interp, ch: i64) -> Obj {
    it.value_error(&format!("character U+{ch:x} is not in range [U+0000; U+10ffff]"))
}

/// A call of an unbound `str` method, so that a subclass cannot intercept it.
fn str_method(it: &mut Interp, name: &str, args: Vec<Value>) -> R<Value> {
    let str_type = builtin(it, "str");
    let m = it.get_attr_str(&str_type, name)?;
    it.call(&m, args, Vec::new())
}

fn codecs_call(it: &mut Interp, name: &str, args: Vec<Value>) -> R<Vec<Value>> {
    let m = it.import_module("_codecs")?;
    let f = it.get_attr_str(&Value::Obj(m), name)?;
    let r = it.call(&f, args, Vec::new())?;
    Ok(r.tuple_items().map(<[Value]>::to_vec).unwrap_or_else(|| vec![r.clone()]))
}

fn errors_value(errors: Option<&str>) -> Value {
    Value::str(errors.unwrap_or("strict"))
}

fn first(mut v: Vec<Value>) -> Value {
    v.swap_remove(0)
}

fn c_string(data: &[u8]) -> &[u8] {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    &data[..end]
}

fn decode_utf8(it: &mut Interp, data: &[u8]) -> R<Value> {
    let r = codecs_call(it, "utf_8_decode", vec![Value::bytes(data.to_vec()), Value::str("strict"), Value::Bool(true)])?;
    Ok(first(r))
}

fn size_value(n: usize) -> Value {
    int_value(n as i128)
}

fn wide_units(cps: &[u32]) -> Vec<u8> {
    cps.iter().flat_map(|c| c.to_le_bytes()).collect()
}

fn from_wide(it: &mut Interp, data: &[u8], size: i64) -> R<Value> {
    let units = data.len() / 4;
    let take = (size as usize).min(units);
    let mut out = Vec::with_capacity(take);
    for chunk in data.chunks_exact(4).take(take) {
        let c = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if i64::from(c) > MAX_UNICODE {
            return Err(char_range_error(it, i64::from(c)));
        }
        out.push(c);
    }
    Ok(from_cps(&out))
}

fn compare_text(a: &str, b: &str) -> i64 {
    match cmp_code_points(a, b) {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

enum Mapped {
    Identity,
    Undefined,
    Chars(Vec<u32>),
}

fn lookup_translation(it: &mut Interp, table: &Value, c: u32) -> R<Mapped> {
    let item = match it.getitem(table, &Value::Int(i64::from(c))) {
        Ok(v) => v,
        Err(e) if it.exc_is(&e, "LookupError") => return Ok(Mapped::Identity),
        Err(e) => return Err(e),
    };
    if item.is_none() {
        return Ok(Mapped::Undefined);
    }
    if let Some(s) = item.as_str() {
        return Ok(Mapped::Chars(cps(s)));
    }
    if item.is_int_like() {
        return match item.as_i64() {
            Some(n) if (0..=MAX_UNICODE).contains(&n) => Ok(Mapped::Chars(vec![n as u32])),
            _ => Err(it.value_error("character mapping must be in range(0x110000)")),
        };
    }
    Err(it.type_error("character mapping must return integer, None or str"))
}

fn translate(it: &mut Interp, input: &[u32], table: &Value, errors: Option<&str>) -> R<Value> {
    let original = from_cps(input);
    let mut out: Vec<u32> = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match lookup_translation(it, table, input[i])? {
            Mapped::Identity => {
                out.push(input[i]);
                i += 1;
            }
            Mapped::Chars(c) => {
                out.extend(c);
                i += 1;
            }
            Mapped::Undefined => {
                let mut end = i + 1;
                while end < input.len() && matches!(lookup_translation(it, table, input[end])?, Mapped::Undefined) {
                    end += 1;
                }
                let exc_type = it.exc_type("UnicodeTranslateError");
                let exc = it.new_exc(
                    &exc_type,
                    vec![original.clone(), Value::Int(i as i64), Value::Int(end as i64), Value::str("character maps to <undefined>")],
                );
                let handler = crate::codecs::lookup_error(it, errors.unwrap_or("strict"))?;
                let r = it.call(&handler, vec![Value::Obj(exc)], Vec::new())?;
                let (replacement, newpos) = match r.tuple_items() {
                    Some([a, b]) => (a.clone(), b.clone()),
                    _ => return Err(it.type_error("encoding error handler must return (str/bytes, int) tuple")),
                };
                let Some(rep) = replacement.as_str() else {
                    return Err(it.type_error("encoding error handler must return (str, int) tuple"));
                };
                out.extend(cps(rep));
                let mut pos = newpos.as_i64().unwrap_or(0);
                if pos < 0 {
                    pos += input.len() as i64;
                }
                if pos < 0 || pos as usize > input.len() {
                    return Err(it.new_exc_str("IndexError", &format!("position {pos} from error handler out of bounds")));
                }
                i = pos as usize;
            }
        }
    }
    Ok(from_cps(&out))
}

fn adjust_args(len: usize, start: i64, end: i64) -> (usize, usize) {
    let len = len as i64;
    let end = if end > len { len } else if end < 0 { (end + len).max(0) } else { end };
    let start = if start < 0 { (start + len).max(0) } else { start };
    (start.min(len) as usize, end as usize)
}

#[lumen_bind::module(name = "_testcapi")]
pub mod unicodem {
    use super::*;

    #[op]
    fn codec_incrementalencoder(it: &mut Interp, encoding: &str, errors: Option<&str>) -> R<Value> {
        let m = it.import_module("codecs")?;
        let info = it.call_method(&Value::Obj(m), "lookup", vec![Value::str(encoding)])?;
        let factory = it.get_attr_str(&info, "incrementalencoder")?;
        let args = errors.map(|e| vec![Value::str(e)]).unwrap_or_default();
        it.call(&factory, args, Vec::new())
    }

    #[op]
    fn codec_incrementaldecoder(it: &mut Interp, encoding: &str, errors: Option<&str>) -> R<Value> {
        let m = it.import_module("codecs")?;
        let info = it.call_method(&Value::Obj(m), "lookup", vec![Value::str(encoding)])?;
        let factory = it.get_attr_str(&info, "incrementaldecoder")?;
        let args = errors.map(|e| vec![Value::str(e)]).unwrap_or_default();
        it.call(&factory, args, Vec::new())
    }

    #[op]
    fn unicode_new(it: &mut Interp, size: i64, maxchar: i64) -> R<Value> {
        if size < 0 {
            return Err(system_error(it, "Negative size passed to PyUnicode_New"));
        }
        if maxchar > MAX_UNICODE {
            return Err(system_error(it, "invalid maximum character passed to PyUnicode_New"));
        }
        let maxchar = maxchar.max(0) as u32;
        let limit = (i64::MAX as u64 - 56) / width_of(maxchar) - 1;
        if size as u64 > limit || size > 1 << 34 {
            return Err(it.memory_error());
        }
        let mut s = String::new();
        for _ in 0..size {
            push_code_point(&mut s, maxchar);
        }
        Ok(Value::string(s))
    }

    #[op]
    fn unicode_fill(it: &mut Interp, to: &Value, start: i64, length: i64, fill_char: i64) -> R<Value> {
        let text = text_arg(it, to)?;
        if start < 0 {
            return Err(index_error(it));
        }
        let fill_char = fill_char as u32;
        if fill_char > kind_max(&text) {
            return Err(it.value_error("fill character is bigger than the string maximum character"));
        }
        let len = text.len() as i64;
        let mut out = text;
        let mut filled = 0;
        if start < len {
            filled = length.min(len - start).max(0);
            for slot in &mut out[start as usize..(start + filled) as usize] {
                *slot = fill_char;
            }
        }
        Ok(Value::tuple(vec![from_cps(&out), Value::Int(filled)]))
    }

    #[op]
    fn unicode_writechar(it: &mut Interp, to: &Value, index: i64, character: i64) -> R<Value> {
        let s = str_or_bad_argument(it, to)?;
        let mut text = cps(s);
        if index < 0 || index as usize >= text.len() {
            return Err(index_error(it));
        }
        let character = character as u32;
        if character > 0x10ffff {
            return Err(char_range_error(it, i64::from(character)));
        }
        if character > kind_max(&text) {
            return Err(it.value_error(&format!("character U+{character:x} is greater than maximum character of the string")));
        }
        text[index as usize] = character;
        Ok(Value::tuple(vec![from_cps(&text), Value::Int(0)]))
    }

    #[op]
    fn unicode_resize(it: &mut Interp, obj: &Value, length: i64) -> R<Value> {
        let mut text = text_arg(it, obj)?;
        if length < 0 {
            return Err(system_error(it, "Negative size passed to PyUnicode_Resize"));
        }
        let width = width_of(kind_max(&text));
        if length as u64 > (i64::MAX as u64 - 56) / width - 1 || length > 1 << 34 {
            return Err(it.memory_error());
        }
        text.resize(length as usize, 0);
        Ok(Value::tuple(vec![from_cps(&text), Value::Int(0)]))
    }

    #[op]
    fn unicode_append(it: &mut Interp, left: &Value, right: &Value) -> R<Value> {
        let mut l = text_arg(it, left)?;
        l.extend(text_arg(it, right)?);
        Ok(from_cps(&l))
    }

    #[op]
    fn unicode_appendanddel(it: &mut Interp, left: &Value, right: &Value) -> R<Value> {
        unicode_append(it, left, right)
    }

    #[op]
    fn unicode_fromstringandsize(it: &mut Interp, s: Option<&[u8]>, bsize: Option<i64>) -> R<Value> {
        let size = bsize.unwrap_or_else(|| s.map_or(0, |b| b.len() as i64));
        if size < 0 {
            return Err(system_error(it, "Negative size passed to PyUnicode_FromStringAndSize"));
        }
        let Some(data) = s else {
            if size > 0 {
                return Err(system_error(it, "NULL string with positive size with NULL passed to PyUnicode_FromStringAndSize"));
            }
            return Ok(Value::str(""));
        };
        if size as usize > data.len() {
            return Err(it.memory_error());
        }
        decode_utf8(it, &data[..size as usize])
    }

    #[op]
    fn unicode_fromstring(it: &mut Interp, s: &[u8]) -> R<Value> {
        decode_utf8(it, c_string(s))
    }

    #[op]
    fn unicode_fromkindanddata(it: &mut Interp, kind: i64, buffer: Option<&[u8]>, size: Option<i64>) -> R<Value> {
        let size = size.unwrap_or_else(|| buffer.map_or(0, |b| b.len() as i64));
        if kind != 0 && size % kind != 0 {
            return Err(it.new_exc_str("AssertionError", "invalid size in unicode_fromkindanddata()"));
        }
        let count = if kind != 0 { size / kind } else { 0 };
        if count < 0 {
            return Err(it.value_error("size must be positive"));
        }
        if !matches!(kind, 1 | 2 | 4) {
            return Err(system_error(it, "invalid kind"));
        }
        let data = buffer.unwrap_or(&[]);
        let count = count as usize;
        if count > data.len() / kind as usize {
            return Err(bad_internal_call(it));
        }
        let mut out = Vec::with_capacity(count);
        for chunk in data.chunks_exact(kind as usize).take(count) {
            out.push(match kind {
                1 => u32::from(chunk[0]),
                2 => u32::from(u16::from_le_bytes([chunk[0], chunk[1]])),
                _ => u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            });
        }
        if out.iter().any(|&c| c > 0x10ffff) {
            return Err(system_error(it, "invalid maximum character passed to PyUnicode_New"));
        }
        Ok(from_cps(&out))
    }

    #[op]
    fn unicode_substring(it: &mut Interp, s: &Value, start: i64, end: i64) -> R<Value> {
        let text = text_arg(it, s)?;
        let len = text.len() as i64;
        let end = end.min(len);
        if start == 0 && end == len {
            return Ok(from_cps(&text));
        }
        if start < 0 || end < 0 {
            return Err(index_error(it));
        }
        if start >= len || end < start {
            return Ok(Value::str(""));
        }
        Ok(from_cps(&text[start as usize..end as usize]))
    }

    #[op]
    fn unicode_getlength(it: &mut Interp, arg: &Value) -> R<i64> {
        let s = str_or_bad_argument(it, arg)?;
        Ok(cps(s).len() as i64)
    }

    #[op]
    fn unicode_readchar(it: &mut Interp, unicode: &Value, index: i64) -> R<i64> {
        let s = str_or_bad_argument(it, unicode)?;
        let text = cps(s);
        if index < 0 || index as usize >= text.len() {
            return Err(index_error(it));
        }
        Ok(i64::from(text[index as usize]))
    }

    #[op]
    fn unicode_fromencodedobject(it: &mut Interp, obj: &Value, encoding: Option<&str>, errors: Option<&str>) -> R<Value> {
        let obj = nonnull_bad(it, obj)?;
        if obj.as_str().is_some() {
            return Err(it.type_error("decoding str is not supported"));
        }
        let view = match call_builtin(it, "memoryview", vec![obj.clone()]) {
            Ok(v) => v,
            Err(e) if it.exc_is(&e, "TypeError") => {
                let t = it.tp_name_of(obj);
                return Err(it.type_error(&format!("decoding to str: need a bytes-like object, {t} found")));
            }
            Err(e) => return Err(e),
        };
        let bytes = call_builtin(it, "bytes", vec![view])?;
        it.call_method(&bytes, "decode", vec![Value::str(encoding.unwrap_or("utf-8")), errors_value(errors)])
    }

    #[op]
    fn unicode_fromobject(it: &mut Interp, arg: &Value) -> R<Value> {
        let arg = nonnull_bad(it, arg)?;
        match arg.as_str() {
            Some(s) => Ok(Value::str(s)),
            None => {
                let t = it.tp_name_of(arg);
                Err(it.type_error(&format!("Can't convert '{t}' object to str implicitly")))
            }
        }
    }

    #[op]
    fn unicode_interninplace(it: &mut Interp, arg: &Value) -> R<Value> {
        let arg = nonnull_bad(it, arg)?;
        let sys = it.import_module("sys")?;
        let f = it.get_attr_str(&Value::Obj(sys), "intern")?;
        it.call(&f, vec![arg.clone()], Vec::new())
    }

    #[op]
    fn unicode_internfromstring(it: &mut Interp, s: &[u8]) -> R<Value> {
        let text = decode_utf8(it, c_string(s))?;
        unicode_interninplace(it, &text)
    }

    #[op]
    fn unicode_fromwidechar(it: &mut Interp, s: Option<&[u8]>, size: Option<i64>) -> R<Value> {
        let size = match size {
            Some(n) => n,
            None => {
                let len = s.map_or(0, <[u8]>::len);
                if len % 4 != 0 {
                    return Err(it.new_exc_str("AssertionError", "invalid size in unicode_fromwidechar()"));
                }
                (len / 4) as i64
            }
        };
        if size < -1 {
            return Err(system_error(it, "Negative size passed to PyUnicode_FromWideChar"));
        }
        let Some(data) = s else {
            if size != 0 {
                return Err(system_error(it, "NULL string with positive size with NULL passed to PyUnicode_FromWideChar"));
            }
            return Ok(Value::str(""));
        };
        let size = if size == -1 {
            data.chunks_exact(4).take_while(|c| c != &[0, 0, 0, 0]).count() as i64
        } else {
            size
        };
        if size as u64 > (i64::MAX as u64) / 8 || size > 1 << 34 {
            return Err(it.memory_error());
        }
        from_wide(it, data, size)
    }

    #[op]
    fn unicode_aswidechar(it: &mut Interp, unicode: &Value, buflen: i64) -> R<Value> {
        let s = str_or_bad_argument(it, unicode)?;
        let text = cps(s);
        let len = text.len() as i64;
        let (copy, size) = if buflen > len { (len + 1, len) } else { (buflen, buflen) };
        let mut out: Vec<u32> = text.iter().copied().take(copy.max(0) as usize).collect();
        out.resize(copy.max(0) as usize, 0);
        Ok(Value::tuple(vec![from_cps(&out), Value::Int(size.max(0))]))
    }

    #[op]
    fn unicode_aswidechar_null(it: &mut Interp, unicode: &Value, buflen: i64) -> R<i64> {
        let _ = buflen;
        let s = str_or_bad_argument(it, unicode)?;
        Ok(cps(s).len() as i64 + 1)
    }

    #[op]
    fn unicode_aswidecharstring(it: &mut Interp, unicode: &Value) -> R<Value> {
        let s = str_or_bad_argument(it, unicode)?;
        let mut text = cps(s);
        let size = text.len();
        text.push(0);
        Ok(Value::tuple(vec![from_cps(&text), size_value(size)]))
    }

    #[op]
    fn unicode_aswidecharstring_null(it: &mut Interp, unicode: &Value) -> R<Value> {
        let s = str_or_bad_argument(it, unicode)?;
        if s.contains('\0') {
            return Err(it.value_error("embedded null character"));
        }
        Ok(Value::str(s))
    }

    #[op]
    fn unicode_asucs4(it: &mut Interp, unicode: &Value, str_len: i64, copy_null: bool) -> R<Value> {
        let text = text_arg(it, unicode)?;
        let buf_len = str_len + 1;
        if buf_len < 0 || buf_len > 1 << 34 {
            return Err(it.memory_error());
        }
        let buf_len = buf_len as usize;
        let mut buffer = vec![0u32; buf_len];
        buffer[buf_len - 1] = 0xffff;
        if buf_len < text.len() + usize::from(copy_null) {
            return Err(system_error(it, "string is longer than the buffer"));
        }
        buffer[..text.len()].copy_from_slice(&text);
        if copy_null {
            buffer[text.len()] = 0;
        }
        Ok(from_cps(&buffer))
    }

    #[op]
    fn unicode_asucs4copy(it: &mut Interp, unicode: &Value) -> R<Value> {
        let mut text = text_arg(it, unicode)?;
        text.push(0);
        Ok(from_cps(&text))
    }

    #[op]
    fn unicode_fromordinal(it: &mut Interp, ordinal: i64) -> R<Value> {
        call_builtin(it, "chr", vec![Value::Int(ordinal)])
    }

    #[op]
    fn unicode_asutf8(it: &mut Interp, unicode: &Value, buflen: i64) -> R<Value> {
        let s = str_or_bad_argument(it, unicode)?;
        let encoded = utf8_bytes(it, s)?;
        Ok(Value::bytes(c_buffer(encoded, buflen)))
    }

    #[op]
    fn unicode_asutf8andsize(it: &mut Interp, unicode: &Value, buflen: i64) -> R<Value> {
        let s = str_or_bad_argument(it, unicode)?;
        let encoded = utf8_bytes(it, s)?;
        let size = encoded.len();
        Ok(Value::tuple(vec![Value::bytes(c_buffer(encoded, buflen)), size_value(size)]))
    }

    #[op]
    fn unicode_asutf8andsize_null(it: &mut Interp, unicode: &Value, buflen: i64) -> R<Value> {
        unicode_asutf8(it, unicode, buflen)
    }

    #[op]
    fn unicode_getdefaultencoding() -> Value {
        Value::bytes(b"utf-8".to_vec())
    }

    #[op]
    fn unicode_transformdecimalandspacetoascii(it: &mut Interp, arg: &Value) -> R<Value> {
        let text = text_arg(it, arg)?;
        let ud = it.import_module("unicodedata")?;
        let decimal = it.get_attr_str(&Value::Obj(ud), "decimal")?;
        let mut out = Vec::with_capacity(text.len());
        for c in text {
            if crate::unicode::is_space(c) {
                out.push(0x20);
                continue;
            }
            if c < 127 {
                out.push(c);
                continue;
            }
            let ch = from_cps(&[c]);
            let d = it.call(&decimal, vec![ch, Value::Int(-1)], Vec::new())?;
            match d.as_i64() {
                Some(n) if n >= 0 => out.push(0x30 + n as u32),
                _ => out.push(u32::from(b'?')),
            }
        }
        Ok(from_cps(&out))
    }

    #[op]
    fn unicode_decode(it: &mut Interp, s: &[u8], encoding: Option<&str>, errors: Option<&str>) -> R<Value> {
        let b = Value::bytes(s.to_vec());
        it.call_method(&b, "decode", vec![Value::str(encoding.unwrap_or("utf-8")), errors_value(errors)])
    }

    #[op]
    fn unicode_asencodedstring(it: &mut Interp, unicode: &Value, encoding: Option<&str>, errors: Option<&str>) -> R<Value> {
        text_arg(it, unicode)?;
        str_method(it, "encode", vec![unicode.clone(), Value::str(encoding.unwrap_or("utf-8")), errors_value(errors)])
    }

    #[op]
    fn unicode_buildencodingmap(it: &mut Interp, arg: &Value) -> R<Value> {
        let arg = nonnull_bad(it, arg)?;
        Ok(first(codecs_call(it, "charmap_build", vec![arg.clone()])?))
    }

    #[op]
    fn unicode_decodeutf7(it: &mut Interp, data: &[u8], errors: Option<&str>) -> R<Value> {
        let r = codecs_call(it, "utf_7_decode", vec![Value::bytes(data.to_vec()), errors_value(errors), Value::Bool(true)])?;
        Ok(first(r))
    }

    #[op]
    fn unicode_decodeutf7stateful(it: &mut Interp, data: &[u8], errors: Option<&str>) -> R<Value> {
        let r = codecs_call(it, "utf_7_decode", vec![Value::bytes(data.to_vec()), errors_value(errors), Value::Bool(false)])?;
        Ok(Value::tuple(r))
    }

    #[op]
    fn unicode_decodeutf8(it: &mut Interp, data: &[u8], errors: Option<&str>) -> R<Value> {
        let r = codecs_call(it, "utf_8_decode", vec![Value::bytes(data.to_vec()), errors_value(errors), Value::Bool(true)])?;
        Ok(first(r))
    }

    #[op]
    fn unicode_decodeutf8stateful(it: &mut Interp, data: &[u8], errors: Option<&str>) -> R<Value> {
        let r = codecs_call(it, "utf_8_decode", vec![Value::bytes(data.to_vec()), errors_value(errors), Value::Bool(false)])?;
        Ok(Value::tuple(r))
    }

    #[op]
    fn unicode_asutf8string(it: &mut Interp, arg: &Value) -> R<Value> {
        encode_with(it, "utf_8_encode", arg, vec![])
    }

    #[op]
    fn unicode_decodeutf16(it: &mut Interp, byteorder: i64, data: &[u8], errors: Option<&str>) -> R<Value> {
        let r = codecs_call(
            it,
            "utf_16_ex_decode",
            vec![Value::bytes(data.to_vec()), errors_value(errors), Value::Int(byteorder), Value::Bool(true)],
        )?;
        Ok(Value::tuple(vec![r[2].clone(), r[0].clone()]))
    }

    #[op]
    fn unicode_decodeutf16stateful(it: &mut Interp, byteorder: i64, data: &[u8], errors: Option<&str>) -> R<Value> {
        let r = codecs_call(
            it,
            "utf_16_ex_decode",
            vec![Value::bytes(data.to_vec()), errors_value(errors), Value::Int(byteorder), Value::Bool(false)],
        )?;
        Ok(Value::tuple(vec![r[2].clone(), r[0].clone(), r[1].clone()]))
    }

    #[op]
    fn unicode_asutf16string(it: &mut Interp, arg: &Value) -> R<Value> {
        encode_with(it, "utf_16_encode", arg, vec![Value::str("strict"), Value::Int(0)])
    }

    #[op]
    fn unicode_decodeutf32(it: &mut Interp, byteorder: i64, data: &[u8], errors: Option<&str>) -> R<Value> {
        let r = codecs_call(
            it,
            "utf_32_ex_decode",
            vec![Value::bytes(data.to_vec()), errors_value(errors), Value::Int(byteorder), Value::Bool(true)],
        )?;
        Ok(Value::tuple(vec![r[2].clone(), r[0].clone()]))
    }

    #[op]
    fn unicode_decodeutf32stateful(it: &mut Interp, byteorder: i64, data: &[u8], errors: Option<&str>) -> R<Value> {
        let r = codecs_call(
            it,
            "utf_32_ex_decode",
            vec![Value::bytes(data.to_vec()), errors_value(errors), Value::Int(byteorder), Value::Bool(false)],
        )?;
        Ok(Value::tuple(vec![r[2].clone(), r[0].clone(), r[1].clone()]))
    }

    #[op]
    fn unicode_asutf32string(it: &mut Interp, arg: &Value) -> R<Value> {
        encode_with(it, "utf_32_encode", arg, vec![Value::str("strict"), Value::Int(0)])
    }

    #[op]
    fn unicode_decodeunicodeescape(it: &mut Interp, data: &[u8], errors: Option<&str>) -> R<Value> {
        let r = codecs_call(it, "unicode_escape_decode", vec![Value::bytes(data.to_vec()), errors_value(errors), Value::Bool(true)])?;
        Ok(first(r))
    }

    #[op]
    fn unicode_asunicodeescapestring(it: &mut Interp, arg: &Value) -> R<Value> {
        encode_with(it, "unicode_escape_encode", arg, vec![])
    }

    #[op]
    fn unicode_decoderawunicodeescape(it: &mut Interp, data: &[u8], errors: Option<&str>) -> R<Value> {
        let r = codecs_call(it, "raw_unicode_escape_decode", vec![Value::bytes(data.to_vec()), errors_value(errors), Value::Bool(true)])?;
        Ok(first(r))
    }

    #[op]
    fn unicode_asrawunicodeescapestring(it: &mut Interp, arg: &Value) -> R<Value> {
        encode_with(it, "raw_unicode_escape_encode", arg, vec![])
    }

    #[op]
    fn unicode_decodelatin1(it: &mut Interp, data: &[u8], errors: Option<&str>) -> R<Value> {
        let r = codecs_call(it, "latin_1_decode", vec![Value::bytes(data.to_vec()), errors_value(errors)])?;
        Ok(first(r))
    }

    #[op]
    fn unicode_aslatin1string(it: &mut Interp, arg: &Value) -> R<Value> {
        encode_with(it, "latin_1_encode", arg, vec![])
    }

    #[op]
    fn unicode_decodeascii(it: &mut Interp, data: &[u8], errors: Option<&str>) -> R<Value> {
        let r = codecs_call(it, "ascii_decode", vec![Value::bytes(data.to_vec()), errors_value(errors)])?;
        Ok(first(r))
    }

    #[op]
    fn unicode_asasciistring(it: &mut Interp, arg: &Value) -> R<Value> {
        encode_with(it, "ascii_encode", arg, vec![])
    }

    #[op]
    fn unicode_decodecharmap(it: &mut Interp, data: &[u8], mapping: &Value, errors: Option<&str>) -> R<Value> {
        let r = if mapping.is_none() {
            codecs_call(it, "latin_1_decode", vec![Value::bytes(data.to_vec()), errors_value(errors)])?
        } else {
            codecs_call(it, "charmap_decode", vec![Value::bytes(data.to_vec()), errors_value(errors), mapping.clone()])?
        };
        Ok(first(r))
    }

    #[op]
    fn unicode_ascharmapstring(it: &mut Interp, unicode: &Value, mapping: &Value) -> R<Value> {
        text_arg(it, unicode)?;
        let mapping = nonnull_bad(it, mapping)?;
        let r = codecs_call(it, "charmap_encode", vec![unicode.clone(), Value::str("strict"), mapping.clone()])?;
        Ok(first(r))
    }

    #[op]
    fn unicode_concat(it: &mut Interp, left: &Value, right: &Value) -> R<Value> {
        let (l, r) = (nonnull(it, left)?, nonnull(it, right)?);
        match (l.as_str(), r.as_str()) {
            (Some(a), Some(b)) => Ok(Value::string(format!("{a}{b}"))),
            (None, _) => {
                let t = it.tp_name_of(l);
                Err(it.type_error(&format!("must be str, not {t}")))
            }
            (_, None) => {
                let t = it.tp_name_of(r);
                Err(it.type_error(&format!("can only concatenate str (not \"{t}\") to str")))
            }
        }
    }

    #[op]
    fn unicode_split(it: &mut Interp, s: &Value, sep: &Value, maxsplit: Option<i64>) -> R<Value> {
        let s = nonnull(it, s)?;
        let sep = sep.clone();
        str_method(it, "split", vec![s.clone(), sep, Value::Int(maxsplit.unwrap_or(-1))])
    }

    #[op]
    fn unicode_rsplit(it: &mut Interp, s: &Value, sep: &Value, maxsplit: Option<i64>) -> R<Value> {
        let s = nonnull(it, s)?;
        let sep = sep.clone();
        str_method(it, "rsplit", vec![s.clone(), sep, Value::Int(maxsplit.unwrap_or(-1))])
    }

    #[op]
    fn unicode_splitlines(it: &mut Interp, s: &Value, keepends: Option<i64>) -> R<Value> {
        let s = nonnull(it, s)?;
        str_method(it, "splitlines", vec![s.clone(), Value::Bool(keepends.unwrap_or(0) != 0)])
    }

    #[op]
    fn unicode_partition(it: &mut Interp, s: &Value, sep: &Value) -> R<Value> {
        let s = nonnull(it, s)?;
        let sep = nonnull(it, sep)?;
        str_method(it, "partition", vec![s.clone(), sep.clone()])
    }

    #[op]
    fn unicode_rpartition(it: &mut Interp, s: &Value, sep: &Value) -> R<Value> {
        let s = nonnull(it, s)?;
        let sep = nonnull(it, sep)?;
        str_method(it, "rpartition", vec![s.clone(), sep.clone()])
    }

    #[op]
    fn unicode_translate(it: &mut Interp, obj: &Value, table: &Value, errors: Option<&str>) -> R<Value> {
        let s = str_or_bad_argument(it, obj)?;
        let text = cps(s);
        if table.is_none() {
            return Err(it.type_error("bad argument type for built-in operation"));
        }
        translate(it, &text, table, errors)
    }

    #[op]
    fn unicode_join(it: &mut Interp, sep: &Value, seq: &Value) -> R<Value> {
        let sep = if sep.is_none() { Value::str(" ") } else { sep.clone() };
        let seq = nonnull(it, seq)?;
        str_method(it, "join", vec![sep, seq.clone()])
    }

    #[op]
    fn unicode_count(it: &mut Interp, s: &Value, substr: &Value, start: i64, end: i64) -> R<Value> {
        let s = nonnull(it, s)?;
        let substr = nonnull(it, substr)?;
        str_method(it, "count", vec![s.clone(), substr.clone(), Value::Int(start), Value::Int(end)])
    }

    #[op]
    fn unicode_tailmatch(it: &mut Interp, s: &Value, substr: &Value, start: i64, end: i64, direction: i64) -> R<i64> {
        str_or_type_error(it, substr, "tailmatch arg must be str")?;
        let name = if direction > 0 { "endswith" } else { "startswith" };
        let r = str_method(it, name, vec![nonnull(it, s)?.clone(), substr.clone(), Value::Int(start), Value::Int(end)])?;
        Ok(i64::from(matches!(r, Value::Bool(true))))
    }

    #[op]
    fn unicode_find(it: &mut Interp, s: &Value, substr: &Value, start: i64, end: i64, direction: i64) -> R<Value> {
        let s = nonnull(it, s)?;
        let substr = nonnull(it, substr)?;
        let name = if direction > 0 { "find" } else { "rfind" };
        str_method(it, name, vec![s.clone(), substr.clone(), Value::Int(start), Value::Int(end)])
    }

    #[op]
    fn unicode_findchar(it: &mut Interp, s: &Value, ch: i64, start: i64, end: i64, direction: i64) -> R<i64> {
        let text = text_arg(it, s)?;
        let ch = ch as u32;
        if ch > 0x10ffff {
            return Ok(-1);
        }
        let (start, end) = adjust_args(text.len(), start, end);
        if start >= end {
            return Ok(-1);
        }
        let window = &text[start..end];
        let found = if direction > 0 { window.iter().position(|&c| c == ch) } else { window.iter().rposition(|&c| c == ch) };
        Ok(found.map_or(-1, |i| (start + i) as i64))
    }

    #[op]
    fn unicode_replace(it: &mut Interp, s: &Value, substr: &Value, repl: &Value, maxcount: Option<i64>) -> R<Value> {
        let s = nonnull(it, s)?;
        let substr = nonnull(it, substr)?;
        let repl = nonnull(it, repl)?;
        str_method(it, "replace", vec![s.clone(), substr.clone(), repl.clone(), Value::Int(maxcount.unwrap_or(-1))])
    }

    #[op]
    fn unicode_compare(it: &mut Interp, left: &Value, right: &Value) -> R<i64> {
        let (l, r) = (nonnull(it, left)?, nonnull(it, right)?);
        match (l.as_str(), r.as_str()) {
            (Some(a), Some(b)) => Ok(compare_text(a, b)),
            _ => {
                let (a, b) = (it.tp_name_of(l), it.tp_name_of(r));
                Err(it.type_error(&format!("Can't compare {a} and {b}")))
            }
        }
    }

    #[op]
    fn unicode_comparewithasciistring(it: &mut Interp, left: &Value, right: Option<&[u8]>) -> R<i64> {
        let text = text_arg(it, left)?;
        let Some(right) = right else {
            return Err(bad_internal_call(it));
        };
        let right: Vec<u32> = c_string(right).iter().map(|&b| u32::from(b)).collect();
        Ok(match text.cmp(&right) {
            Ordering::Less => -1,
            Ordering::Equal => 0,
            Ordering::Greater => 1,
        })
    }

    #[op]
    fn unicode_richcompare(it: &mut Interp, left: &Value, right: &Value, op: i64) -> R<Value> {
        let (l, r) = (nonnull(it, left)?, nonnull(it, right)?);
        let (Some(a), Some(b)) = (l.as_str(), r.as_str()) else {
            return Ok(Value::NotImplemented);
        };
        let ord = cmp_code_points(a, b);
        let result = match op {
            0 => ord == Ordering::Less,
            1 => ord != Ordering::Greater,
            2 => ord == Ordering::Equal,
            3 => ord != Ordering::Equal,
            4 => ord == Ordering::Greater,
            5 => ord != Ordering::Less,
            _ => return Err(bad_internal_call(it)),
        };
        Ok(Value::Bool(result))
    }

    #[op]
    fn unicode_format(it: &mut Interp, format: &Value, fargs: &Value) -> R<Value> {
        let format = nonnull_bad(it, format)?;
        let fargs = nonnull_bad(it, fargs)?;
        if format.as_str().is_none() {
            return Err(bad_internal_call(it));
        }
        it.binary_op(BinOp::Mod, format, fargs)
    }

    #[op]
    fn unicode_contains(it: &mut Interp, container: &Value, element: &Value) -> R<i64> {
        let container = nonnull(it, container)?;
        let element = nonnull(it, element)?;
        let r = str_method(it, "__contains__", vec![container.clone(), element.clone()])?;
        Ok(i64::from(matches!(r, Value::Bool(true))))
    }

    #[op]
    fn unicode_isidentifier(it: &mut Interp, arg: &Value) -> R<i64> {
        let arg = nonnull(it, arg)?;
        let r = str_method(it, "isidentifier", vec![arg.clone()])?;
        Ok(i64::from(matches!(r, Value::Bool(true))))
    }

    #[op]
    fn unicode_copycharacters(it: &mut Interp, to: &Value, to_start: i64, from: &Value, from_start: i64, how_many: i64) -> R<Value> {
        let to = str_or_type_error(it, to, "argument 1 must be str")?;
        let to = cps(to);
        let from = text_arg(it, from).map_err(|_| bad_internal_call(it))?;
        let (from_len, to_len) = (from.len() as i64, to.len() as i64);
        if from_start < 0 || from_start > from_len || to_start < 0 || to_start > to_len {
            return Err(index_error(it));
        }
        if how_many < 0 {
            return Err(system_error(it, "how_many cannot be negative"));
        }
        let how_many = how_many.min(from_len - from_start);
        if to_start + how_many > to_len {
            return Err(system_error(it, &format!("Cannot write {how_many} characters at {to_start} in a string of {to_len} characters")));
        }
        let range = &from[from_start as usize..(from_start + how_many) as usize];
        if range.iter().copied().max().unwrap_or(0) > kind_max(&to) {
            return Err(system_error(it, "Cannot copy UCS4 characters into a string of narrower characters"));
        }
        let mut out = vec![0u32; to.len()];
        out[to_start as usize..(to_start + how_many) as usize].copy_from_slice(range);
        Ok(Value::tuple(vec![from_cps(&out), Value::Int(how_many)]))
    }

    #[op]
    fn test_widechar() {}

    #[op]
    fn test_unicode_compare_with_ascii() {}

    #[op]
    fn test_string_from_format() {}
}

pub(super) fn utf8_bytes(it: &mut Interp, s: &str) -> R<Vec<u8>> {
    let r = codecs_call(it, "utf_8_encode", vec![Value::str(s), Value::str("strict")])?;
    it.bytes_of(&first(r))
}

fn c_buffer(mut encoded: Vec<u8>, buflen: i64) -> Vec<u8> {
    encoded.push(0);
    encoded.resize(buflen.max(0) as usize, 0);
    encoded
}

fn encode_with(it: &mut Interp, name: &str, arg: &Value, extra: Vec<Value>) -> R<Value> {
    let s = str_or_bad_argument(it, arg)?;
    let mut args = vec![Value::str(s)];
    if extra.is_empty() {
        args.push(Value::str("strict"));
    } else {
        args.extend(extra);
    }
    Ok(first(codecs_call(it, name, args)?))
}
