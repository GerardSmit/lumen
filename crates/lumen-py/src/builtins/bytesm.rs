//! `bytes` and `bytearray`.

use super::numeric::{reg_binops, reg_compare};
use super::slots::reg_slots;
use crate::object::*;
use crate::vm::*;


type Kw<'a> = &'a [(Obj, Value)];

fn data(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
    match v {
        Value::Obj(o) => match &o.kind {
            Kind::Bytes(b) => Ok(b.clone()),
            Kind::ByteArray(b) => Ok(b.to_vec()),
            _ => Err(it.type_error("descriptor requires a 'bytes' object")),
        },
        _ => Err(it.type_error("descriptor requires a 'bytes' object")),
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

fn build(it: &mut Interp, a: &[Value], kw: Kw, array: bool) -> R<Vec<u8>> {
    let name = if array { "bytearray" } else { "bytes" };
    let b = it.bind_args(name, &a[1.min(a.len())..], kw, &["source", "encoding", "errors"], 0)?;
    let Some(src) = &b[0] else { return Ok(Vec::new()) };
    if let Some(s) = src.as_str() {
        let Some(enc) = &b[1] else { return Err(it.type_error("string argument without an encoding")) };
        let enc = it.str_arg(enc, "encoding")?;
        let errs = match &b[2] {
            Some(e) => it.str_arg(e, "errors")?,
            None => "strict".to_string(),
        };
        return it.encode_str(s, &enc, &errs);
    }
    if b[1].is_some() {
        return Err(it.type_error("encoding without a string argument"));
    }
    match src {
        Value::Int(n) => {
            if *n < 0 {
                return Err(it.value_error("negative count"));
            }
            zeroed(it, *n as usize)
        }
        Value::Bool(n) => zeroed(it, *n as usize),
        Value::Obj(o) if matches!(o.kind, Kind::Int(_)) => Err(it.overflow_err("cannot fit 'int' into an index-sized integer")),
        _ => it.bytes_of(src),
    }
}

fn bytes_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let v = build(it, a, kw, false)?;
    match a.first() {
        Some(Value::Obj(c)) if !std::rc::Rc::ptr_eq(c, &it.types.bytes) => Ok(Value::Obj(Object::with_cls(c.clone(), Kind::Bytes(v)))),
        _ => Ok(Value::bytes(v)),
    }
}

fn bytearray_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let v = build(it, a, kw, true)?;
    match a.first() {
        Some(Value::Obj(c)) if !std::rc::Rc::ptr_eq(c, &it.types.bytearray) => Ok(Value::Obj(Object::with_cls(c.clone(), Kind::ByteArray(ba_store(v))))),
        _ => Ok(wrap(&Value::Obj(Object::new(Kind::ByteArray(ba_store(Vec::new())))), v)),
    }
}

fn bytearray_init(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::None)
}

fn decode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("decode", &a[1..], kw, &["encoding", "errors"], 0)?;
    let d = data(it, &a[0])?;
    let enc = match &b[0] {
        Some(e) => it.str_arg(e, "decode() argument 'encoding'")?,
        None => "utf-8".to_string(),
    };
    let errs = match &b[1] {
        Some(e) => it.str_arg(e, "decode() argument 'errors'")?,
        None => "strict".to_string(),
    };
    Ok(Value::string(it.decode_bytes(&d, &enc, &errs)?))
}

fn hex(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let d = data(it, &a[0])?;
    let b = it.bind_args("hex", &a[1..], kw, &["sep", "bytes_per_sep"], 0)?;
    let sep = super::memview::hex_sep_arg(it, b[0].as_ref())?;
    let per = match &b[1] {
        Some(v) => it.index_of(v)?,
        None => 1,
    };
    Ok(Value::string(lumen_common::codec::hex_encode_sep(&d, sep, per)))
}

fn fromhex(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("fromhex", a, 2, 2)?;
    let s = it.str_arg(&a[1], "fromhex() argument")?;
    let digits: Vec<char> = s.chars().filter(|c| !c.is_whitespace()).collect();
    let mut out = Vec::new();
    for (i, pair) in digits.chunks(2).enumerate() {
        let hi = pair[0].to_digit(16);
        let lo = pair.get(1).and_then(|c| c.to_digit(16));
        match (hi, lo) {
            (Some(h), Some(l)) => out.push((h * 16 + l) as u8),
            _ => {
                return Err(it.value_error(&format!("non-hexadecimal number found in fromhex() arg at position {}", i * 2)));
            }
        }
    }
    Ok(match &a[0] {
        Value::Obj(c) if std::rc::Rc::ptr_eq(c, &it.types.bytearray) => wrap(&Value::Obj(Object::new(Kind::ByteArray(ba_store(Vec::new())))), out),
        _ => Value::bytes(out),
    })
}

fn join(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("join", a, 2, 2)?;
    let sep = data(it, &a[0])?;
    let items = it.iterate_to_vec(&a[1])?;
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
            out.extend_from_slice(&sep);
        }
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Bytes(_) | Kind::ByteArray(_)) => out.extend(data(it, v)?),
            _ => {
                let t = it.type_name_of(v);
                return Err(it.type_error(&format!("sequence item {}: expected a bytes-like object, {} found", i, t)));
            }
        }
    }
    Ok(wrap(&a[0], out))
}

fn find_sub(h: &[u8], n: &[u8], from: usize, rev: bool) -> Option<usize> {
    if n.len() > h.len() {
        return None;
    }
    let range = from..=(h.len() - n.len());
    if rev {
        range.rev().find(|&i| &h[i..i + n.len()] == n)
    } else {
        range.into_iter().find(|&i| &h[i..i + n.len()] == n)
    }
}

fn sub_arg(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
    match v {
        Value::Int(n) if (0..256).contains(n) => Ok(vec![*n as u8]),
        _ => it.bytes_of(v),
    }
}

fn find_impl(it: &mut Interp, a: &[Value], rev: bool, name: &str) -> R<Option<usize>> {
    it.check_args(name, a, 2, 4)?;
    let h = data(it, &a[0])?;
    let n = sub_arg(it, &a[1])?;
    let len = h.len() as i64;
    let norm = |it: &mut Interp, v: Option<&Value>, d: i64| -> R<i64> {
        match v {
            None | Some(Value::None) => Ok(d),
            Some(v) => {
                let i = it.slice_index(v)?;
                Ok(if i < 0 { i.saturating_add(len).max(0) } else { i.min(len) })
            }
        }
    };
    let s = norm(it, a.get(2), 0)? as usize;
    let e = norm(it, a.get(3), len)? as usize;
    if s > e {
        return Ok(None);
    }
    Ok(find_sub(&h[..e], &n, s, rev))
}

fn find(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(find_impl(it, a, false, "find")?.map_or(-1, |i| i as i64)))
}

fn rfind(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(find_impl(it, a, true, "rfind")?.map_or(-1, |i| i as i64)))
}

fn index(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match find_impl(it, a, false, "index")? {
        Some(i) => Ok(Value::Int(i as i64)),
        None => Err(it.value_error("subsection not found")),
    }
}

fn rindex(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match find_impl(it, a, true, "rindex")? {
        Some(i) => Ok(Value::Int(i as i64)),
        None => Err(it.value_error("subsection not found")),
    }
}

fn count(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("count", a, 2, 2)?;
    let h = data(it, &a[0])?;
    let n = sub_arg(it, &a[1])?;
    if n.is_empty() {
        return Ok(Value::Int(h.len() as i64 + 1));
    }
    let (mut c, mut i) = (0, 0);
    while let Some(p) = find_sub(&h, &n, i, false) {
        c += 1;
        i = p + n.len();
    }
    Ok(Value::Int(c))
}

fn affix(it: &mut Interp, a: &[Value], start: bool) -> R<Value> {
    it.check_args(if start { "startswith" } else { "endswith" }, a, 2, 2)?;
    let h = data(it, &a[0])?;
    let cands = match a[1].tuple_items() {
        Some(t) => t.to_vec(),
        None => vec![a[1].clone()],
    };
    for c in cands {
        let n = it.bytes_of(&c)?;
        if (start && h.starts_with(&n)) || (!start && h.ends_with(&n)) {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn startswith(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    affix(it, a, true)
}

fn endswith(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    affix(it, a, false)
}

fn replace(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("replace", a, 3, 4)?;
    let h = data(it, &a[0])?;
    let old = it.bytes_of(&a[1])?;
    let new = it.bytes_of(&a[2])?;
    let mut max = match a.get(3) {
        Some(v) => it.index_of(v)?,
        None => -1,
    };
    let growth = new.len().saturating_sub(old.len());
    if growth > 0 {
        let cap = if max < 0 { usize::MAX } else { max as usize };
        let mut hits = 0;
        if old.is_empty() {
            hits = (h.len() + 1).min(cap);
        } else {
            let mut from = 0;
            while hits < cap {
                let Some(p) = find_sub(&h, &old, from, false) else { break };
                hits += 1;
                from = p + old.len();
            }
        }
        it.check_bytes_len(h.len().saturating_add(growth.saturating_mul(hits)))?;
    }
    let mut out = Vec::new();
    let mut i = 0;
    if old.is_empty() {
        for (k, b) in h.iter().enumerate() {
            if max != 0 {
                out.extend_from_slice(&new);
                max -= 1;
            }
            out.push(*b);
            let _ = k;
        }
        if max != 0 {
            out.extend_from_slice(&new);
        }
        return Ok(wrap(&a[0], out));
    }
    while max != 0 {
        match find_sub(&h, &old, i, false) {
            Some(p) => {
                out.extend_from_slice(&h[i..p]);
                out.extend_from_slice(&new);
                i = p + old.len();
                max -= 1;
            }
            None => break,
        }
    }
    out.extend_from_slice(&h[i..]);
    Ok(wrap(&a[0], out))
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn strip_impl(it: &mut Interp, a: &[Value], left: bool, right: bool) -> R<Value> {
    let h = data(it, &a[0])?;
    let chars = match a.get(1) {
        None | Some(Value::None) => None,
        Some(v) => Some(it.bytes_of(v)?),
    };
    let strip = |b: u8| match &chars {
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
    Ok(wrap(&a[0], h[s..e].to_vec()))
}

fn strip(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    strip_impl(it, a, true, true)
}

fn lstrip(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    strip_impl(it, a, true, false)
}

fn rstrip(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    strip_impl(it, a, false, true)
}

fn split_impl(it: &mut Interp, a: &[Value], kw: Kw, rev: bool) -> R<Value> {
    let b = it.bind_args("split", &a[1..], kw, &["sep", "maxsplit"], 0)?;
    let h = data(it, &a[0])?;
    let max = match &b[1] {
        Some(v) => it.index_of(v)?,
        None => -1,
    };
    let sep = match &b[0] {
        None | Some(Value::None) => None,
        Some(v) => Some(it.bytes_of(v)?),
    };
    let mut parts: Vec<Vec<u8>> = Vec::new();
    match sep {
        Some(sep) => {
            if sep.is_empty() {
                return Err(it.value_error("empty separator"));
            }
            if !rev {
                let (mut i, mut n) = (0, max);
                while n != 0 {
                    match find_sub(&h, &sep, i, false) {
                        Some(p) => {
                            parts.push(h[i..p].to_vec());
                            i = p + sep.len();
                            n -= 1;
                        }
                        None => break,
                    }
                }
                parts.push(h[i..].to_vec());
            } else {
                let (mut e, mut n) = (h.len(), max);
                while n != 0 {
                    match find_sub(&h[..e], &sep, 0, true) {
                        Some(p) => {
                            parts.push(h[p + sep.len()..e].to_vec());
                            e = p;
                            n -= 1;
                        }
                        None => break,
                    }
                }
                parts.push(h[..e].to_vec());
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
                        parts.push(h[i..e].to_vec());
                        break;
                    }
                    let s = i;
                    while i < h.len() && !is_ws(h[i]) {
                        i += 1;
                    }
                    parts.push(h[s..i].to_vec());
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
                        parts.push(h[s..e].to_vec());
                        break;
                    }
                    let end = e;
                    while e > 0 && !is_ws(h[e - 1]) {
                        e -= 1;
                    }
                    parts.push(h[e..end].to_vec());
                    n -= 1;
                }
                parts.reverse();
            }
        }
    }
    Ok(Value::list(parts.into_iter().map(|p| wrap(&a[0], p)).collect()))
}

fn split(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    split_impl(it, a, kw, false)
}

fn rsplit(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    split_impl(it, a, kw, true)
}

fn splitlines(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let h = data(it, &a[0])?;
    let keep = match a.get(1) {
        Some(v) => it.truthy(v)?,
        None => false,
    };
    let mut out = Vec::new();
    let mut s = 0;
    let mut i = 0;
    while i < h.len() {
        if h[i] == b'\n' || h[i] == b'\r' {
            let mut e = i + 1;
            if h[i] == b'\r' && e < h.len() && h[e] == b'\n' {
                e += 1;
            }
            out.push(wrap(&a[0], h[s..if keep { e } else { i }].to_vec()));
            s = e;
            i = e;
        } else {
            i += 1;
        }
    }
    if s < h.len() {
        out.push(wrap(&a[0], h[s..].to_vec()));
    }
    Ok(Value::list(out))
}

fn partition_impl(it: &mut Interp, a: &[Value], rev: bool) -> R<Value> {
    it.check_args("partition", a, 2, 2)?;
    let h = data(it, &a[0])?;
    let sep = it.bytes_of(&a[1])?;
    if sep.is_empty() {
        return Err(it.value_error("empty separator"));
    }
    Ok(match find_sub(&h, &sep, 0, rev) {
        Some(p) => Value::tuple(vec![wrap(&a[0], h[..p].to_vec()), wrap(&a[0], sep.clone()), wrap(&a[0], h[p + sep.len()..].to_vec())]),
        None if rev => Value::tuple(vec![wrap(&a[0], Vec::new()), wrap(&a[0], Vec::new()), wrap(&a[0], h)]),
        None => Value::tuple(vec![wrap(&a[0], h), wrap(&a[0], Vec::new()), wrap(&a[0], Vec::new())]),
    })
}

fn partition(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    partition_impl(it, a, false)
}

fn rpartition(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    partition_impl(it, a, true)
}

fn upper(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let d = data(it, &a[0])?;
    Ok(wrap(&a[0], d.to_ascii_uppercase()))
}

fn lower(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let d = data(it, &a[0])?;
    Ok(wrap(&a[0], d.to_ascii_lowercase()))
}

fn capitalize(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let d = data(it, &a[0])?;
    let out = d.iter().enumerate().map(|(i, b)| if i == 0 { b.to_ascii_uppercase() } else { b.to_ascii_lowercase() }).collect();
    Ok(wrap(&a[0], out))
}

fn swapcase(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let d = data(it, &a[0])?;
    let out = d.iter().map(|b| if b.is_ascii_uppercase() { b.to_ascii_lowercase() } else { b.to_ascii_uppercase() }).collect();
    Ok(wrap(&a[0], out))
}

fn title(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let d = data(it, &a[0])?;
    let mut prev = false;
    let out = d
        .iter()
        .map(|b| {
            let r = if prev { b.to_ascii_lowercase() } else { b.to_ascii_uppercase() };
            prev = b.is_ascii_alphabetic();
            r
        })
        .collect();
    Ok(wrap(&a[0], out))
}

fn pred(it: &mut Interp, a: &[Value], f: fn(&[u8]) -> bool) -> R<Value> {
    let d = data(it, &a[0])?;
    Ok(Value::Bool(f(&d)))
}

fn isdigit(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    pred(it, a, |d| !d.is_empty() && d.iter().all(u8::is_ascii_digit))
}

fn isalpha(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    pred(it, a, |d| !d.is_empty() && d.iter().all(u8::is_ascii_alphabetic))
}

fn isalnum(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    pred(it, a, |d| !d.is_empty() && d.iter().all(u8::is_ascii_alphanumeric))
}

fn isspace(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    pred(it, a, |d| !d.is_empty() && d.iter().all(|&b| is_ws(b)))
}

fn isupper(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    pred(it, a, |d| d.iter().any(u8::is_ascii_uppercase) && !d.iter().any(u8::is_ascii_lowercase))
}

fn islower(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    pred(it, a, |d| d.iter().any(u8::is_ascii_lowercase) && !d.iter().any(u8::is_ascii_uppercase))
}

fn isascii(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    pred(it, a, |d| d.is_ascii())
}

fn zfill(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("zfill", a, 2, 2)?;
    let d = data(it, &a[0])?;
    let w = it.index_of(&a[1])?.max(0) as usize;
    if d.len() >= w {
        return Ok(wrap(&a[0], d));
    }
    it.check_bytes_len(w)?;
    let (sign, rest) = match d.first() {
        Some(b'+') | Some(b'-') => (d[..1].to_vec(), d[1..].to_vec()),
        _ => (Vec::new(), d),
    };
    let mut out = sign;
    out.extend(std::iter::repeat_n(b'0', w - out.len() - rest.len()));
    out.extend(rest);
    Ok(wrap(&a[0], out))
}

fn justify(it: &mut Interp, a: &[Value], mode: u8) -> R<Value> {
    it.check_args("center", a, 2, 3)?;
    let d = data(it, &a[0])?;
    let w = it.index_of(&a[1])?.max(0) as usize;
    let fill = match a.get(2) {
        Some(v) => {
            let f = it.bytes_of(v)?;
            if f.len() != 1 {
                return Err(it.type_error("argument 2 must be a byte string of length 1"));
            }
            f[0]
        }
        None => b' ',
    };
    if d.len() >= w {
        return Ok(wrap(&a[0], d));
    }
    it.check_bytes_len(w)?;
    let pad = w - d.len();
    let (l, r) = match mode {
        0 => (0, pad),
        1 => (pad, 0),
        _ => (pad / 2 + (pad & w & 1), pad - (pad / 2 + (pad & w & 1))),
    };
    let mut out = vec![fill; l];
    out.extend(d);
    out.extend(vec![fill; r]);
    Ok(wrap(&a[0], out))
}

fn ljust(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    justify(it, a, 0)
}

fn rjust(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    justify(it, a, 1)
}

fn center(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    justify(it, a, 2)
}

fn removeprefix(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("removeprefix", a, 2, 2)?;
    let d = data(it, &a[0])?;
    let p = it.bytes_of(&a[1])?;
    Ok(wrap(&a[0], d.strip_prefix(p.as_slice()).unwrap_or(&d).to_vec()))
}

fn removesuffix(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("removesuffix", a, 2, 2)?;
    let d = data(it, &a[0])?;
    let p = it.bytes_of(&a[1])?;
    Ok(wrap(&a[0], d.strip_suffix(p.as_slice()).unwrap_or(&d).to_vec()))
}

fn bytes_hash(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(it.native_hash(&a[0])?))
}

fn bytes_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::string(it.native_repr(&a[0])?))
}

fn bytes_bytes(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::bytes(data(it, &a[0])?))
}

fn bytes_mod(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__mod__", a, 2, 2)?;
    let d = data(it, &a[0])?;
    let fmt = Value::string(String::from_utf8_lossy(&d).into_owned());
    let s = super::format::percent_format(it, &fmt, &a[1])?;
    Ok(wrap(&a[0], s.into_bytes()))
}

fn table_arg(it: &mut Interp, v: &Value) -> R<Option<Vec<u8>>> {
    if v.is_none() {
        return Ok(None);
    }
    let t = data(it, v)?;
    if t.len() != 256 {
        return Err(it.value_error("translation table must be 256 characters long"));
    }
    Ok(Some(t))
}

fn translate(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("translate", &a[1.min(a.len())..], kw, &["table", "delete"], 1)?;
    let d = data(it, &a[0])?;
    let table = match &b[0] {
        Some(t) => table_arg(it, t)?,
        None => None,
    };
    let del = match &b[1] {
        Some(x) => data(it, x)?,
        None => Vec::new(),
    };
    let out: Vec<u8> = d
        .iter()
        .filter(|c| !del.contains(c))
        .map(|&c| table.as_ref().map_or(c, |t| t[c as usize]))
        .collect();
    Ok(wrap(&a[0], out))
}

fn maketrans(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("maketrans", a, 2, 2)?;
    let from = data(it, &a[0])?;
    let to = data(it, &a[1])?;
    if from.len() != to.len() {
        return Err(it.value_error("maketrans arguments must have same length"));
    }
    let mut t: Vec<u8> = (0..=255u8).collect();
    for (f, x) in from.iter().zip(to.iter()) {
        t[*f as usize] = *x;
    }
    Ok(Value::bytes(t))
}

fn expandtabs(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("expandtabs", &a[1.min(a.len())..], kw, &["tabsize"], 0)?;
    let ts = match &b[0] {
        Some(v) => it.index_of(v)?,
        None => 8,
    };
    if i32::try_from(ts).is_err() {
        return Err(it.overflow_err("Python int too large to convert to C int"));
    }
    let d = data(it, &a[0])?;
    let mut out = Vec::new();
    let mut col = 0i64;
    for c in d {
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
    Ok(wrap(&a[0], out))
}

fn ba_of<'a>(it: &mut Interp, a: &'a [Value]) -> R<&'a ByteStore> {
    match a.first() {
        Some(Value::Obj(o)) => match &o.kind {
            Kind::ByteArray(b) => Ok(b),
            _ => Err(it.type_error("descriptor requires a 'bytearray' object")),
        },
        _ => Err(it.type_error("descriptor requires a 'bytearray' object")),
    }
}

fn byte_val(it: &mut Interp, v: &Value) -> R<u8> {
    let n = it.index_of(v)?;
    if !(0..256).contains(&n) {
        return Err(it.value_error("byte must be in range(0, 256)"));
    }
    Ok(n as u8)
}

fn ba_append(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("append", a, 2, 2)?;
    let b = byte_val(it, &a[1])?;
    let cell = ba_of(it, a)?;
    it.ba_edit(cell, |v| v.push(b))?;
    Ok(Value::None)
}

fn ba_extend(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("extend", a, 2, 2)?;
    let items = it.bytes_of(&a[1])?;
    let cell = ba_of(it, a)?;
    it.ba_edit(cell, |v| v.extend(items))?;
    Ok(Value::None)
}

fn ba_pop(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("pop", a, 1, 2)?;
    let i = match a.get(1) {
        Some(v) => it.index_of(v)?,
        None => -1,
    };
    let cell = ba_of(it, a)?;
    let len = cell.len() as i64;
    if len == 0 {
        return Err(it.new_exc_str("IndexError", "pop from empty bytearray"));
    }
    let k = if i < 0 { i + len } else { i };
    if k < 0 || k >= len {
        return Err(it.new_exc_str("IndexError", "pop index out of range"));
    }
    Ok(Value::Int(it.ba_edit(cell, |v| v.remove(k as usize))? as i64))
}

fn ba_clear(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let cell = ba_of(it, a)?;
    it.ba_edit(cell, |v| v.clear())?;
    Ok(Value::None)
}

fn ba_insert(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("insert", a, 3, 3)?;
    let i = it.index_of(&a[1])?;
    let b = byte_val(it, &a[2])?;
    let cell = ba_of(it, a)?;
    let len = cell.len() as i64;
    let k = if i < 0 { (i + len).max(0) } else { i.min(len) };
    it.ba_edit(cell, |v| v.insert(k as usize, b))?;
    Ok(Value::None)
}

fn ba_remove(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("remove", a, 2, 2)?;
    let b = byte_val(it, &a[1])?;
    let cell = ba_of(it, a)?;
    let pos = cell.bytes().iter().position(|&x| x == b);
    match pos {
        Some(p) => {
            it.ba_edit(cell, |v| v.remove(p))?;
            Ok(Value::None)
        }
        None => Err(it.value_error("value not found in bytearray")),
    }
}

fn ba_reverse(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let cell = ba_of(it, a)?;
    it.ba_write(cell)?.reverse();
    Ok(Value::None)
}

fn ba_copy(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let d = data(it, &a[0])?;
    Ok(wrap(&a[0], d))
}

pub fn init(it: &mut Interp) {
    let (bytes, ba) = (it.types.bytes.clone(), it.types.bytearray.clone());
    it.reg_new(&bytes, bytes_new);
    it.reg_new(&ba, bytearray_new);
    it.reg(&ba, "__init__", bytearray_init);
    for t in [&bytes, &ba] {
        it.reg(t, "decode", decode);
        it.reg(t, "hex", hex);
        it.reg_class(t, "fromhex", fromhex);
        it.reg(t, "join", join);
        it.reg(t, "find", find);
        it.reg(t, "rfind", rfind);
        it.reg(t, "index", index);
        it.reg(t, "rindex", rindex);
        it.reg(t, "count", count);
        it.reg(t, "startswith", startswith);
        it.reg(t, "endswith", endswith);
        it.reg(t, "replace", replace);
        it.reg(t, "strip", strip);
        it.reg(t, "lstrip", lstrip);
        it.reg(t, "rstrip", rstrip);
        it.reg(t, "split", split);
        it.reg(t, "rsplit", rsplit);
        it.reg(t, "splitlines", splitlines);
        it.reg(t, "partition", partition);
        it.reg(t, "rpartition", rpartition);
        it.reg(t, "upper", upper);
        it.reg(t, "lower", lower);
        it.reg(t, "capitalize", capitalize);
        it.reg(t, "swapcase", swapcase);
        it.reg(t, "title", title);
        it.reg(t, "isdigit", isdigit);
        it.reg(t, "isalpha", isalpha);
        it.reg(t, "isalnum", isalnum);
        it.reg(t, "isspace", isspace);
        it.reg(t, "isupper", isupper);
        it.reg(t, "islower", islower);
        it.reg(t, "isascii", isascii);
        it.reg(t, "zfill", zfill);
        it.reg(t, "translate", translate);
        it.reg(t, "expandtabs", expandtabs);
        it.reg_static(t, "maketrans", maketrans);
        it.reg(t, "ljust", ljust);
        it.reg(t, "rjust", rjust);
        it.reg(t, "center", center);
        it.reg(t, "removeprefix", removeprefix);
        it.reg(t, "removesuffix", removesuffix);
        it.reg(t, "__repr__", bytes_repr);
        it.reg(t, "__mod__", bytes_mod);
        reg_slots(it, t, &["__getitem__", "__len__", "__contains__", "__iter__"]);
        reg_binops(it, t, &["__add__", "__mul__", "__rmul__"]);
        reg_compare(it, t, true);
    }
    it.reg(&bytes, "__hash__", bytes_hash);
    it.reg(&bytes, "__bytes__", bytes_bytes);
    reg_slots(it, &ba, &["__setitem__", "__delitem__"]);
    it.reg(&ba, "append", ba_append);
    it.reg(&ba, "extend", ba_extend);
    it.reg(&ba, "pop", ba_pop);
    it.reg(&ba, "clear", ba_clear);
    it.reg(&ba, "insert", ba_insert);
    it.reg(&ba, "remove", ba_remove);
    it.reg(&ba, "reverse", ba_reverse);
    it.reg(&ba, "copy", ba_copy);
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
        b.try_bytes_mut().map_err(|e| crate::bind::buffer_error(self, e))
    }
}
