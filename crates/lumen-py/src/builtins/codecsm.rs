//! The `_codecs` module (on top of `crate::codecs`) and the attributes of `UnicodeEncodeError`,
//! `UnicodeDecodeError` and `UnicodeTranslateError`.

use super::native::{set_fn, Kw};
use crate::codecs;
use crate::object::*;
use crate::vm::*;

fn errors_arg(it: &mut Interp, v: &Option<Value>, fname: &str, pos: usize) -> R<String> {
    match v {
        None | Some(Value::None) => Ok("strict".into()),
        Some(v) => match v.as_str() {
            Some(s) => Ok(s.to_string()),
            None => {
                let t = it.type_name_of(v);
                Err(it.type_error(&format!("{}() argument {} must be str or None, not {}", fname, pos, t)))
            }
        },
    }
}

fn str_arg(it: &mut Interp, v: &Option<Value>, fname: &str, pos: usize) -> R<String> {
    let v = v.as_ref().unwrap_or(&Value::None);
    match v.as_str() {
        Some(s) => Ok(s.to_string()),
        None => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("{}() argument {} must be str, not {}", fname, pos, t)))
        }
    }
}

fn nchars(v: &Option<Value>) -> i64 {
    v.as_ref().and_then(|v| v.as_pystr()).map(|s| s.nchars as i64).unwrap_or(0)
}

/// A bytes-like argument (bytes, bytearray, or a str read as UTF-8 where CPython's `s*` allows it).
fn buf_arg(it: &mut Interp, v: &Option<Value>, allow_str: bool) -> R<Vec<u8>> {
    let v = v.as_ref().unwrap_or(&Value::None);
    if let Value::Obj(o) = v {
        match &o.kind {
            Kind::Bytes(b) => return Ok(b.clone()),
            Kind::ByteArray(b) => return Ok(b.to_vec()),
            Kind::Str(s) if allow_str => return Ok(s.s.as_bytes().to_vec()),
            _ => {}
        }
    }
    let t = it.type_name_of(v);
    Err(it.type_error(&format!("a bytes-like object is required, not '{}'", t)))
}

fn final_arg(it: &mut Interp, v: &Option<Value>, default: bool) -> R<bool> {
    match v {
        None => Ok(default),
        Some(v) => it.truthy(v),
    }
}

fn int_arg(it: &mut Interp, v: &Option<Value>) -> R<i64> {
    match v {
        None => Ok(0),
        Some(v) => it.index_of(v),
    }
}

fn pair_str(s: String, n: usize) -> Value {
    Value::tuple(vec![Value::string(s), Value::Int(n as i64)])
}

fn pair_bytes(b: Vec<u8>, n: i64) -> Value {
    Value::tuple(vec![Value::bytes(b), Value::Int(n)])
}

// ---- registry ----

fn register(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("register", a, 1, 1)?;
    codecs::register(it, a[0].clone())?;
    Ok(Value::None)
}

fn unregister(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("unregister", a, 1, 1)?;
    codecs::unregister(it, &a[0]);
    Ok(Value::None)
}

fn name_arg(it: &mut Interp, a: &[Value], fname: &str) -> R<String> {
    it.check_args(fname, a, 1, 1)?;
    match a[0].as_str() {
        Some(s) => Ok(s.to_string()),
        None => {
            let t = it.type_name_of(&a[0]);
            Err(it.type_error(&format!("{}() argument must be str, not {}", fname, t)))
        }
    }
}

fn lookup(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let name = name_arg(it, a, "lookup")?;
    codecs::lookup(it, &name)
}

fn forget_codec(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let name = name_arg(it, a, "_forget_codec")?;
    codecs::forget_codec(it, &name);
    Ok(Value::None)
}

fn named_str(it: &mut Interp, v: &Option<Value>, fname: &str, arg: &str, default: &str) -> R<String> {
    match v {
        None => Ok(default.into()),
        Some(v) => match v.as_str() {
            Some(s) => Ok(s.to_string()),
            None => {
                let t = it.type_name_of(v);
                Err(it.type_error(&format!("{}() argument '{}' must be str, not {}", fname, arg, t)))
            }
        },
    }
}

fn encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("encode", a, kw, &["obj", "encoding", "errors"], 1)?;
    let enc = named_str(it, &b[1], "encode", "encoding", "utf-8")?;
    let errors = named_str(it, &b[2], "encode", "errors", "strict")?;
    codecs::encode_obj(it, b[0].as_ref().unwrap_or(&Value::None), &enc, &errors)
}

fn decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("decode", a, kw, &["obj", "encoding", "errors"], 1)?;
    let enc = named_str(it, &b[1], "decode", "encoding", "utf-8")?;
    let errors = named_str(it, &b[2], "decode", "errors", "strict")?;
    codecs::decode_obj(it, b[0].as_ref().unwrap_or(&Value::None), &enc, &errors)
}

fn register_error(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("register_error", a, 2, 2)?;
    let name = str_arg(it, &Some(a[0].clone()), "register_error", 1)?;
    codecs::register_error(it, &name, a[1].clone())?;
    Ok(Value::None)
}

fn lookup_error(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let name = name_arg(it, a, "lookup_error")?;
    codecs::lookup_error(it, &name)
}

fn unregister_error(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let name = name_arg(it, a, "_unregister_error")?;
    Ok(Value::Bool(codecs::unregister_error(it, &name)?))
}

// ---- codecs ----

/// `(str, errors=None)` for the stateless encoders.
fn enc_args(it: &mut Interp, a: &[Value], kw: Kw, fname: &str) -> R<(String, String, i64)> {
    let b = it.bind_args(fname, a, kw, &["str", "errors"], 1)?;
    let s = str_arg(it, &b[0], fname, 1)?;
    let errors = errors_arg(it, &b[1], fname, 2)?;
    Ok((s, errors, nchars(&b[0])))
}

/// `(data, errors=None, final=False)` for the stateful decoders.
fn dec_args(it: &mut Interp, a: &[Value], kw: Kw, fname: &str, final_default: bool) -> R<(Vec<u8>, String, bool)> {
    let b = it.bind_args(fname, a, kw, &["data", "errors", "final"], 1)?;
    let data = buf_arg(it, &b[0], false)?;
    let errors = errors_arg(it, &b[1], fname, 2)?;
    let final_ = final_arg(it, &b[2], final_default)?;
    Ok((data, errors, final_))
}

fn utf_8_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let (s, errors, n) = enc_args(it, a, kw, "utf_8_encode")?;
    Ok(pair_bytes(codecs::utf8_encode(it, &s, &errors)?, n))
}

fn utf_8_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let (d, errors, final_) = dec_args(it, a, kw, "utf_8_decode", false)?;
    let (s, n) = codecs::utf8_decode(it, &d, &errors, final_)?;
    Ok(pair_str(s, n))
}

fn utf_7_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let (s, _errors, n) = enc_args(it, a, kw, "utf_7_encode")?;
    Ok(pair_bytes(codecs::utf7_encode(&s), n))
}

fn utf_7_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let (d, errors, final_) = dec_args(it, a, kw, "utf_7_decode", false)?;
    let (s, n) = codecs::utf7_decode(it, &d, &errors, final_)?;
    Ok(pair_str(s, n))
}

fn utf_bo_encode(it: &mut Interp, a: &[Value], kw: Kw, fname: &str, wide: bool, fixed: Option<i32>) -> R<Value> {
    let (s, errors, n, bo) = if fixed.is_some() {
        let (s, e, n) = enc_args(it, a, kw, fname)?;
        (s, e, n, fixed.unwrap_or(0))
    } else {
        let b = it.bind_args(fname, a, kw, &["str", "errors", "byteorder"], 1)?;
        let s = str_arg(it, &b[0], fname, 1)?;
        let errors = errors_arg(it, &b[1], fname, 2)?;
        let bo = int_arg(it, &b[2])?;
        (s, errors, nchars(&b[0]), bo.clamp(-1, 1) as i32)
    };
    let out = if wide { codecs::utf32_encode(it, &s, &errors, bo)? } else { codecs::utf16_encode(it, &s, &errors, bo)? };
    Ok(pair_bytes(out, n))
}

fn utf_bo_decode(it: &mut Interp, a: &[Value], kw: Kw, fname: &str, wide: bool, bo: i32) -> R<Value> {
    let (d, errors, final_) = dec_args(it, a, kw, fname, false)?;
    let mut bo = bo;
    let (s, n) = if wide { codecs::utf32_decode(it, &d, &errors, &mut bo, final_)? } else { codecs::utf16_decode(it, &d, &errors, &mut bo, final_)? };
    Ok(pair_str(s, n))
}

fn utf_ex_decode(it: &mut Interp, a: &[Value], kw: Kw, fname: &str, wide: bool) -> R<Value> {
    let b = it.bind_args(fname, a, kw, &["data", "errors", "byteorder", "final"], 1)?;
    let d = buf_arg(it, &b[0], false)?;
    let errors = errors_arg(it, &b[1], fname, 2)?;
    let mut bo = int_arg(it, &b[2])?.clamp(-1, 1) as i32;
    let final_ = final_arg(it, &b[3], false)?;
    let (s, n) = if wide { codecs::utf32_decode(it, &d, &errors, &mut bo, final_)? } else { codecs::utf16_decode(it, &d, &errors, &mut bo, final_)? };
    Ok(Value::tuple(vec![Value::string(s), Value::Int(n as i64), Value::Int(bo as i64)]))
}

fn utf_16_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_bo_encode(it, a, kw, "utf_16_encode", false, None)
}
fn utf_16_le_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_bo_encode(it, a, kw, "utf_16_le_encode", false, Some(-1))
}
fn utf_16_be_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_bo_encode(it, a, kw, "utf_16_be_encode", false, Some(1))
}
fn utf_32_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_bo_encode(it, a, kw, "utf_32_encode", true, None)
}
fn utf_32_le_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_bo_encode(it, a, kw, "utf_32_le_encode", true, Some(-1))
}
fn utf_32_be_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_bo_encode(it, a, kw, "utf_32_be_encode", true, Some(1))
}
fn utf_16_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_bo_decode(it, a, kw, "utf_16_decode", false, 0)
}
fn utf_16_le_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_bo_decode(it, a, kw, "utf_16_le_decode", false, -1)
}
fn utf_16_be_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_bo_decode(it, a, kw, "utf_16_be_decode", false, 1)
}
fn utf_32_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_bo_decode(it, a, kw, "utf_32_decode", true, 0)
}
fn utf_32_le_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_bo_decode(it, a, kw, "utf_32_le_decode", true, -1)
}
fn utf_32_be_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_bo_decode(it, a, kw, "utf_32_be_decode", true, 1)
}
fn utf_16_ex_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_ex_decode(it, a, kw, "utf_16_ex_decode", false)
}
fn utf_32_ex_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    utf_ex_decode(it, a, kw, "utf_32_ex_decode", true)
}

fn latin_1_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let (s, errors, n) = enc_args(it, a, kw, "latin_1_encode")?;
    Ok(pair_bytes(codecs::latin1_encode(it, &s, &errors)?, n))
}

fn latin_1_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("latin_1_decode", a, kw, &["data", "errors"], 1)?;
    let d = buf_arg(it, &b[0], false)?;
    errors_arg(it, &b[1], "latin_1_decode", 2)?;
    let n = d.len();
    Ok(pair_str(codecs::latin1_decode(&d), n))
}

fn ascii_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let (s, errors, n) = enc_args(it, a, kw, "ascii_encode")?;
    Ok(pair_bytes(codecs::ascii_encode(it, &s, &errors)?, n))
}

fn ascii_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("ascii_decode", a, kw, &["data", "errors"], 1)?;
    let d = buf_arg(it, &b[0], false)?;
    let errors = errors_arg(it, &b[1], "ascii_decode", 2)?;
    let s = codecs::ascii_decode(it, &d, &errors)?;
    Ok(pair_str(s, d.len()))
}

fn charmap_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("charmap_encode", a, kw, &["str", "errors", "mapping"], 1)?;
    let s = str_arg(it, &b[0], "charmap_encode", 1)?;
    let errors = errors_arg(it, &b[1], "charmap_encode", 2)?;
    let mapping = b[2].clone().unwrap_or(Value::None);
    let out = codecs::charmap_encode(it, &s, &errors, &mapping)?;
    Ok(pair_bytes(out, nchars(&b[0])))
}

fn charmap_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("charmap_decode", a, kw, &["data", "errors", "mapping"], 1)?;
    let d = buf_arg(it, &b[0], false)?;
    let errors = errors_arg(it, &b[1], "charmap_decode", 2)?;
    let mapping = b[2].clone().unwrap_or(Value::None);
    let s = codecs::charmap_decode(it, &d, &errors, &mapping)?;
    Ok(pair_str(s, d.len()))
}

fn charmap_build(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("charmap_build", a, 1, 1)?;
    let s = str_arg(it, &Some(a[0].clone()), "charmap_build", 1)?;
    codecs::charmap_build(it, &s)
}

fn unicode_escape_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let (s, _errors, n) = enc_args(it, a, kw, "unicode_escape_encode")?;
    Ok(pair_bytes(codecs::unicode_escape_encode(&s, false), n))
}

fn raw_unicode_escape_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let (s, _errors, n) = enc_args(it, a, kw, "raw_unicode_escape_encode")?;
    Ok(pair_bytes(codecs::unicode_escape_encode(&s, true), n))
}

fn escape_args(it: &mut Interp, a: &[Value], kw: Kw, fname: &str) -> R<(Vec<u8>, String, bool)> {
    let b = it.bind_args(fname, a, kw, &["data", "errors", "final"], 1)?;
    let d = buf_arg(it, &b[0], true)?;
    let errors = errors_arg(it, &b[1], fname, 2)?;
    let final_ = final_arg(it, &b[2], true)?;
    Ok((d, errors, final_))
}

fn unicode_escape_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let (d, errors, final_) = escape_args(it, a, kw, "unicode_escape_decode")?;
    let (s, n) = codecs::unicode_escape_decode(it, &d, &errors, final_, false)?;
    Ok(pair_str(s, n))
}

fn raw_unicode_escape_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let (d, errors, final_) = escape_args(it, a, kw, "raw_unicode_escape_decode")?;
    let (s, n) = codecs::unicode_escape_decode(it, &d, &errors, final_, true)?;
    Ok(pair_str(s, n))
}

fn escape_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("escape_encode", a, kw, &["data", "errors"], 1)?;
    let d = match &b[0] {
        Some(Value::Obj(o)) if matches!(o.kind, Kind::Bytes(_)) => it.bytes_of(b[0].as_ref().unwrap_or(&Value::None))?,
        other => {
            let t = it.type_name_of(other.as_ref().unwrap_or(&Value::None));
            return Err(it.type_error(&format!("escape_encode() argument 1 must be bytes, not {}", t)));
        }
    };
    errors_arg(it, &b[1], "escape_encode", 2)?;
    let n = d.len() as i64;
    Ok(pair_bytes(codecs::escape_encode(&d), n))
}

fn escape_decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("escape_decode", a, kw, &["data", "errors"], 1)?;
    let d = buf_arg(it, &b[0], true)?;
    let errors = errors_arg(it, &b[1], "escape_decode", 2)?;
    match codecs::escape_decode(&d, &errors) {
        Ok(out) => Ok(pair_bytes(out, d.len() as i64)),
        Err(msg) => Err(it.value_error(&msg)),
    }
}

fn readbuffer_encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("readbuffer_encode", a, kw, &["data", "errors"], 1)?;
    let d = buf_arg(it, &b[0], true)?;
    errors_arg(it, &b[1], "readbuffer_encode", 2)?;
    let n = d.len() as i64;
    Ok(pair_bytes(d, n))
}

pub fn make(it: &mut Interp) -> Obj {
    let m = it.new_module("_codecs");
    let d = it.module_dict(&m);
    let fns: &[(&'static str, NativeFn)] = &[
        ("register", register),
        ("unregister", unregister),
        ("lookup", lookup),
        ("encode", encode),
        ("decode", decode),
        ("register_error", register_error),
        ("lookup_error", lookup_error),
        ("_unregister_error", unregister_error),
        ("_forget_codec", forget_codec),
        ("utf_8_encode", utf_8_encode),
        ("utf_8_decode", utf_8_decode),
        ("utf_7_encode", utf_7_encode),
        ("utf_7_decode", utf_7_decode),
        ("utf_16_encode", utf_16_encode),
        ("utf_16_decode", utf_16_decode),
        ("utf_16_le_encode", utf_16_le_encode),
        ("utf_16_le_decode", utf_16_le_decode),
        ("utf_16_be_encode", utf_16_be_encode),
        ("utf_16_be_decode", utf_16_be_decode),
        ("utf_16_ex_decode", utf_16_ex_decode),
        ("utf_32_encode", utf_32_encode),
        ("utf_32_decode", utf_32_decode),
        ("utf_32_le_encode", utf_32_le_encode),
        ("utf_32_le_decode", utf_32_le_decode),
        ("utf_32_be_encode", utf_32_be_encode),
        ("utf_32_be_decode", utf_32_be_decode),
        ("utf_32_ex_decode", utf_32_ex_decode),
        ("latin_1_encode", latin_1_encode),
        ("latin_1_decode", latin_1_decode),
        ("ascii_encode", ascii_encode),
        ("ascii_decode", ascii_decode),
        ("charmap_encode", charmap_encode),
        ("charmap_decode", charmap_decode),
        ("charmap_build", charmap_build),
        ("unicode_escape_encode", unicode_escape_encode),
        ("unicode_escape_decode", unicode_escape_decode),
        ("raw_unicode_escape_encode", raw_unicode_escape_encode),
        ("raw_unicode_escape_decode", raw_unicode_escape_decode),
        ("escape_encode", escape_encode),
        ("escape_decode", escape_decode),
        ("readbuffer_encode", readbuffer_encode),
    ];
    for (name, f) in fns {
        set_fn(it, &d, name, *f);
    }
    m
}

// ---- Unicode*Error ----

fn exc_self(it: &mut Interp, a: &[Value]) -> R<Obj> {
    match a.first() {
        Some(Value::Obj(o)) if matches!(o.kind, Kind::Exception(_)) => Ok(o.clone()),
        _ => Err(it.type_error("descriptor requires a 'BaseException' object")),
    }
}

fn arg_type_err(it: &mut Interp, n: usize, want: &str, v: &Value) -> Obj {
    let t = it.type_name_of(v);
    it.type_error(&format!("argument {} must be {}, not {}", n, want, t))
}

/// Shared `__init__`: `(encoding, object, start, end, reason)`, or without `encoding` for
/// `UnicodeTranslateError`.
fn unicode_init(it: &mut Interp, a: &[Value], kw: Kw, cls: &str, with_encoding: bool, bytes_object: bool) -> R<Value> {
    let e = exc_self(it, a)?;
    if !kw.is_empty() {
        return Err(it.type_error(&format!("{}() takes no keyword arguments", cls)));
    }
    let args = &a[1..];
    if let Kind::Exception(d) = &e.kind {
        d.borrow_mut().args = Value::tuple(args.to_vec());
    }
    let want = if with_encoding { 5 } else { 4 };
    if args.len() != want {
        return Err(it.type_error(&format!("function takes exactly {} arguments ({} given)", want, args.len())));
    }
    let off = with_encoding as usize;
    let encoding = if with_encoding {
        if args[0].as_str().is_none() {
            return Err(arg_type_err(it, 1, "str", &args[0]));
        }
        args[0].clone()
    } else {
        Value::None
    };
    let object = if bytes_object {
        match &args[off] {
            Value::Obj(o) if matches!(o.kind, Kind::Bytes(_)) => args[off].clone(),
            Value::Obj(o) if matches!(o.kind, Kind::ByteArray(_)) => Value::bytes(it.bytes_of(&args[off])?),
            v => {
                let t = it.type_name_of(v);
                return Err(it.type_error(&format!("a bytes-like object is required, not '{}'", t)));
            }
        }
    } else {
        if args[off].as_str().is_none() {
            return Err(arg_type_err(it, off + 1, "str", &args[off]));
        }
        args[off].clone()
    };
    let start = it.index_of(&args[off + 1])?;
    let end = it.index_of(&args[off + 2])?;
    if args[off + 3].as_str().is_none() {
        return Err(arg_type_err(it, off + 4, "str", &args[off + 3]));
    }
    let d = it.instance_dict(&e);
    dict_set_str(&d, "encoding", encoding);
    dict_set_str(&d, "object", object);
    dict_set_str(&d, "start", Value::Int(start));
    dict_set_str(&d, "end", Value::Int(end));
    dict_set_str(&d, "reason", args[off + 3].clone());
    Ok(Value::None)
}

fn encode_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    unicode_init(it, a, kw, "UnicodeEncodeError", true, false)
}

fn decode_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    unicode_init(it, a, kw, "UnicodeDecodeError", true, true)
}

fn translate_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    unicode_init(it, a, kw, "UnicodeTranslateError", false, false)
}

pub(crate) fn char_escape(cp: u32) -> String {
    if cp <= 0xff {
        format!("\\x{:02x}", cp)
    } else if cp <= 0xffff {
        format!("\\u{:04x}", cp)
    } else {
        format!("\\U{:08x}", cp)
    }
}

#[derive(PartialEq)]
enum StrKind {
    Encode,
    Decode,
    Translate,
}

fn unicode_str(it: &mut Interp, a: &[Value], kind: StrKind) -> R<Value> {
    let e = exc_self(it, a)?;
    let d = it.instance_dict(&e);
    let Some(object) = dict_get_str(&d, "object") else { return Ok(Value::str("")) };
    let get = |n: &str| dict_get_str(&d, n).unwrap_or(Value::None);
    let (sv, ev, rv, encv) = (get("start"), get("end"), get("reason"), get("encoding"));
    let start = it.index_of(&sv)?;
    let end = it.index_of(&ev)?;
    let reason = it.str_of(&rv)?;
    let prefix = if kind == StrKind::Translate { "can't translate".to_string() } else { format!("'{}' codec can't {}", it.str_of(&encv)?, if kind == StrKind::Encode { "encode" } else { "decode" }) };
    let single = if kind == StrKind::Decode {
        let b = it.bytes_of(&object)?;
        (start >= 0 && (start as usize) < b.len() && end == start + 1).then(|| format!("byte 0x{:02x}", b[start as usize]))
    } else {
        let c = object.as_pystr().filter(|s| start >= 0 && (start as usize) < s.nchars && end == start + 1).and_then(|s| s.char_at(start as usize));
        c.map(|c| format!("character '{}'", char_escape(c)))
    };
    let msg = match single {
        Some(what) => format!("{} {} in position {}: {}", prefix, what, start, reason),
        None => {
            let what = if kind == StrKind::Decode { "bytes" } else { "characters" };
            format!("{} {} in position {}-{}: {}", prefix, what, start, end - 1, reason)
        }
    };
    Ok(Value::string(msg))
}

fn encode_str(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    unicode_str(it, a, StrKind::Encode)
}

fn decode_str(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    unicode_str(it, a, StrKind::Decode)
}

fn translate_str(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    unicode_str(it, a, StrKind::Translate)
}

pub fn init(it: &mut Interp) {
    let pairs: [(&str, NativeFn, NativeFn); 3] = [
        ("UnicodeEncodeError", encode_init, encode_str),
        ("UnicodeDecodeError", decode_init, decode_str),
        ("UnicodeTranslateError", translate_init, translate_str),
    ];
    for (name, init, str_) in pairs {
        let t = it.exc_type(name);
        it.reg(&t, "__init__", init);
        it.reg(&t, "__str__", str_);
    }
}
