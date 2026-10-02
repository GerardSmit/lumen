//! The `_codecs` module, on top of `crate::codecs`.

use crate::bind::{PyCx, PyHost};
use crate::object::*;
use lumen_bind::{FromArg, Slot};

/// A `str` argument with its length in code points.
pub struct Text<'a> {
    s: &'a str,
    n: usize,
}

impl<'a> FromArg<'a, PyHost> for Text<'a> {
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v.as_pystr() {
            Some(s) => Ok(Text { s: &s.s[..], n: s.nchars }),
            None => Err(cx.arg_error(at, "str", v)),
        }
    }
}

/// An `errors` argument: a `str`, or `None` for `"strict"` (behind `Option`).
pub struct Errors<'a>(&'a str);

impl<'a> FromArg<'a, PyHost> for Errors<'a> {
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v.as_str() {
            Some(s) => Ok(Errors(s)),
            None => Err(cx.arg_error(at, "str or None", v)),
        }
    }
}

fn errors(e: Option<Errors<'_>>) -> &str {
    e.map_or("strict", |e| e.0)
}

/// A bytes-like argument that may also be a `str`, read as UTF-8 (CPython's `s*`).
fn text_or_bytes(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
    if let Value::Obj(o) = v {
        match &o.kind {
            Kind::Bytes(b) => return Ok(b.clone()),
            Kind::ByteArray(b) => return Ok(b.to_vec()),
            Kind::Str(s) => return Ok(s.s.as_bytes().to_vec()),
            _ => {}
        }
    }
    let t = it.type_name_of(v);
    Err(it.type_error(&format!("a bytes-like object is required, not '{}'", t)))
}

fn pair_str(s: String, n: usize) -> Value {
    Value::tuple(vec![Value::string(s), Value::Int(n as i64)])
}

fn pair_bytes(b: Vec<u8>, n: usize) -> Value {
    Value::tuple(vec![Value::bytes(b), Value::Int(n as i64)])
}

fn utf16_32_encode(it: &mut Interp, s: Text<'_>, errors: &str, byteorder: i32, wide: bool) -> R<Value> {
    let out = if wide { codecs::utf32_encode(it, s.s, errors, byteorder)? } else { codecs::utf16_encode(it, s.s, errors, byteorder)? };
    Ok(pair_bytes(out, s.n))
}

fn utf16_32_decode(it: &mut Interp, data: &[u8], errors: &str, byteorder: &mut i32, final_: bool, wide: bool) -> R<(String, usize)> {
    if wide {
        codecs::utf32_decode(it, data, errors, byteorder, final_)
    } else {
        codecs::utf16_decode(it, data, errors, byteorder, final_)
    }
}

use crate::codecs;
use crate::vm::Interp;

#[lumen_bind::module(name = "_codecs")]
pub mod _codecs {
    use super::*;

    /// Register a codec search function.
    ///
    /// Search functions are expected to take one argument, the encoding name in
    /// all lower case letters, and either return None, or a tuple of functions
    /// (encoder, decoder, stream_reader, stream_writer) (or a CodecInfo object).
    #[op]
    fn register(it: &mut Interp, search_function: &Value) -> R<()> {
        codecs::register(it, search_function.clone())
    }

    /// Unregister a codec search function and clear the registry's cache.
    ///
    /// If the search function is not registered, do nothing.
    #[op]
    fn unregister(it: &mut Interp, search_function: &Value) {
        codecs::unregister(it, search_function);
    }

    /// Looks up a codec tuple in the Python codec registry and returns a CodecInfo object.
    #[op]
    fn lookup(it: &mut Interp, encoding: &str) -> R<Value> {
        codecs::lookup(it, encoding)
    }

    /// Encodes obj using the codec registered for encoding.
    ///
    /// The default encoding is 'utf-8'.  errors may be given to set a
    /// different error handling scheme.  Default is 'strict' meaning that encoding
    /// errors raise a ValueError.  Other possible values are 'ignore', 'replace'
    /// and 'backslashreplace' as well as any other name registered with
    /// codecs.register_error that can handle ValueErrors.
    #[op]
    fn encode(it: &mut Interp, #[kw] obj: &Value, #[kw] #[default("utf-8")] encoding: &str, #[kw] #[default("strict")] errors: &str) -> R<Value> {
        codecs::encode_obj(it, obj, encoding, errors)
    }

    /// Decodes obj using the codec registered for encoding.
    ///
    /// Default encoding is 'utf-8'.  errors may be given to set a
    /// different error handling scheme.  Default is 'strict' meaning that encoding
    /// errors raise a ValueError.  Other possible values are 'ignore', 'replace'
    /// and 'backslashreplace' as well as any other name registered with
    /// codecs.register_error that can handle ValueErrors.
    #[op]
    fn decode(it: &mut Interp, #[kw] obj: &Value, #[kw] #[default("utf-8")] encoding: &str, #[kw] #[default("strict")] errors: &str) -> R<Value> {
        codecs::decode_obj(it, obj, encoding, errors)
    }

    /// Register the specified error handler under the name errors.
    ///
    /// handler must be a callable object, that will be called with an exception
    /// instance containing information about the location of the encoding/decoding
    /// error and must return a (replacement, new position) tuple.
    #[op]
    fn register_error(it: &mut Interp, errors: &str, handler: &Value) -> R<()> {
        codecs::register_error(it, errors, handler.clone())
    }

    /// Un-register the specified error handler for the error handling `errors'.
    ///
    /// Only custom error handlers can be un-registered. An exception is raised
    /// if the error handling is a built-in one (e.g., 'strict'), or if an error
    /// occurs.
    ///
    /// Otherwise, this returns True if a custom handler has been successfully
    /// un-registered, and False if no custom handler for the specified error
    /// handling exists.
    #[op]
    fn _unregister_error(it: &mut Interp, errors: &str) -> R<bool> {
        codecs::unregister_error(it, errors)
    }

    /// lookup_error(errors) -> handler
    ///
    /// Return the error handler for the specified error handling name or raise a
    /// LookupError, if no handler exists under this name.
    #[op]
    fn lookup_error(it: &mut Interp, name: &str) -> R<Value> {
        codecs::lookup_error(it, name)
    }

    #[op]
    fn utf_8_encode(it: &mut Interp, str: Text<'_>, errors: Option<Errors<'_>>) -> R<Value> {
        Ok(pair_bytes(codecs::utf8_encode(it, str.s, super::errors(errors))?, str.n))
    }

    #[op]
    fn utf_8_decode(it: &mut Interp, data: &[u8], errors: Option<Errors<'_>>, #[default(false)] r#final: bool) -> R<Value> {
        let (s, n) = codecs::utf8_decode(it, data, super::errors(errors), r#final)?;
        Ok(pair_str(s, n))
    }

    #[op]
    fn utf_7_encode(str: Text<'_>, _errors: Option<Errors<'_>>) -> Value {
        pair_bytes(codecs::utf7_encode(str.s), str.n)
    }

    #[op]
    fn utf_7_decode(it: &mut Interp, data: &[u8], errors: Option<Errors<'_>>, #[default(false)] r#final: bool) -> R<Value> {
        let (s, n) = codecs::utf7_decode(it, data, super::errors(errors), r#final)?;
        Ok(pair_str(s, n))
    }

    #[op]
    fn utf_16_encode(it: &mut Interp, str: Text<'_>, errors: Option<Errors<'_>>, #[default(0)] byteorder: i32) -> R<Value> {
        utf16_32_encode(it, str, super::errors(errors), byteorder.clamp(-1, 1), false)
    }

    #[op]
    fn utf_16_le_encode(it: &mut Interp, str: Text<'_>, errors: Option<Errors<'_>>) -> R<Value> {
        utf16_32_encode(it, str, super::errors(errors), -1, false)
    }

    #[op]
    fn utf_16_be_encode(it: &mut Interp, str: Text<'_>, errors: Option<Errors<'_>>) -> R<Value> {
        utf16_32_encode(it, str, super::errors(errors), 1, false)
    }

    #[op]
    fn utf_32_encode(it: &mut Interp, str: Text<'_>, errors: Option<Errors<'_>>, #[default(0)] byteorder: i32) -> R<Value> {
        utf16_32_encode(it, str, super::errors(errors), byteorder.clamp(-1, 1), true)
    }

    #[op]
    fn utf_32_le_encode(it: &mut Interp, str: Text<'_>, errors: Option<Errors<'_>>) -> R<Value> {
        utf16_32_encode(it, str, super::errors(errors), -1, true)
    }

    #[op]
    fn utf_32_be_encode(it: &mut Interp, str: Text<'_>, errors: Option<Errors<'_>>) -> R<Value> {
        utf16_32_encode(it, str, super::errors(errors), 1, true)
    }

    #[op]
    fn utf_16_decode(it: &mut Interp, data: &[u8], errors: Option<Errors<'_>>, #[default(false)] r#final: bool) -> R<Value> {
        let (s, n) = utf16_32_decode(it, data, super::errors(errors), &mut 0, r#final, false)?;
        Ok(pair_str(s, n))
    }

    #[op]
    fn utf_16_le_decode(it: &mut Interp, data: &[u8], errors: Option<Errors<'_>>, #[default(false)] r#final: bool) -> R<Value> {
        let (s, n) = utf16_32_decode(it, data, super::errors(errors), &mut -1, r#final, false)?;
        Ok(pair_str(s, n))
    }

    #[op]
    fn utf_16_be_decode(it: &mut Interp, data: &[u8], errors: Option<Errors<'_>>, #[default(false)] r#final: bool) -> R<Value> {
        let (s, n) = utf16_32_decode(it, data, super::errors(errors), &mut 1, r#final, false)?;
        Ok(pair_str(s, n))
    }

    #[op]
    fn utf_32_decode(it: &mut Interp, data: &[u8], errors: Option<Errors<'_>>, #[default(false)] r#final: bool) -> R<Value> {
        let (s, n) = utf16_32_decode(it, data, super::errors(errors), &mut 0, r#final, true)?;
        Ok(pair_str(s, n))
    }

    #[op]
    fn utf_32_le_decode(it: &mut Interp, data: &[u8], errors: Option<Errors<'_>>, #[default(false)] r#final: bool) -> R<Value> {
        let (s, n) = utf16_32_decode(it, data, super::errors(errors), &mut -1, r#final, true)?;
        Ok(pair_str(s, n))
    }

    #[op]
    fn utf_32_be_decode(it: &mut Interp, data: &[u8], errors: Option<Errors<'_>>, #[default(false)] r#final: bool) -> R<Value> {
        let (s, n) = utf16_32_decode(it, data, super::errors(errors), &mut 1, r#final, true)?;
        Ok(pair_str(s, n))
    }

    #[op(hint(py(text_signature = "($module, data, errors=None, byteorder=0, final=False,\n                 /)")))]
    fn utf_16_ex_decode(
        it: &mut Interp,
        data: &[u8],
        errors: Option<Errors<'_>>,
        #[default(0)] byteorder: i32,
        #[default(false)] r#final: bool,
    ) -> R<Value> {
        let mut bo = byteorder.clamp(-1, 1);
        let (s, n) = utf16_32_decode(it, data, super::errors(errors), &mut bo, r#final, false)?;
        Ok(Value::tuple(vec![Value::string(s), Value::Int(n as i64), Value::Int(bo as i64)]))
    }

    #[op(hint(py(text_signature = "($module, data, errors=None, byteorder=0, final=False,\n                 /)")))]
    fn utf_32_ex_decode(
        it: &mut Interp,
        data: &[u8],
        errors: Option<Errors<'_>>,
        #[default(0)] byteorder: i32,
        #[default(false)] r#final: bool,
    ) -> R<Value> {
        let mut bo = byteorder.clamp(-1, 1);
        let (s, n) = utf16_32_decode(it, data, super::errors(errors), &mut bo, r#final, true)?;
        Ok(Value::tuple(vec![Value::string(s), Value::Int(n as i64), Value::Int(bo as i64)]))
    }

    #[op]
    fn latin_1_encode(it: &mut Interp, str: Text<'_>, errors: Option<Errors<'_>>) -> R<Value> {
        Ok(pair_bytes(codecs::latin1_encode(it, str.s, super::errors(errors))?, str.n))
    }

    #[op]
    fn latin_1_decode(data: &[u8], _errors: Option<Errors<'_>>) -> Value {
        pair_str(codecs::latin1_decode(data), data.len())
    }

    #[op]
    fn ascii_encode(it: &mut Interp, str: Text<'_>, errors: Option<Errors<'_>>) -> R<Value> {
        Ok(pair_bytes(codecs::ascii_encode(it, str.s, super::errors(errors))?, str.n))
    }

    #[op]
    fn ascii_decode(it: &mut Interp, data: &[u8], errors: Option<Errors<'_>>) -> R<Value> {
        let s = codecs::ascii_decode(it, data, super::errors(errors))?;
        Ok(pair_str(s, data.len()))
    }

    #[op]
    fn charmap_encode(it: &mut Interp, str: Text<'_>, errors: Option<Errors<'_>>, mapping: Option<&Value>) -> R<Value> {
        let out = codecs::charmap_encode(it, str.s, super::errors(errors), mapping.unwrap_or(&Value::None))?;
        Ok(pair_bytes(out, str.n))
    }

    #[op]
    fn charmap_decode(it: &mut Interp, data: &[u8], errors: Option<Errors<'_>>, mapping: Option<&Value>) -> R<Value> {
        let s = codecs::charmap_decode(it, data, super::errors(errors), mapping.unwrap_or(&Value::None))?;
        Ok(pair_str(s, data.len()))
    }

    #[op]
    fn charmap_build(it: &mut Interp, map: &str) -> R<Value> {
        codecs::charmap_build(it, map)
    }

    #[op]
    fn unicode_escape_encode(str: Text<'_>, _errors: Option<Errors<'_>>) -> Value {
        pair_bytes(codecs::unicode_escape_encode(str.s, false), str.n)
    }

    #[op]
    fn raw_unicode_escape_encode(str: Text<'_>, _errors: Option<Errors<'_>>) -> Value {
        pair_bytes(codecs::unicode_escape_encode(str.s, true), str.n)
    }

    #[op]
    fn unicode_escape_decode(it: &mut Interp, data: &Value, errors: Option<Errors<'_>>, #[default(true)] r#final: bool) -> R<Value> {
        let d = text_or_bytes(it, data)?;
        let (s, n) = codecs::unicode_escape_decode(it, &d, super::errors(errors), r#final, false)?;
        Ok(pair_str(s, n))
    }

    #[op]
    fn raw_unicode_escape_decode(it: &mut Interp, data: &Value, errors: Option<Errors<'_>>, #[default(true)] r#final: bool) -> R<Value> {
        let d = text_or_bytes(it, data)?;
        let (s, n) = codecs::unicode_escape_decode(it, &d, super::errors(errors), r#final, true)?;
        Ok(pair_str(s, n))
    }

    #[op]
    fn escape_encode(it: &mut Interp, data: &Value, _errors: Option<Errors<'_>>) -> R<Value> {
        let d = match data {
            Value::Obj(o) if matches!(o.kind, Kind::Bytes(_)) => it.bytes_of(data)?,
            _ => {
                let t = it.type_name_of(data);
                return Err(it.type_error(&format!("escape_encode() argument 1 must be bytes, not {}", t)));
            }
        };
        let n = d.len();
        Ok(pair_bytes(codecs::escape_encode(&d), n))
    }

    #[op]
    fn escape_decode(it: &mut Interp, data: &Value, errors: Option<Errors<'_>>) -> R<Value> {
        let d = text_or_bytes(it, data)?;
        match codecs::escape_decode(&d, super::errors(errors)) {
            Ok(out) => Ok(pair_bytes(out, d.len())),
            Err(msg) => Err(it.value_error(&msg)),
        }
    }

    #[op]
    fn readbuffer_encode(it: &mut Interp, data: &Value, _errors: Option<Errors<'_>>) -> R<Value> {
        let d = text_or_bytes(it, data)?;
        let n = d.len();
        Ok(pair_bytes(d, n))
    }
}
