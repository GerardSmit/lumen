//! `bytes` and `bytearray`.

use super::slots::{reg_binops, reg_compare, reg_slots};
use crate::bind::{install_all, KwArgs, PyCx, PyHost, This};
use crate::object::*;
use crate::vm::*;
use lumen_bind::{FromArg, Passed, Slot};
use lumen_common::search;
use std::borrow::Cow;
use std::rc::Rc;

/// A `bytes` or `bytearray` receiver, with its contents (a `bytearray` is copied, so methods may
/// run Python code while they hold the data).
pub struct ByteSelf<'a> {
    v: &'a Value,
    d: Cow<'a, [u8]>,
}

impl<'a> FromArg<'a, PyHost> for ByteSelf<'a> {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        if let Value::Obj(o) = v {
            match &o.kind {
                Kind::Bytes(b) => {
                    return Ok(ByteSelf {
                        v,
                        d: Cow::Borrowed(b),
                    });
                }
                Kind::ByteArray(b) => {
                    return Ok(ByteSelf {
                        v,
                        d: Cow::Owned(b.to_vec()),
                    });
                }
                _ => {}
            }
        }
        Err(cx.arg_error(at, "bytes", v))
    }
}

impl ByteSelf<'_> {
    /// `b` as the receiver's kind: `bytes` for a `bytes`, `bytearray` for a `bytearray`.
    fn wrap(&self, b: Vec<u8>) -> Value {
        wrap(self.v, b)
    }
}

/// A `bytearray` (or subclass instance): the receiver of the mutating methods.
#[derive(Clone, Copy)]
pub struct BaRef<'a>(&'a ByteStore);

impl<'a> FromArg<'a, PyHost> for BaRef<'a> {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        if let Value::Obj(o) = v {
            if let Kind::ByteArray(b) = &o.kind {
                return Ok(BaRef(b));
            }
        }
        Err(cx.arg_error(at, "bytearray", v))
    }
}

/// The `fillchar` of `ljust` / `rjust` / `center`: a `bytes` or `bytearray` of length 1.
#[derive(Clone, Copy)]
pub struct FillByte(u8);

impl<'a> FromArg<'a, PyHost> for FillByte {
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        if let Value::Obj(o) = v {
            match &o.kind {
                Kind::Bytes(b) if b.len() == 1 => return Ok(FillByte(b[0])),
                Kind::ByteArray(b) if b.len() == 1 => return Ok(FillByte(b.to_vec()[0])),
                _ => {}
            }
        }
        Err(cx.arg_error(at, "a byte string of length 1", v))
    }
}

fn is_array(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if matches!(o.kind, Kind::ByteArray(_)))
}

fn wrap(like: &Value, b: Vec<u8>) -> Value {
    if is_array(like) {
        Value::Obj(Object::new(Kind::ByteArray(ba_store(b))))
    } else {
        Value::bytes(b)
    }
}

fn zeroed(it: &mut Interp, n: usize) -> R<Vec<u8>> {
    let mut v = it.vec_with_capacity(n, crate::limits::MAX_BYTES_LEN)?;
    v.resize(n, 0);
    Ok(v)
}

/// The contents `bytes(source, encoding, errors)` / `bytearray(...)` describe.
fn build(
    it: &mut Interp,
    source: Passed<&Value>,
    encoding: Passed<&str>,
    errors: Passed<&str>,
) -> R<Vec<u8>> {
    let Some(src) = source.0 else {
        if encoding.0.is_some() {
            return Err(it.type_error("encoding without a string argument"));
        }
        if errors.0.is_some() {
            return Err(it.type_error("errors without a string argument"));
        }
        return Ok(Vec::new());
    };
    if let Some(s) = src.as_str() {
        let Some(enc) = encoding.0 else {
            return Err(it.type_error("string argument without an encoding"));
        };
        return it.encode_str(s, enc, errors.0.unwrap_or("strict"));
    }
    if encoding.0.is_some() {
        return Err(it.type_error("encoding without a string argument"));
    }
    if errors.0.is_some() {
        return Err(it.type_error("errors without a string argument"));
    }
    match src {
        Value::Int(n) => {
            if *n < 0 {
                return Err(it.value_error("negative count"));
            }
            zeroed(it, *n as usize)
        }
        Value::Bool(n) => zeroed(it, *n as usize),
        Value::Obj(o) if matches!(o.kind, Kind::Int(_)) => {
            Err(it.overflow_err("cannot fit 'int' into an index-sized integer"))
        }
        _ => it.bytes_from_object(src),
    }
}

/// Where `n` first (or, with `rev`, last) occurs in `h` at or after `from`.
fn find_sub(h: &[u8], n: &[u8], from: usize, rev: bool) -> Option<usize> {
    if rev {
        Some(search::rfind(h.get(from..)?, n)? + from)
    } else {
        search::find_from(h, n, from)
    }
}

/// The `sub` of `find` and friends: a byte value or a bytes-like object.
fn sub_arg(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
    if v.is_int_like() {
        let n = it.index_of(v)?;
        if !(0..256).contains(&n) {
            return Err(it.value_error("byte must be in range(0, 256)"));
        }
        return Ok(vec![n as u8]);
    }
    if it.is_buffer(v) {
        return it.bytes_of(v);
    }
    let t = it.type_name_of(v);
    Err(it.type_error(&format!(
        "argument should be integer or bytes-like object, not '{t}'"
    )))
}

/// The `start` / `end` of `find` and friends (`_PyEval_SliceIndex`), clamped to `0..=len`.
fn opt_range(
    it: &mut Interp,
    start: Option<&Value>,
    end: Option<&Value>,
    len: usize,
) -> R<(usize, usize)> {
    let len = len as i64;
    let norm = |it: &mut Interp, v: Option<&Value>, d: i64| -> R<i64> {
        match v {
            None | Some(Value::None) => Ok(d),
            Some(v) if !it.has_index(v) => Err(
                it.type_error("slice indices must be integers or None or have an __index__ method")
            ),
            Some(v) => {
                let i = it.slice_index(v)?;
                Ok(if i < 0 {
                    i.saturating_add(len).max(0)
                } else {
                    i.min(len)
                })
            }
        }
    };
    let s = norm(it, start, 0)?;
    let e = norm(it, end, len)?;
    Ok((s as usize, e as usize))
}

fn find_impl(
    it: &mut Interp,
    h: &[u8],
    sub: &Value,
    start: Option<&Value>,
    end: Option<&Value>,
    rev: bool,
) -> R<Option<usize>> {
    let n = sub_arg(it, sub)?;
    let (s, e) = opt_range(it, start, end, h.len())?;
    if s > e {
        return Ok(None);
    }
    Ok(find_sub(&h[..e], &n, s, rev))
}

fn affix(
    it: &mut Interp,
    h: &[u8],
    prefix: &Value,
    start: Option<&Value>,
    end: Option<&Value>,
    name: &str,
    at_start: bool,
) -> R<bool> {
    let cands = match prefix.tuple_items() {
        Some(t) => t.to_vec(),
        None if it.is_buffer(prefix) => vec![prefix.clone()],
        None => {
            let t = it.type_name_of(prefix);
            return Err(it.type_error(&format!(
                "{name} first arg must be bytes or a tuple of bytes, not {t}"
            )));
        }
    };
    let (s, e) = opt_range(it, start, end, h.len())?;
    let hay = if s > e { None } else { Some(&h[s..e]) };
    for c in cands {
        let n = it.buffer_bytes(&c)?;
        if hay.is_some_and(|h| {
            if at_start {
                h.starts_with(&n)
            } else {
                h.ends_with(&n)
            }
        }) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn strip_impl(slf: &ByteSelf<'_>, chars: Option<&[u8]>, left: bool, right: bool) -> Value {
    let h = &slf.d;
    let strip = |b: u8| match chars {
        Some(c) => c.contains(&b),
        None => is_ws(b),
    };
    let mut s = 0;
    let mut e = h.len();
    if left {
        while s < e && strip(h[s]) {
            s += 1;
        }
    }
    if right {
        while e > s && strip(h[e - 1]) {
            e -= 1;
        }
    }
    slf.wrap(h[s..e].to_vec())
}

fn split_impl(
    it: &mut Interp,
    slf: &ByteSelf<'_>,
    sep: Option<&[u8]>,
    max: isize,
    rev: bool,
) -> R<Value> {
    let h: &[u8] = &slf.d;
    let max = max as i64;
    let mut parts: Vec<&[u8]> = Vec::new();
    match sep {
        Some(sep) => {
            if sep.is_empty() {
                return Err(it.value_error("empty separator"));
            }
            if !rev {
                let (mut i, mut n) = (0, max);
                while n != 0 {
                    match find_sub(h, sep, i, false) {
                        Some(p) => {
                            parts.push(&h[i..p]);
                            i = p + sep.len();
                            n -= 1;
                        }
                        None => break,
                    }
                }
                parts.push(&h[i..]);
            } else {
                let (mut e, mut n) = (h.len(), max);
                while n != 0 {
                    match find_sub(&h[..e], sep, 0, true) {
                        Some(p) => {
                            parts.push(&h[p + sep.len()..e]);
                            e = p;
                            n -= 1;
                        }
                        None => break,
                    }
                }
                parts.push(&h[..e]);
                parts.reverse();
            }
        }
        None => {
            if !rev {
                let mut i = 0;
                let mut n = max;
                loop {
                    while i < h.len() && is_ws(h[i]) {
                        i += 1;
                    }
                    if i >= h.len() {
                        break;
                    }
                    if n == 0 {
                        let mut e = h.len();
                        while e > i && is_ws(h[e - 1]) {
                            e -= 1;
                        }
                        parts.push(&h[i..e]);
                        break;
                    }
                    let s = i;
                    while i < h.len() && !is_ws(h[i]) {
                        i += 1;
                    }
                    parts.push(&h[s..i]);
                    n -= 1;
                }
            } else {
                let mut e = h.len();
                let mut n = max;
                loop {
                    while e > 0 && is_ws(h[e - 1]) {
                        e -= 1;
                    }
                    if e == 0 {
                        break;
                    }
                    if n == 0 {
                        let mut s = 0;
                        while s < e && is_ws(h[s]) {
                            s += 1;
                        }
                        parts.push(&h[s..e]);
                        break;
                    }
                    let end = e;
                    while e > 0 && !is_ws(h[e - 1]) {
                        e -= 1;
                    }
                    parts.push(&h[e..end]);
                    n -= 1;
                }
                parts.reverse();
            }
        }
    }
    Ok(Value::list(
        parts.into_iter().map(|p| slf.wrap(p.to_vec())).collect(),
    ))
}

fn splitlines_impl(slf: &ByteSelf<'_>, keepends: bool) -> Value {
    let h = &slf.d;
    let mut out = Vec::new();
    let mut s = 0;
    let mut i = 0;
    while i < h.len() {
        if h[i] == b'\n' || h[i] == b'\r' {
            let mut e = i + 1;
            if h[i] == b'\r' && e < h.len() && h[e] == b'\n' {
                e += 1;
            }
            out.push(slf.wrap(h[s..if keepends { e } else { i }].to_vec()));
            s = e;
            i = e;
        } else {
            i += 1;
        }
    }
    if s < h.len() {
        out.push(slf.wrap(h[s..].to_vec()));
    }
    Value::list(out)
}

fn partition_impl(it: &mut Interp, slf: &ByteSelf<'_>, sep: &[u8], rev: bool) -> R<Value> {
    let h = &slf.d;
    if sep.is_empty() {
        return Err(it.value_error("empty separator"));
    }
    let w = |b: &[u8]| slf.wrap(b.to_vec());
    Ok(match find_sub(h, sep, 0, rev) {
        Some(p) => Value::tuple(vec![w(&h[..p]), w(sep), w(&h[p + sep.len()..])]),
        None if rev => Value::tuple(vec![w(&[]), w(&[]), w(h)]),
        None => Value::tuple(vec![w(h), w(&[]), w(&[])]),
    })
}

fn join_impl(it: &mut Interp, slf: &ByteSelf<'_>, iterable: &Value) -> R<Value> {
    let sep = &slf.d;
    let items = match iterable {
        Value::Obj(o) if o.cls.is_none() && matches!(o.kind, Kind::List(_) | Kind::Tuple(_)) => {
            it.iterate_to_vec(iterable)?
        }
        _ => {
            let iter = match it.get_iter(iterable) {
                Ok(i) => i,
                Err(e) if it.exc_is(&e, "TypeError") => {
                    return Err(it.type_error("can only join an iterable"));
                }
                Err(e) => return Err(e),
            };
            it.iterate_to_vec(&iter)?
        }
    };
    let mut total = sep.len().saturating_mul(items.len().saturating_sub(1));
    for v in &items {
        if let Value::Obj(o) = v {
            total = total.saturating_add(match &o.kind {
                Kind::Bytes(b) => b.len(),
                Kind::ByteArray(b) => b.len(),
                _ => 0,
            });
        }
    }
    let mut out = it.vec_with_capacity(total, crate::limits::MAX_BYTES_LEN)?;
    for (i, v) in items.iter().enumerate() {
        if i & 0x3ff == 0 {
            it.poll()?;
        }
        if i > 0 {
            out.extend_from_slice(sep);
        }
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Bytes(_)) => {
                if let Kind::Bytes(b) = &o.kind {
                    out.extend_from_slice(b);
                }
            }
            _ if it.is_buffer(v) => out.extend(it.bytes_of(v)?),
            _ => {
                let t = it.type_name_of(v);
                return Err(it.type_error(&format!(
                    "sequence item {}: expected a bytes-like object, {} found",
                    i, t
                )));
            }
        }
    }
    Ok(slf.wrap(out))
}

fn hex_impl(it: &mut Interp, d: &[u8], sep: Passed<&Value>, bytes_per_sep: isize) -> R<String> {
    let sep = super::memview::hex_sep_arg(it, sep.0)?;
    Ok(lumen_common::codec::hex_encode_sep(
        d,
        sep,
        bytes_per_sep as i64,
    ))
}

/// `bytes.fromhex(string)` / `bytearray.fromhex(string)` for class `cls` (a subclass is called
/// with the exact-type result, as CPython does).
fn fromhex_impl(it: &mut Interp, cls: &Value, string: &Value, array: bool) -> R<Value> {
    let digits: Vec<u32> = if let Some(s) = string.as_str() {
        let cps: Vec<u32> = lumen_common::smuggle::code_points(s).collect();
        if let Some(at) = cps.iter().position(|&c| c >= 128) {
            return Err(it.value_error(&format!(
                "non-hexadecimal number found in fromhex() arg at position {at}"
            )));
        }
        cps
    } else if it.is_buffer(string) {
        it.bytes_of(string)?.into_iter().map(u32::from).collect()
    } else {
        let t = it.type_name_of(string);
        return Err(it.type_error(&format!(
            "fromhex() argument must be str or bytes-like, not {t}"
        )));
    };
    let is_space = |c: u32| matches!(c, 0x20 | 0x09..=0x0d);
    let digit = |c: u32| {
        char::from_u32(c)
            .and_then(|c| c.to_digit(16))
            .filter(|_| c < 128)
    };
    let mut out = Vec::with_capacity(digits.len() / 2);
    let mut i = 0;
    loop {
        while i < digits.len() && is_space(digits[i]) {
            i += 1;
        }
        if i >= digits.len() {
            break;
        }
        let Some(hi) = digit(digits[i]) else {
            return Err(it.value_error(&format!(
                "non-hexadecimal number found in fromhex() arg at position {i}"
            )));
        };
        i += 1;
        if i >= digits.len() {
            return Err(
                it.value_error("fromhex() arg must contain an even number of hexadecimal digits")
            );
        }
        let Some(lo) = digit(digits[i]) else {
            return Err(it.value_error(&format!(
                "non-hexadecimal number found in fromhex() arg at position {i}"
            )));
        };
        i += 1;
        out.push((hi * 16 + lo) as u8);
    }
    let exact = if array {
        Value::Obj(Object::new(Kind::ByteArray(ba_store(out))))
    } else {
        Value::bytes(out)
    };
    let base = if array {
        &it.types.bytearray
    } else {
        &it.types.bytes
    };
    match cls {
        Value::Obj(c) if !Rc::ptr_eq(c, base) => it.call(cls, vec![exact], Vec::new()),
        _ => Ok(exact),
    }
}

fn justify(
    it: &mut Interp,
    slf: &ByteSelf<'_>,
    width: isize,
    fill: Passed<FillByte>,
    mode: u8,
) -> R<Value> {
    let d = &slf.d;
    let w = width.max(0) as usize;
    let fill = fill.0.map_or(b' ', |f| f.0);
    if d.len() >= w {
        return Ok(slf.wrap(d.to_vec()));
    }
    it.check_bytes_len(w)?;
    let pad = w - d.len();
    let (l, r) = match mode {
        0 => (0, pad),
        1 => (pad, 0),
        _ => {
            let left = pad / 2 + (pad & w & 1);
            (left, pad - left)
        }
    };
    let mut out = vec![fill; l];
    out.extend_from_slice(d);
    out.extend(std::iter::repeat_n(fill, r));
    Ok(slf.wrap(out))
}

/// `bytearray.__reduce_ex__(proto)`: a latin-1 `str` below protocol 3 (for Python 2), else bytes.
fn reduce_impl(it: &mut Interp, slf: &ByteSelf<'_>, proto: i32) -> Value {
    let Value::Obj(o) = slf.v else {
        unreachable!("a bytearray")
    };
    let state = o.dict.borrow().clone().map_or(Value::None, Value::Obj);
    let args = if proto < 3 {
        vec![
            Value::string(slf.d.iter().map(|&b| b as char).collect::<String>()),
            Value::str("latin-1"),
        ]
    } else if slf.d.is_empty() {
        Vec::new()
    } else {
        vec![Value::bytes(slf.d.to_vec())]
    };
    Value::tuple(vec![
        Value::Obj(it.type_of(slf.v)),
        Value::tuple(args),
        state,
    ])
}

fn byte_val(it: &mut Interp, v: &Value) -> R<u8> {
    let n = it.index_of(v)?;
    if !(0..256).contains(&n) {
        return Err(it.value_error("byte must be in range(0, 256)"));
    }
    Ok(n as u8)
}

// The methods `bytes` and `bytearray` share (same signature and docs).
#[lumen_bind::class(name = "bytes", hint(py(shared)))]
pub struct ByteMethods;

#[lumen_bind::methods]
impl ByteMethods {
    /// B.find(sub[, start[, end]]) -> int
    ///
    /// Return the lowest index in B where subsection sub is found,
    /// such that sub is contained within B[start,end].  Optional
    /// arguments start and end are interpreted as in slice notation.
    ///
    /// Return -1 on failure.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn find(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        sub: &Value,
        start: Option<&Value>,
        end: Option<&Value>,
    ) -> R<i64> {
        Ok(find_impl(it, &slf.0.d, sub, start, end, false)?.map_or(-1, |i| i as i64))
    }

    /// B.rfind(sub[, start[, end]]) -> int
    ///
    /// Return the highest index in B where subsection sub is found,
    /// such that sub is contained within B[start,end].  Optional
    /// arguments start and end are interpreted as in slice notation.
    ///
    /// Return -1 on failure.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn rfind(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        sub: &Value,
        start: Option<&Value>,
        end: Option<&Value>,
    ) -> R<i64> {
        Ok(find_impl(it, &slf.0.d, sub, start, end, true)?.map_or(-1, |i| i as i64))
    }

    /// B.index(sub[, start[, end]]) -> int
    ///
    /// Return the lowest index in B where subsection sub is found,
    /// such that sub is contained within B[start,end].  Optional
    /// arguments start and end are interpreted as in slice notation.
    ///
    /// Raises ValueError when the subsection is not found.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn index(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        sub: &Value,
        start: Option<&Value>,
        end: Option<&Value>,
    ) -> R<i64> {
        match find_impl(it, &slf.0.d, sub, start, end, false)? {
            Some(i) => Ok(i as i64),
            None => Err(it.value_error("subsection not found")),
        }
    }

    /// B.rindex(sub[, start[, end]]) -> int
    ///
    /// Return the highest index in B where subsection sub is found,
    /// such that sub is contained within B[start,end].  Optional
    /// arguments start and end are interpreted as in slice notation.
    ///
    /// Raise ValueError when the subsection is not found.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn rindex(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        sub: &Value,
        start: Option<&Value>,
        end: Option<&Value>,
    ) -> R<i64> {
        match find_impl(it, &slf.0.d, sub, start, end, true)? {
            Some(i) => Ok(i as i64),
            None => Err(it.value_error("subsection not found")),
        }
    }

    /// B.count(sub[, start[, end]]) -> int
    ///
    /// Return the number of non-overlapping occurrences of subsection sub in
    /// bytes B[start:end].  Optional arguments start and end are interpreted
    /// as in slice notation.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn count(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        sub: &Value,
        start: Option<&Value>,
        end: Option<&Value>,
    ) -> R<i64> {
        let h = &slf.0.d;
        let n = sub_arg(it, sub)?;
        let (s, e) = opt_range(it, start, end, h.len())?;
        if s > e {
            return Ok(0);
        }
        if n.is_empty() {
            return Ok((e - s) as i64 + 1);
        }
        Ok(search::count(&h[s..e], &n, usize::MAX) as i64)
    }

    /// B.startswith(prefix[, start[, end]]) -> bool
    ///
    /// Return True if B starts with the specified prefix, False otherwise.
    /// With optional start, test B beginning at that position.
    /// With optional end, stop comparing B at that position.
    /// prefix can also be a tuple of bytes to try.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn startswith(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        prefix: &Value,
        start: Option<&Value>,
        end: Option<&Value>,
    ) -> R<bool> {
        affix(it, &slf.0.d, prefix, start, end, "startswith", true)
    }

    /// B.endswith(suffix[, start[, end]]) -> bool
    ///
    /// Return True if B ends with the specified suffix, False otherwise.
    /// With optional start, test B beginning at that position.
    /// With optional end, stop comparing B at that position.
    /// suffix can also be a tuple of bytes to try.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn endswith(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        suffix: &Value,
        start: Option<&Value>,
        end: Option<&Value>,
    ) -> R<bool> {
        affix(it, &slf.0.d, suffix, start, end, "endswith", false)
    }

    /// Return a copy with all occurrences of substring old replaced by new.
    ///
    ///   count
    ///     Maximum number of occurrences to replace.
    ///     -1 (the default value) means replace all occurrences.
    ///
    /// If the optional argument count is given, only the first count occurrences are
    /// replaced.
    #[method]
    fn replace(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        old: &[u8],
        new: &[u8],
        #[default(-1)] count: isize,
    ) -> R<Value> {
        let h: &[u8] = &slf.0.d;
        let mut max = count as i64;
        let growth = new.len().saturating_sub(old.len());
        if growth > 0 {
            let cap = if max < 0 { usize::MAX } else { max as usize };
            let mut hits = 0;
            if old.is_empty() {
                hits = (h.len() + 1).min(cap);
            } else {
                let mut from = 0;
                while hits < cap {
                    let Some(p) = find_sub(h, old, from, false) else {
                        break;
                    };
                    hits += 1;
                    from = p + old.len();
                }
            }
            it.check_bytes_len(h.len().saturating_add(growth.saturating_mul(hits)))?;
        }
        let mut out = Vec::with_capacity(h.len());
        if old.is_empty() {
            for b in h {
                if max != 0 {
                    out.extend_from_slice(new);
                    max -= 1;
                }
                out.push(*b);
            }
            if max != 0 {
                out.extend_from_slice(new);
            }
            return Ok(slf.0.wrap(out));
        }
        let mut i = 0;
        while max != 0 {
            match find_sub(h, old, i, false) {
                Some(p) => {
                    out.extend_from_slice(&h[i..p]);
                    out.extend_from_slice(new);
                    i = p + old.len();
                    max -= 1;
                }
                None => break,
            }
        }
        out.extend_from_slice(&h[i..]);
        Ok(slf.0.wrap(out))
    }

    /// Strip leading and trailing bytes contained in the argument.
    ///
    /// If the argument is omitted or None, strip leading and trailing ASCII whitespace.
    #[method]
    fn strip(slf: This<ByteSelf<'_>>, bytes: Option<&[u8]>) -> Value {
        strip_impl(&slf.0, bytes, true, true)
    }

    /// Strip leading bytes contained in the argument.
    ///
    /// If the argument is omitted or None, strip leading  ASCII whitespace.
    #[method]
    fn lstrip(slf: This<ByteSelf<'_>>, bytes: Option<&[u8]>) -> Value {
        strip_impl(&slf.0, bytes, true, false)
    }

    /// Strip trailing bytes contained in the argument.
    ///
    /// If the argument is omitted or None, strip trailing ASCII whitespace.
    #[method]
    fn rstrip(slf: This<ByteSelf<'_>>, bytes: Option<&[u8]>) -> Value {
        strip_impl(&slf.0, bytes, false, true)
    }

    /// B.upper() -> copy of B
    ///
    /// Return a copy of B with all ASCII characters converted to uppercase.
    #[method(hint(py(text_signature = "")))]
    fn upper(slf: This<ByteSelf<'_>>) -> Value {
        slf.0.wrap(slf.0.d.to_ascii_uppercase())
    }

    /// B.lower() -> copy of B
    ///
    /// Return a copy of B with all ASCII characters converted to lowercase.
    #[method(hint(py(text_signature = "")))]
    fn lower(slf: This<ByteSelf<'_>>) -> Value {
        slf.0.wrap(slf.0.d.to_ascii_lowercase())
    }

    /// B.capitalize() -> copy of B
    ///
    /// Return a copy of B with only its first character capitalized (ASCII)
    /// and the rest lower-cased.
    #[method(hint(py(text_signature = "")))]
    fn capitalize(slf: This<ByteSelf<'_>>) -> Value {
        let out = slf
            .0
            .d
            .iter()
            .enumerate()
            .map(|(i, b)| {
                if i == 0 {
                    b.to_ascii_uppercase()
                } else {
                    b.to_ascii_lowercase()
                }
            })
            .collect();
        slf.0.wrap(out)
    }

    /// B.swapcase() -> copy of B
    ///
    /// Return a copy of B with uppercase ASCII characters converted
    /// to lowercase ASCII and vice versa.
    #[method(hint(py(text_signature = "")))]
    fn swapcase(slf: This<ByteSelf<'_>>) -> Value {
        let out = slf
            .0
            .d
            .iter()
            .map(|b| {
                if b.is_ascii_uppercase() {
                    b.to_ascii_lowercase()
                } else {
                    b.to_ascii_uppercase()
                }
            })
            .collect();
        slf.0.wrap(out)
    }

    /// B.title() -> copy of B
    ///
    /// Return a titlecased version of B, i.e. ASCII words start with uppercase
    /// characters, all remaining cased characters have lowercase.
    #[method(hint(py(text_signature = "")))]
    fn title(slf: This<ByteSelf<'_>>) -> Value {
        let mut prev = false;
        let out = slf
            .0
            .d
            .iter()
            .map(|b| {
                let r = if prev {
                    b.to_ascii_lowercase()
                } else {
                    b.to_ascii_uppercase()
                };
                prev = b.is_ascii_alphabetic();
                r
            })
            .collect();
        slf.0.wrap(out)
    }

    /// B.isdigit() -> bool
    ///
    /// Return True if all characters in B are digits
    /// and there is at least one character in B, False otherwise.
    #[method(hint(py(text_signature = "")))]
    fn isdigit(slf: This<ByteSelf<'_>>) -> bool {
        !slf.0.d.is_empty() && slf.0.d.iter().all(u8::is_ascii_digit)
    }

    /// B.isalpha() -> bool
    ///
    /// Return True if all characters in B are alphabetic
    /// and there is at least one character in B, False otherwise.
    #[method(hint(py(text_signature = "")))]
    fn isalpha(slf: This<ByteSelf<'_>>) -> bool {
        !slf.0.d.is_empty() && slf.0.d.iter().all(u8::is_ascii_alphabetic)
    }

    /// B.isalnum() -> bool
    ///
    /// Return True if all characters in B are alphanumeric
    /// and there is at least one character in B, False otherwise.
    #[method(hint(py(text_signature = "")))]
    fn isalnum(slf: This<ByteSelf<'_>>) -> bool {
        !slf.0.d.is_empty() && slf.0.d.iter().all(u8::is_ascii_alphanumeric)
    }

    /// B.isspace() -> bool
    ///
    /// Return True if all characters in B are whitespace
    /// and there is at least one character in B, False otherwise.
    #[method(hint(py(text_signature = "")))]
    fn isspace(slf: This<ByteSelf<'_>>) -> bool {
        !slf.0.d.is_empty() && slf.0.d.iter().all(|&b| is_ws(b))
    }

    /// B.isupper() -> bool
    ///
    /// Return True if all cased characters in B are uppercase and there is
    /// at least one cased character in B, False otherwise.
    #[method(hint(py(text_signature = "")))]
    fn isupper(slf: This<ByteSelf<'_>>) -> bool {
        let d = &slf.0.d;
        d.iter().any(u8::is_ascii_uppercase) && !d.iter().any(u8::is_ascii_lowercase)
    }

    /// B.islower() -> bool
    ///
    /// Return True if all cased characters in B are lowercase and there is
    /// at least one cased character in B, False otherwise.
    #[method(hint(py(text_signature = "")))]
    fn islower(slf: This<ByteSelf<'_>>) -> bool {
        let d = &slf.0.d;
        d.iter().any(u8::is_ascii_lowercase) && !d.iter().any(u8::is_ascii_uppercase)
    }

    /// B.istitle() -> bool
    ///
    /// Return True if B is a titlecased string and there is at least one
    /// character in B, i.e. uppercase characters may only follow uncased
    /// characters and lowercase characters only cased ones. Return False
    /// otherwise.
    #[method(hint(py(text_signature = "")))]
    fn istitle(slf: This<ByteSelf<'_>>) -> bool {
        let (mut prev_cased, mut cased) = (false, false);
        for &b in slf.0.d.iter() {
            if b.is_ascii_uppercase() {
                if prev_cased {
                    return false;
                }
                prev_cased = true;
                cased = true;
            } else if b.is_ascii_lowercase() {
                if !prev_cased {
                    return false;
                }
                prev_cased = true;
                cased = true;
            } else {
                prev_cased = false;
            }
        }
        cased
    }

    /// B.isascii() -> bool
    ///
    /// Return True if B is empty or all characters in B are ASCII,
    /// False otherwise.
    #[method(hint(py(text_signature = "")))]
    fn isascii(slf: This<ByteSelf<'_>>) -> bool {
        slf.0.d.is_ascii()
    }

    /// Pad a numeric string with zeros on the left, to fill a field of the given width.
    ///
    /// The original string is never truncated.
    #[method]
    fn zfill(slf: This<ByteSelf<'_>>, it: &mut Interp, width: isize) -> R<Value> {
        let d = &slf.0.d;
        let w = width.max(0) as usize;
        if d.len() >= w {
            return Ok(slf.0.wrap(d.to_vec()));
        }
        it.check_bytes_len(w)?;
        let (sign, rest) = match d.first() {
            Some(b'+' | b'-') => (&d[..1], &d[1..]),
            _ => (&d[..0], &d[..]),
        };
        let mut out = sign.to_vec();
        out.extend(std::iter::repeat_n(b'0', w - d.len()));
        out.extend_from_slice(rest);
        Ok(slf.0.wrap(out))
    }

    /// Return a copy with each character mapped by the given translation table.
    ///
    ///   table
    ///     Translation table, which must be a bytes object of length 256.
    ///
    /// All characters occurring in the optional argument delete are removed.
    /// The remaining characters are mapped through the given translation table.
    #[method(hint(py(text_signature = "($self, table, /, delete=b'')")))]
    fn translate(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        table: &Value,
        #[kw] delete: Passed<&[u8]>,
    ) -> R<Value> {
        let table = if table.is_none() {
            None
        } else {
            let t = it.buffer_bytes(table)?;
            if t.len() != 256 {
                return Err(it.value_error("translation table must be 256 characters long"));
            }
            Some(t)
        };
        let del = delete.0.unwrap_or(&[]);
        let out: Vec<u8> = slf
            .0
            .d
            .iter()
            .filter(|c| !del.contains(c))
            .map(|&c| table.as_ref().map_or(c, |t| t[c as usize]))
            .collect();
        Ok(slf.0.wrap(out))
    }

    /// Return a translation table useable for the bytes or bytearray translate method.
    ///
    /// The returned table will be one where each byte in frm is mapped to the byte at
    /// the same position in to.
    ///
    /// The bytes objects frm and to must be of the same length.
    #[method]
    fn maketrans(it: &mut Interp, frm: &[u8], to: &[u8]) -> R<Value> {
        if frm.len() != to.len() {
            return Err(it.value_error("maketrans arguments must have same length"));
        }
        let mut t: Vec<u8> = (0..=255u8).collect();
        for (f, x) in frm.iter().zip(to.iter()) {
            t[*f as usize] = *x;
        }
        Ok(Value::bytes(t))
    }

    /// Return a copy where all tab characters are expanded using spaces.
    ///
    /// If tabsize is not given, a tab size of 8 characters is assumed.
    #[method]
    fn expandtabs(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        #[kw]
        #[default(8)]
        tabsize: i32,
    ) -> R<Value> {
        let ts = tabsize as i64;
        let mut out = Vec::new();
        let mut col = 0i64;
        for &c in slf.0.d.iter() {
            match c {
                b'\t' => {
                    if ts > 0 {
                        let n = ts - (col % ts);
                        it.check_bytes_len(out.len() + n as usize)?;
                        out.extend(std::iter::repeat_n(b' ', n as usize));
                        col += n;
                    }
                }
                b'\n' | b'\r' => {
                    out.push(c);
                    col = 0;
                }
                c => {
                    out.push(c);
                    col += 1;
                }
            }
        }
        Ok(slf.0.wrap(out))
    }

    /// Return a left-justified string of length width.
    ///
    /// Padding is done using the specified fill character.
    #[method(hint(py(text_signature = "($self, width, fillchar=b' ', /)")))]
    fn ljust(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        width: isize,
        fillchar: Passed<FillByte>,
    ) -> R<Value> {
        justify(it, &slf.0, width, fillchar, 0)
    }

    /// Return a right-justified string of length width.
    ///
    /// Padding is done using the specified fill character.
    #[method(hint(py(text_signature = "($self, width, fillchar=b' ', /)")))]
    fn rjust(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        width: isize,
        fillchar: Passed<FillByte>,
    ) -> R<Value> {
        justify(it, &slf.0, width, fillchar, 1)
    }

    /// Return a centered string of length width.
    ///
    /// Padding is done using the specified fill character.
    #[method(hint(py(text_signature = "($self, width, fillchar=b' ', /)")))]
    fn center(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        width: isize,
        fillchar: Passed<FillByte>,
    ) -> R<Value> {
        justify(it, &slf.0, width, fillchar, 2)
    }

    #[proto(repr)]
    fn repr(slf: This<&Value>, it: &mut Interp) -> R<String> {
        it.native_repr(&slf)
    }

    #[proto(mod)]
    fn r#mod(slf: This<ByteSelf<'_>>, it: &mut Interp, value: &Value) -> R<Value> {
        let fmt = Value::string(String::from_utf8_lossy(&slf.0.d).into_owned());
        let s = super::format::percent_format(it, &fmt, value)?;
        Ok(slf.0.wrap(s.into_bytes()))
    }
}

#[lumen_bind::class(name = "bytes")]
/// bytes(iterable_of_ints) -> bytes
/// bytes(string, encoding[, errors]) -> bytes
/// bytes(bytes_or_buffer) -> immutable copy of bytes_or_buffer
/// bytes(int) -> bytes object of size given by the parameter initialized with null bytes
/// bytes() -> empty bytes object
///
/// Construct an immutable array of bytes from:
///   - an iterable yielding integers in range(256)
///   - a text string encoded using the specified encoding
///   - any object implementing the buffer API.
///   - an integer
pub struct Bytes;

#[lumen_bind::methods]
impl Bytes {
    #[constructor(hint(py(text_signature = "")))]
    fn new(
        cls: This<Value>,
        it: &mut Interp,
        #[kw] source: Passed<&Value>,
        #[kw] encoding: Passed<&str>,
        #[kw] errors: Passed<&str>,
    ) -> R<Value> {
        let Value::Obj(cls) = &*cls else {
            unreachable!("checked by the entry")
        };
        let v = build(it, source, encoding, errors)?;
        if Rc::ptr_eq(cls, &it.types.bytes) {
            Ok(Value::bytes(v))
        } else {
            Ok(Value::Obj(Object::with_cls(cls.clone(), Kind::Bytes(v))))
        }
    }

    /// Decode the bytes using the codec registered for encoding.
    ///
    ///   encoding
    ///     The encoding with which to decode the bytes.
    ///   errors
    ///     The error handling scheme to use for the handling of decoding errors.
    ///     The default is 'strict' meaning that decoding errors raise a
    ///     UnicodeDecodeError. Other possible values are 'ignore' and 'replace'
    ///     as well as any other name registered with codecs.register_error that
    ///     can handle UnicodeDecodeErrors.
    #[method]
    fn decode(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        #[kw]
        #[default("utf-8")]
        encoding: &str,
        #[kw]
        #[default("strict")]
        errors: &str,
    ) -> R<String> {
        it.decode_bytes(&slf.0.d, encoding, errors)
    }

    /// Create a string of hexadecimal numbers from a bytes object.
    ///
    ///   sep
    ///     An optional single character or byte to separate hex bytes.
    ///   bytes_per_sep
    ///     How many bytes between separators.  Positive values count from the
    ///     right, negative values count from the left.
    ///
    /// Example:
    /// >>> value = b'\xb9\x01\xef'
    /// >>> value.hex()
    /// 'b901ef'
    /// >>> value.hex(':')
    /// 'b9:01:ef'
    /// >>> value.hex(':', 2)
    /// 'b9:01ef'
    /// >>> value.hex(':', -2)
    /// 'b901:ef'
    #[method]
    fn hex(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        #[kw] sep: Passed<&Value>,
        #[kw]
        #[default(1)]
        bytes_per_sep: isize,
    ) -> R<String> {
        hex_impl(it, &slf.0.d, sep, bytes_per_sep)
    }

    /// Create a bytes object from a string of hexadecimal numbers.
    ///
    /// Spaces between two numbers are accepted.
    /// Example: bytes.fromhex('B9 01EF') -> b'\\xb9\\x01\\xef'.
    #[classmethod]
    fn fromhex(cls: This<Value>, it: &mut Interp, string: &Value) -> R<Value> {
        fromhex_impl(it, &cls, string, false)
    }

    /// Concatenate any number of bytes objects.
    ///
    /// The bytes whose method is called is inserted in between each pair.
    ///
    /// The result is returned as a new bytes object.
    ///
    /// Example: b'.'.join([b'ab', b'pq', b'rs']) -> b'ab.pq.rs'.
    #[method]
    fn join(slf: This<ByteSelf<'_>>, it: &mut Interp, iterable_of_bytes: &Value) -> R<Value> {
        join_impl(it, &slf.0, iterable_of_bytes)
    }

    /// Return a list of the sections in the bytes, using sep as the delimiter.
    ///
    ///   sep
    ///     The delimiter according which to split the bytes.
    ///     None (the default value) means split on ASCII whitespace characters
    ///     (space, tab, return, newline, formfeed, vertical tab).
    ///   maxsplit
    ///     Maximum number of splits to do.
    ///     -1 (the default value) means no limit.
    #[method]
    fn split(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        #[kw] sep: Option<&[u8]>,
        #[kw]
        #[default(-1)]
        maxsplit: isize,
    ) -> R<Value> {
        split_impl(it, &slf.0, sep, maxsplit, false)
    }

    /// Return a list of the sections in the bytes, using sep as the delimiter.
    ///
    ///   sep
    ///     The delimiter according which to split the bytes.
    ///     None (the default value) means split on ASCII whitespace characters
    ///     (space, tab, return, newline, formfeed, vertical tab).
    ///   maxsplit
    ///     Maximum number of splits to do.
    ///     -1 (the default value) means no limit.
    ///
    /// Splitting is done starting at the end of the bytes and working to the front.
    #[method]
    fn rsplit(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        #[kw] sep: Option<&[u8]>,
        #[kw]
        #[default(-1)]
        maxsplit: isize,
    ) -> R<Value> {
        split_impl(it, &slf.0, sep, maxsplit, true)
    }

    /// Return a list of the lines in the bytes, breaking at line boundaries.
    ///
    /// Line breaks are not included in the resulting list unless keepends is given and
    /// true.
    #[method]
    fn splitlines(
        slf: This<ByteSelf<'_>>,
        #[kw]
        #[default(false)]
        keepends: bool,
    ) -> Value {
        splitlines_impl(&slf.0, keepends)
    }

    /// Partition the bytes into three parts using the given separator.
    ///
    /// This will search for the separator sep in the bytes. If the separator is found,
    /// returns a 3-tuple containing the part before the separator, the separator
    /// itself, and the part after it.
    ///
    /// If the separator is not found, returns a 3-tuple containing the original bytes
    /// object and two empty bytes objects.
    #[method]
    fn partition(slf: This<ByteSelf<'_>>, it: &mut Interp, sep: &[u8]) -> R<Value> {
        partition_impl(it, &slf.0, sep, false)
    }

    /// Partition the bytes into three parts using the given separator.
    ///
    /// This will search for the separator sep in the bytes, starting at the end. If
    /// the separator is found, returns a 3-tuple containing the part before the
    /// separator, the separator itself, and the part after it.
    ///
    /// If the separator is not found, returns a 3-tuple containing two empty bytes
    /// objects and the original bytes object.
    #[method]
    fn rpartition(slf: This<ByteSelf<'_>>, it: &mut Interp, sep: &[u8]) -> R<Value> {
        partition_impl(it, &slf.0, sep, true)
    }

    /// Return a bytes object with the given prefix string removed if present.
    ///
    /// If the bytes starts with the prefix string, return bytes[len(prefix):].
    /// Otherwise, return a copy of the original bytes.
    #[method]
    fn removeprefix(slf: This<ByteSelf<'_>>, prefix: &[u8]) -> Value {
        let d = &slf.0.d;
        slf.0.wrap(d.strip_prefix(prefix).unwrap_or(d).to_vec())
    }

    /// Return a bytes object with the given suffix string removed if present.
    ///
    /// If the bytes ends with the suffix string and that suffix is not empty,
    /// return bytes[:-len(prefix)].  Otherwise, return a copy of the original
    /// bytes.
    #[method]
    fn removesuffix(slf: This<ByteSelf<'_>>, suffix: &[u8]) -> Value {
        let d = &slf.0.d;
        slf.0.wrap(d.strip_suffix(suffix).unwrap_or(d).to_vec())
    }

    /// Convert this value to exact type bytes.
    #[method(name = "__bytes__")]
    fn dunder_bytes(slf: This<ByteSelf<'_>>) -> Value {
        match slf.0.v {
            Value::Obj(o) if o.cls.is_none() => slf.0.v.clone(),
            _ => Value::bytes(slf.0.d.to_vec()),
        }
    }

    #[method(name = "__getnewargs__", hint(py(text_signature = "")))]
    fn getnewargs(slf: This<ByteSelf<'_>>) -> Value {
        Value::tuple(vec![Value::bytes(slf.0.d.to_vec())])
    }

    #[proto(hash)]
    fn hash(slf: This<&Value>, it: &mut Interp) -> R<i64> {
        it.native_hash(&slf)
    }
}

#[lumen_bind::class(name = "bytearray")]
/// bytearray(iterable_of_ints) -> bytearray
/// bytearray(string, encoding[, errors]) -> bytearray
/// bytearray(bytes_or_buffer) -> mutable copy of bytes_or_buffer
/// bytearray(int) -> bytes array of size given by the parameter initialized with null bytes
/// bytearray() -> empty bytes array
///
/// Construct a mutable bytearray object from:
///   - an iterable yielding integers in range(256)
///   - a text string encoded using the specified encoding
///   - a bytes or a buffer object
///   - any object implementing the buffer API.
///   - an integer
pub struct ByteArray;

#[lumen_bind::methods]
impl ByteArray {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> Value {
        let _ = (args, kwargs);
        let Value::Obj(cls) = &*cls else {
            unreachable!("checked by the entry")
        };
        Value::Obj(Object::with_cls(
            cls.clone(),
            Kind::ByteArray(ba_store(Vec::new())),
        ))
    }

    #[proto(init)]
    fn init(
        slf: This<BaRef<'_>>,
        it: &mut Interp,
        #[kw] source: Passed<&Value>,
        #[kw] encoding: Passed<&str>,
        #[kw] errors: Passed<&str>,
    ) -> R<()> {
        let v = build(it, source, encoding, errors)?;
        it.ba_edit(slf.0 .0, |b| *b = v)
    }

    /// Decode the bytearray using the codec registered for encoding.
    ///
    ///   encoding
    ///     The encoding with which to decode the bytearray.
    ///   errors
    ///     The error handling scheme to use for the handling of decoding errors.
    ///     The default is 'strict' meaning that decoding errors raise a
    ///     UnicodeDecodeError. Other possible values are 'ignore' and 'replace'
    ///     as well as any other name registered with codecs.register_error that
    ///     can handle UnicodeDecodeErrors.
    #[method]
    fn decode(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        #[kw]
        #[default("utf-8")]
        encoding: &str,
        #[kw]
        #[default("strict")]
        errors: &str,
    ) -> R<String> {
        it.decode_bytes(&slf.0.d, encoding, errors)
    }

    /// Create a string of hexadecimal numbers from a bytearray object.
    ///
    ///   sep
    ///     An optional single character or byte to separate hex bytes.
    ///   bytes_per_sep
    ///     How many bytes between separators.  Positive values count from the
    ///     right, negative values count from the left.
    ///
    /// Example:
    /// >>> value = bytearray([0xb9, 0x01, 0xef])
    /// >>> value.hex()
    /// 'b901ef'
    /// >>> value.hex(':')
    /// 'b9:01:ef'
    /// >>> value.hex(':', 2)
    /// 'b9:01ef'
    /// >>> value.hex(':', -2)
    /// 'b901:ef'
    #[method]
    fn hex(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        #[kw] sep: Passed<&Value>,
        #[kw]
        #[default(1)]
        bytes_per_sep: isize,
    ) -> R<String> {
        hex_impl(it, &slf.0.d, sep, bytes_per_sep)
    }

    /// Create a bytearray object from a string of hexadecimal numbers.
    ///
    /// Spaces between two numbers are accepted.
    /// Example: bytearray.fromhex('B9 01EF') -> bytearray(b'\\xb9\\x01\\xef')
    #[classmethod]
    fn fromhex(cls: This<Value>, it: &mut Interp, string: &Value) -> R<Value> {
        fromhex_impl(it, &cls, string, true)
    }

    /// Concatenate any number of bytes/bytearray objects.
    ///
    /// The bytearray whose method is called is inserted in between each pair.
    ///
    /// The result is returned as a new bytearray object.
    #[method]
    fn join(slf: This<ByteSelf<'_>>, it: &mut Interp, iterable_of_bytes: &Value) -> R<Value> {
        join_impl(it, &slf.0, iterable_of_bytes)
    }

    /// Return a list of the sections in the bytearray, using sep as the delimiter.
    ///
    ///   sep
    ///     The delimiter according which to split the bytearray.
    ///     None (the default value) means split on ASCII whitespace characters
    ///     (space, tab, return, newline, formfeed, vertical tab).
    ///   maxsplit
    ///     Maximum number of splits to do.
    ///     -1 (the default value) means no limit.
    #[method]
    fn split(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        #[kw] sep: Option<&[u8]>,
        #[kw]
        #[default(-1)]
        maxsplit: isize,
    ) -> R<Value> {
        split_impl(it, &slf.0, sep, maxsplit, false)
    }

    /// Return a list of the sections in the bytearray, using sep as the delimiter.
    ///
    ///   sep
    ///     The delimiter according which to split the bytearray.
    ///     None (the default value) means split on ASCII whitespace characters
    ///     (space, tab, return, newline, formfeed, vertical tab).
    ///   maxsplit
    ///     Maximum number of splits to do.
    ///     -1 (the default value) means no limit.
    ///
    /// Splitting is done starting at the end of the bytearray and working to the front.
    #[method]
    fn rsplit(
        slf: This<ByteSelf<'_>>,
        it: &mut Interp,
        #[kw] sep: Option<&[u8]>,
        #[kw]
        #[default(-1)]
        maxsplit: isize,
    ) -> R<Value> {
        split_impl(it, &slf.0, sep, maxsplit, true)
    }

    /// Return a list of the lines in the bytearray, breaking at line boundaries.
    ///
    /// Line breaks are not included in the resulting list unless keepends is given and
    /// true.
    #[method]
    fn splitlines(
        slf: This<ByteSelf<'_>>,
        #[kw]
        #[default(false)]
        keepends: bool,
    ) -> Value {
        splitlines_impl(&slf.0, keepends)
    }

    /// Partition the bytearray into three parts using the given separator.
    ///
    /// This will search for the separator sep in the bytearray. If the separator is
    /// found, returns a 3-tuple containing the part before the separator, the
    /// separator itself, and the part after it as new bytearray objects.
    ///
    /// If the separator is not found, returns a 3-tuple containing the copy of the
    /// original bytearray object and two empty bytearray objects.
    #[method]
    fn partition(slf: This<ByteSelf<'_>>, it: &mut Interp, sep: &[u8]) -> R<Value> {
        partition_impl(it, &slf.0, sep, false)
    }

    /// Partition the bytearray into three parts using the given separator.
    ///
    /// This will search for the separator sep in the bytearray, starting at the end.
    /// If the separator is found, returns a 3-tuple containing the part before the
    /// separator, the separator itself, and the part after it as new bytearray
    /// objects.
    ///
    /// If the separator is not found, returns a 3-tuple containing two empty bytearray
    /// objects and the copy of the original bytearray object.
    #[method]
    fn rpartition(slf: This<ByteSelf<'_>>, it: &mut Interp, sep: &[u8]) -> R<Value> {
        partition_impl(it, &slf.0, sep, true)
    }

    /// Return a bytearray with the given prefix string removed if present.
    ///
    /// If the bytearray starts with the prefix string, return
    /// bytearray[len(prefix):].  Otherwise, return a copy of the original
    /// bytearray.
    #[method]
    fn removeprefix(slf: This<ByteSelf<'_>>, prefix: &[u8]) -> Value {
        let d = &slf.0.d;
        slf.0.wrap(d.strip_prefix(prefix).unwrap_or(d).to_vec())
    }

    /// Return a bytearray with the given suffix string removed if present.
    ///
    /// If the bytearray ends with the suffix string and that suffix is not
    /// empty, return bytearray[:-len(suffix)].  Otherwise, return a copy of
    /// the original bytearray.
    #[method]
    fn removesuffix(slf: This<ByteSelf<'_>>, suffix: &[u8]) -> Value {
        let d = &slf.0.d;
        slf.0.wrap(d.strip_suffix(suffix).unwrap_or(d).to_vec())
    }

    /// Append a single item to the end of the bytearray.
    ///
    ///   item
    ///     The item to be appended.
    #[method]
    fn append(slf: This<BaRef<'_>>, it: &mut Interp, item: &Value) -> R<()> {
        let b = byte_val(it, item)?;
        it.ba_edit(slf.0 .0, |v| v.push(b))
    }

    /// Append all the items from the iterator or sequence to the end of the bytearray.
    ///
    ///   iterable_of_ints
    ///     The iterable of items to append.
    #[method]
    fn extend(slf: This<BaRef<'_>>, it: &mut Interp, iterable_of_ints: &Value) -> R<()> {
        let items = match iterable_of_ints {
            Value::Int(_) | Value::Bool(_) => {
                let t = it.type_name_of(iterable_of_ints);
                return Err(it.type_error(&format!("can't extend bytearray with {t}")));
            }
            v if v.as_str().is_some() => {
                return Err(it.type_error("expected iterable of integers; got: 'str'"));
            }
            v => it.bytes_from_object(v)?,
        };
        it.ba_edit(slf.0 .0, |v| v.extend(items))
    }

    /// Remove and return a single item from B.
    ///
    ///   index
    ///     The index from where to remove the item.
    ///     -1 (the default value) means remove the last item.
    ///
    /// If no index argument is given, will pop the last item.
    #[method]
    fn pop(slf: This<BaRef<'_>>, it: &mut Interp, #[default(-1)] index: isize) -> R<i64> {
        let store = slf.0 .0;
        let len = store.len() as i64;
        if len == 0 {
            return Err(it.new_exc_str("IndexError", "pop from empty bytearray"));
        }
        let i = index as i64;
        let k = if i < 0 { i + len } else { i };
        if k < 0 || k >= len {
            return Err(it.new_exc_str("IndexError", "pop index out of range"));
        }
        Ok(it.ba_edit(store, |v| v.remove(k as usize))? as i64)
    }

    /// Insert a single item into the bytearray before the given index.
    ///
    ///   index
    ///     The index where the value is to be inserted.
    ///   item
    ///     The item to be inserted.
    #[method]
    fn insert(slf: This<BaRef<'_>>, it: &mut Interp, index: isize, item: &Value) -> R<()> {
        let b = byte_val(it, item)?;
        let store = slf.0 .0;
        let (i, len) = (index as i64, store.len() as i64);
        let k = if i < 0 { (i + len).max(0) } else { i.min(len) };
        it.ba_edit(store, |v| v.insert(k as usize, b))
    }

    /// Remove the first occurrence of a value in the bytearray.
    ///
    ///   value
    ///     The value to remove.
    #[method]
    fn remove(slf: This<BaRef<'_>>, it: &mut Interp, value: &Value) -> R<()> {
        let b = byte_val(it, value)?;
        let store = slf.0 .0;
        let pos = store.bytes().iter().position(|&x| x == b);
        match pos {
            Some(p) => it.ba_edit(store, |v| {
                v.remove(p);
            }),
            None => Err(it.value_error("value not found in bytearray")),
        }
    }

    /// Return state information for pickling.
    #[method(name = "__reduce__")]
    fn reduce(slf: This<ByteSelf<'_>>, it: &mut Interp) -> Value {
        reduce_impl(it, &slf.0, 2)
    }

    /// Return state information for pickling.
    #[method(name = "__reduce_ex__")]
    fn reduce_ex(slf: This<ByteSelf<'_>>, it: &mut Interp, #[default(0)] proto: i32) -> Value {
        reduce_impl(it, &slf.0, proto)
    }

    /// Reverse the order of the values in B in place.
    #[method]
    fn reverse(slf: This<BaRef<'_>>, it: &mut Interp) -> R<()> {
        it.ba_write(slf.0 .0)?.reverse();
        Ok(())
    }

    /// Return a copy of B.
    #[method]
    fn copy(slf: This<BaRef<'_>>) -> Value {
        Value::Obj(Object::new(Kind::ByteArray(ba_store(slf.0 .0.to_vec()))))
    }

    /// Remove all items from the bytearray.
    #[method]
    fn clear(slf: This<BaRef<'_>>, it: &mut Interp) -> R<()> {
        it.ba_edit(slf.0 .0, |v| v.clear())
    }

    /// B.__alloc__() -> int
    ///
    /// Return the number of bytes actually allocated.
    #[method(name = "__alloc__", hint(py(text_signature = "")))]
    fn alloc(slf: This<BaRef<'_>>) -> i64 {
        slf.0 .0.len() as i64 + 1
    }
}

pub fn init(it: &mut Interp) {
    use crate::bind::extend_type_documented;
    let (bytes, ba) = (it.types.bytes.clone(), it.types.bytearray.clone());
    for t in [&bytes, &ba] {
        install_all::<ByteMethods>(t);
        reg_slots(
            it,
            t,
            &["__getitem__", "__len__", "__contains__", "__iter__"],
        );
        reg_binops(it, t, &["__add__", "__mul__", "__rmul__"]);
        reg_compare(it, t, true);
    }
    extend_type_documented::<Bytes>(it, &bytes);
    extend_type_documented::<ByteArray>(it, &ba);
    reg_slots(it, &ba, &["__setitem__", "__delitem__"]);
    let d = ba.dict.borrow().clone();
    if let Some(d) = d {
        dict_set_str(&d, "__hash__", Value::None);
    }
}

impl Interp {
    /// A length-changing edit of a `bytearray`; refused while a memoryview exports it.
    pub fn ba_edit<T>(&mut self, b: &ByteStore, f: impl FnOnce(&mut Vec<u8>) -> T) -> R<T> {
        b.edit(f).map_err(|e| crate::bind::buffer_error(self, e))
    }

    /// In-place (same-length) writes to a `bytearray`; allowed while exported.
    pub fn ba_write<'a>(&mut self, b: &'a ByteStore) -> R<lumen_common::buffer::BytesMut<'a>> {
        b.try_bytes_mut()
            .map_err(|e| crate::bind::buffer_error(self, e))
    }
}
