//! `str` methods.

use super::numeric::{reg_binops, reg_compare};
use super::slots::reg_slots;
use crate::object::*;
use crate::vm::*;
use std::rc::Rc;

type Kw<'a> = &'a [(Obj, Value)];

fn this<'a>(it: &mut Interp, a: &'a [Value], name: &str) -> R<&'a PyStr> {
    match a.first().and_then(|v| v.as_pystr()) {
        Some(s) => Ok(s),
        None => {
            let t = a.first().map(|v| it.type_name_of(v)).unwrap_or_default();
            Err(it.type_error(&format!("descriptor '{}' for 'str' objects doesn't apply to a '{}' object", name, t)))
        }
    }
}

fn str_arg<'a>(it: &mut Interp, v: &'a Value, meth: &str) -> R<&'a str> {
    match v.as_str() {
        Some(s) => Ok(s),
        None => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("{}() argument must be str, not {}", meth, t)))
        }
    }
}

fn is_py_space(c: char) -> bool {
    c.is_whitespace() || ('\x1c'..='\x1f').contains(&c)
}

fn str_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let cls = match a.first() {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => c.clone(),
        _ => return Err(it.type_error("str.__new__(X): X is not a type object")),
    };
    let b = it.bind_args("str", &a[1..], kw, &["object", "encoding", "errors"], 0)?;
    let v = match &b[0] {
        None => Value::str(""),
        Some(x) => {
            if b[1].is_some() || b[2].is_some() {
                let enc = match &b[1] {
                    Some(e) => it.str_arg(e, "str() argument 'encoding'")?,
                    None => "utf-8".into(),
                };
                let errs = match &b[2] {
                    Some(e) => it.str_arg(e, "str() argument 'errors'")?,
                    None => "strict".into(),
                };
                let data = match x {
                    Value::Obj(o) if matches!(o.kind, Kind::Bytes(_) | Kind::ByteArray(_)) => it.bytes_of(x)?,
                    _ => {
                        let t = it.type_name_of(x);
                        return Err(it.type_error(&format!("decoding to str: need a bytes-like object, {} found", t)));
                    }
                };
                Value::string(it.decode_bytes(&data, &enc, &errs)?)
            } else {
                it.str_value(x)?
            }
        }
    };
    if Rc::ptr_eq(&cls, &it.types.str_) {
        return Ok(v);
    }
    let s = v.as_str().unwrap_or("").to_string();
    Ok(Value::Obj(Object::with_cls(cls, Kind::Str(PyStr::from_box(s.into_boxed_str())))))
}

impl Interp {
    pub fn decode_bytes(&mut self, data: &[u8], enc: &str, errors: &str) -> R<String> {
        let e = enc.to_ascii_lowercase().replace('_', "-");
        match e.as_str() {
            "utf-8" | "utf8" => match std::str::from_utf8(data) {
                Ok(s) => Ok(s.to_string()),
                Err(err) => match errors {
                    "ignore" => Ok(String::from_utf8_lossy(data).replace('\u{FFFD}', "")),
                    "replace" => Ok(String::from_utf8_lossy(data).into_owned()),
                    _ => {
                        let pos = err.valid_up_to();
                        let byte = data[pos];
                        let exc = self.new_exc_str(
                            "UnicodeDecodeError",
                            &format!("'utf-8' codec can't decode byte 0x{:02x} in position {}: invalid start byte", byte, pos),
                        );
                        Err(exc)
                    }
                },
            },
            "ascii" | "us-ascii" => {
                let mut out = String::new();
                for (i, &b) in data.iter().enumerate() {
                    if b < 128 {
                        out.push(b as char);
                    } else {
                        match errors {
                            "ignore" => {}
                            "replace" => out.push('\u{FFFD}'),
                            _ => {
                                return Err(self.new_exc_str(
                                    "UnicodeDecodeError",
                                    &format!("'ascii' codec can't decode byte 0x{:02x} in position {}: ordinal not in range(128)", b, i),
                                ))
                            }
                        }
                    }
                }
                Ok(out)
            }
            "latin-1" | "latin1" | "iso-8859-1" | "iso8859-1" => Ok(data.iter().map(|&b| b as char).collect()),
            _ => Err(self.new_exc_str("LookupError", &format!("unknown encoding: {}", enc))),
        }
    }

    pub fn encode_str(&mut self, s: &str, enc: &str, errors: &str) -> R<Vec<u8>> {
        let e = enc.to_ascii_lowercase().replace('_', "-");
        match e.as_str() {
            "utf-8" | "utf8" => Ok(s.as_bytes().to_vec()),
            "ascii" | "us-ascii" | "latin-1" | "latin1" | "iso-8859-1" | "iso8859-1" => {
                let limit = if e.contains("ascii") { 128 } else { 256 };
                let name = if e.contains("ascii") { "ascii" } else { "latin-1" };
                let mut out = Vec::new();
                for (i, c) in s.chars().enumerate() {
                    if (c as u32) < limit {
                        out.push(c as u32 as u8);
                    } else {
                        match errors {
                            "ignore" => {}
                            "replace" => out.push(b'?'),
                            "backslashreplace" => out.extend(crate::repr::ascii_escape(&c.to_string()).bytes()),
                            "xmlcharrefreplace" => out.extend(format!("&#{};", c as u32).bytes()),
                            _ => {
                                let r = crate::repr::ascii_escape(&c.to_string());
                                return Err(self.new_exc_str(
                                    "UnicodeEncodeError",
                                    &format!("'{}' codec can't encode character '{}' in position {}: ordinal not in range({})", name, r, i, limit),
                                ));
                            }
                        }
                    }
                }
                Ok(out)
            }
            "utf-16" | "utf16" | "utf-16-le" => {
                let mut out = Vec::new();
                if e == "utf-16" || e == "utf16" {
                    out.extend([0xff, 0xfe]);
                }
                for u in s.encode_utf16() {
                    out.extend(u.to_le_bytes());
                }
                Ok(out)
            }
            "utf-16-be" => Ok(s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect()),
            "utf-32" | "utf-32-le" => Ok(s.chars().flat_map(|c| (c as u32).to_le_bytes()).collect()),
            _ => Err(self.new_exc_str("LookupError", &format!("unknown encoding: {}", enc))),
        }
    }
}

fn upper(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let s = this(it, a, "upper")?;
    Ok(Value::string(s.s.to_uppercase()))
}

fn lower(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let s = this(it, a, "lower")?;
    Ok(Value::string(s.s.to_lowercase()))
}

fn casefold(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let s = this(it, a, "casefold")?;
    Ok(Value::string(s.s.to_lowercase().replace('ß', "ss")))
}

fn push_title(out: &mut String, c: char) {
    match c as u32 {
        0x1C4..=0x1C6 => out.push('\u{1C5}'),
        0x1C7..=0x1C9 => out.push('\u{1C8}'),
        0x1CA..=0x1CC => out.push('\u{1CB}'),
        0x1F1..=0x1F3 => out.push('\u{1F2}'),
        _ => out.extend(c.to_uppercase()),
    }
}

fn is_cased(c: char) -> bool {
    c.is_lowercase() || c.is_uppercase() || crate::unicode::is_titlecase(c)
}

fn capitalize(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let s = this(it, a, "capitalize")?;
    let mut out = String::with_capacity(s.s.len());
    let mut chars = s.s.chars();
    if let Some(c) = chars.next() {
        push_title(&mut out, c);
    }
    for c in chars {
        out.extend(c.to_lowercase());
    }
    Ok(Value::string(out))
}

fn title(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let s = this(it, a, "title")?;
    let mut out = String::with_capacity(s.s.len());
    let mut prev_cased = false;
    for c in s.s.chars() {
        if is_cased(c) {
            if prev_cased {
                out.extend(c.to_lowercase());
            } else {
                push_title(&mut out, c);
            }
            prev_cased = true;
        } else {
            out.push(c);
            prev_cased = false;
        }
    }
    Ok(Value::string(out))
}

fn swapcase(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let s = this(it, a, "swapcase")?;
    let mut out = String::with_capacity(s.s.len());
    for c in s.s.chars() {
        if c.is_uppercase() {
            out.extend(c.to_lowercase());
        } else if c.is_lowercase() {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
    }
    Ok(Value::string(out))
}

fn strip_impl(it: &mut Interp, a: &[Value], name: &str, left: bool, right: bool) -> R<Value> {
    it.check_args(&format!("str.{}", name), a, 1, 2)?;
    let s = this(it, a, name)?;
    let chars: Option<Vec<char>> = match a.get(1) {
        Some(Value::None) | None => None,
        Some(v) => Some(str_arg(it, v, name).map(|x| x.chars().collect())?),
    };
    let pred = |c: char| match &chars {
        None => is_py_space(c),
        Some(set) => set.contains(&c),
    };
    let mut t: &str = &s.s;
    if left {
        t = t.trim_start_matches(pred);
    }
    if right {
        t = t.trim_end_matches(pred);
    }
    if t.len() == s.s.len() && a[0].as_str().is_some() && matches!(&a[0], Value::Obj(o) if o.cls.is_none()) {
        return Ok(a[0].clone());
    }
    Ok(Value::str(t))
}

fn strip(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    strip_impl(it, a, "strip", true, true)
}
fn lstrip(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    strip_impl(it, a, "lstrip", true, false)
}
fn rstrip(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    strip_impl(it, a, "rstrip", false, true)
}

fn split_impl(it: &mut Interp, a: &[Value], kw: Kw, name: &str, right: bool) -> R<Value> {
    let b = it.bind_args(name, &a[1.min(a.len())..], kw, &["sep", "maxsplit"], 0)?;
    let s = this(it, a, name)?;
    let sep: Option<String> = match &b[0] {
        None | Some(Value::None) => None,
        Some(v) => Some(str_arg(it, v, "must be str or None, not").map(|x| x.to_string()).map_err(|_| {
            let t = it.type_name_of(v);
            it.type_error(&format!("must be str or None, not {}", t))
        })?),
    };
    let max = match &b[1] {
        Some(v) => it.index_of(v)?,
        None => -1,
    };
    let mut out: Vec<Value> = Vec::new();
    match sep {
        Some(sep) => {
            if sep.is_empty() {
                return Err(it.value_error("empty separator"));
            }
            if !right {
                let mut rest: &str = &s.s;
                let mut n = 0;
                while max < 0 || n < max {
                    match rest.find(&sep) {
                        Some(i) => {
                            out.push(Value::str(&rest[..i]));
                            rest = &rest[i + sep.len()..];
                            n += 1;
                        }
                        None => break,
                    }
                }
                out.push(Value::str(rest));
            } else {
                let mut rest: &str = &s.s;
                let mut n = 0;
                let mut tail: Vec<Value> = Vec::new();
                while max < 0 || n < max {
                    match rest.rfind(&sep) {
                        Some(i) => {
                            tail.push(Value::str(&rest[i + sep.len()..]));
                            rest = &rest[..i];
                            n += 1;
                        }
                        None => break,
                    }
                }
                tail.push(Value::str(rest));
                tail.reverse();
                out = tail;
            }
        }
        None => {
            if !right {
                let mut rest: &str = &s.s;
                let mut n = 0;
                loop {
                    rest = rest.trim_start_matches(is_py_space);
                    if rest.is_empty() {
                        break;
                    }
                    if max >= 0 && n >= max {
                        out.push(Value::str(rest));
                        break;
                    }
                    let end = rest.find(is_py_space).unwrap_or(rest.len());
                    out.push(Value::str(&rest[..end]));
                    rest = &rest[end..];
                    n += 1;
                }
            } else {
                let mut rest: &str = &s.s;
                let mut n = 0;
                let mut tail: Vec<Value> = Vec::new();
                loop {
                    rest = rest.trim_end_matches(is_py_space);
                    if rest.is_empty() {
                        break;
                    }
                    if max >= 0 && n >= max {
                        tail.push(Value::str(rest));
                        break;
                    }
                    let start = rest.rfind(is_py_space).map(|i| i + rest[i..].chars().next().map_or(1, |c| c.len_utf8())).unwrap_or(0);
                    tail.push(Value::str(&rest[start..]));
                    rest = &rest[..start];
                    n += 1;
                }
                tail.reverse();
                out = tail;
            }
        }
    }
    Ok(Value::list(out))
}

fn split(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    split_impl(it, a, kw, "split", false)
}
fn rsplit(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    split_impl(it, a, kw, "rsplit", true)
}

fn splitlines(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("splitlines", &a[1.min(a.len())..], kw, &["keepends"], 0)?;
    let keep = match &b[0] {
        Some(v) => it.truthy(v)?,
        None => false,
    };
    let s = this(it, a, "splitlines")?;
    let chars: Vec<(usize, char)> = s.s.char_indices().collect();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        let (bi, c) = chars[i];
        let is_break = matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}');
        if is_break {
            let mut end = bi + c.len_utf8();
            if c == '\r' && i + 1 < chars.len() && chars[i + 1].1 == '\n' {
                i += 1;
                end += 1;
            }
            let line_end = if keep { end } else { bi };
            out.push(Value::str(&s.s[start..line_end]));
            start = end;
        }
        i += 1;
    }
    if start < s.s.len() {
        out.push(Value::str(&s.s[start..]));
    }
    Ok(Value::list(out))
}

fn partition_impl(it: &mut Interp, a: &[Value], right: bool) -> R<Value> {
    it.check_args("partition", a, 2, 2)?;
    let s = this(it, a, "partition")?;
    let sep = str_arg(it, &a[1], "partition")?;
    if sep.is_empty() {
        return Err(it.value_error("empty separator"));
    }
    let pos = if right { s.s.rfind(sep) } else { s.s.find(sep) };
    Ok(match pos {
        Some(i) => Value::tuple(vec![Value::str(&s.s[..i]), Value::str(sep), Value::str(&s.s[i + sep.len()..])]),
        None => {
            if right {
                Value::tuple(vec![Value::str(""), Value::str(""), Value::str(&s.s)])
            } else {
                Value::tuple(vec![Value::str(&s.s), Value::str(""), Value::str("")])
            }
        }
    })
}

fn partition(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    partition_impl(it, a, false)
}
fn rpartition(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    partition_impl(it, a, true)
}

fn join(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("str.join", a, 2, 2)?;
    let sep = this(it, a, "join")?.s.clone();
    let items = it.iterate_to_vec(&a[1])?;
    let mut total = sep.len().saturating_mul(items.len().saturating_sub(1));
    for v in &items {
        total = total.saturating_add(v.as_str().map_or(0, str::len));
    }
    let mut out = it.string_with_capacity(total)?;
    for (i, v) in items.iter().enumerate() {
        if i & 0x3ff == 0 {
            it.poll()?;
        }
        match v.as_str() {
            Some(s) => {
                if i > 0 {
                    out.push_str(&sep);
                }
                out.push_str(s);
            }
            None => {
                let t = it.type_name_of(v);
                return Err(it.type_error(&format!("sequence item {}: expected str instance, {} found", i, t)));
            }
        }
    }
    Ok(Value::string(out))
}

fn replace(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("replace", &a[1.min(a.len())..], kw, &["old", "new", "count"], 2)?;
    let s = this(it, a, "replace")?;
    let old = str_arg(it, b[0].as_ref().unwrap_or(&Value::None), "replace")?.to_string();
    let new = str_arg(it, b[1].as_ref().unwrap_or(&Value::None), "replace")?.to_string();
    let count = match &b[2] {
        Some(v) => it.index_of(v)?,
        None => -1,
    };
    let growth = new.len().saturating_sub(old.len());
    if growth > 0 {
        let hits = if old.is_empty() {
            let n = s.nchars + 1;
            if count < 0 { n } else { n.min(count as usize) }
        } else {
            let n = s.s.matches(old.as_str()).count();
            if count < 0 { n } else { n.min(count as usize) }
        };
        let extra = if old.is_empty() { new.len().saturating_mul(hits) } else { growth.saturating_mul(hits) };
        it.check_str_len(s.s.len().saturating_add(extra))?;
    }
    if count < 0 {
        if old.is_empty() {
            let mut out = String::new();
            for c in s.s.chars() {
                out.push_str(&new);
                out.push(c);
            }
            out.push_str(&new);
            return Ok(Value::string(out));
        }
        return Ok(Value::string(s.s.replace(&old, &new)));
    }
    if old.is_empty() {
        let mut out = String::new();
        let mut n = 0;
        for c in s.s.chars() {
            if n < count {
                out.push_str(&new);
                n += 1;
            }
            out.push(c);
        }
        if n < count {
            out.push_str(&new);
        }
        return Ok(Value::string(out));
    }
    Ok(Value::string(s.s.replacen(&old, &new, count as usize)))
}

fn opt_range(it: &mut Interp, a: &[Value], from: usize, nchars: usize) -> R<(usize, usize)> {
    let get = |it: &mut Interp, v: Option<&Value>, dflt: i64| -> R<i64> {
        match v {
            None | Some(Value::None) => Ok(dflt),
            Some(v) => it.index_of(v),
        }
    };
    let n = nchars as i64;
    let mut s = get(it, a.get(from), 0)?;
    let mut e = get(it, a.get(from + 1), n)?;
    if s < 0 {
        s = (s + n).max(0);
    }
    if e < 0 {
        e = (e + n).max(0);
    }
    Ok((s.min(n + 1) as usize, e.min(n) as usize))
}

fn find_impl(it: &mut Interp, a: &[Value], name: &str, right: bool, raise: bool) -> R<Value> {
    it.check_args(&format!("str.{}", name), a, 2, 4)?;
    let s = this(it, a, name)?;
    let sub = match a[1].as_str() {
        Some(x) => x,
        None => {
            let t = it.type_name_of(&a[1]);
            return Err(it.type_error(&format!("must be str, not {}", t)));
        }
    };
    let (st, en) = opt_range(it, a, 2, s.nchars)?;
    let found = if st > en || st > s.nchars {
        None
    } else {
        let bs = s.byte_offset(st);
        let be = s.byte_offset(en);
        let hay = &s.s[bs..be];
        let pos = if right { hay.rfind(sub) } else { hay.find(sub) };
        pos.map(|p| if s.ascii { bs + p } else { s.s[..bs + p].chars().count() })
    };
    match found {
        Some(i) => Ok(Value::Int(i as i64)),
        None => {
            if raise {
                Err(it.value_error("substring not found"))
            } else {
                Ok(Value::Int(-1))
            }
        }
    }
}

fn find(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    find_impl(it, a, "find", false, false)
}
fn rfind(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    find_impl(it, a, "rfind", true, false)
}
fn index(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    find_impl(it, a, "index", false, true)
}
fn rindex(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    find_impl(it, a, "rindex", true, true)
}

fn count(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("str.count", a, 2, 4)?;
    let s = this(it, a, "count")?;
    let sub = match a[1].as_str() {
        Some(x) => x,
        None => {
            let t = it.type_name_of(&a[1]);
            return Err(it.type_error(&format!("must be str, not {}", t)));
        }
    };
    let (st, en) = opt_range(it, a, 2, s.nchars)?;
    if st > en {
        return Ok(Value::Int(0));
    }
    let hay = s.slice(st, en);
    if sub.is_empty() {
        return Ok(Value::Int(hay.chars().count() as i64 + 1));
    }
    Ok(Value::Int(hay.matches(sub).count() as i64))
}

fn startswith_impl(it: &mut Interp, a: &[Value], name: &str, end: bool) -> R<Value> {
    it.check_args(&format!("str.{}", name), a, 2, 4)?;
    let s = this(it, a, name)?;
    let (st, en) = opt_range(it, a, 2, s.nchars)?;
    if st > en || st > s.nchars {
        return Ok(Value::Bool(false));
    }
    let hay = s.slice(st, en);
    let test = |p: &str| if end { hay.ends_with(p) } else { hay.starts_with(p) };
    match &a[1] {
        v if v.as_str().is_some() => Ok(Value::Bool(test(v.as_str().unwrap_or("")))),
        v => match v.tuple_items() {
            Some(items) => {
                for x in items {
                    match x.as_str() {
                        Some(p) => {
                            if test(p) {
                                return Ok(Value::Bool(true));
                            }
                        }
                        None => {
                            let t = it.type_name_of(x);
                            return Err(it.type_error(&format!("tuple for {} must only contain str, not {}", name, t)));
                        }
                    }
                }
                Ok(Value::Bool(false))
            }
            None => {
                let t = it.type_name_of(v);
                Err(it.type_error(&format!("{} first arg must be str or a tuple of str, not {}", name, t)))
            }
        },
    }
}

fn startswith(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    startswith_impl(it, a, "startswith", false)
}
fn endswith(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    startswith_impl(it, a, "endswith", true)
}

fn removeprefix(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("removeprefix", a, 2, 2)?;
    let s = this(it, a, "removeprefix")?;
    let p = str_arg(it, &a[1], "removeprefix")?;
    Ok(Value::str(s.s.strip_prefix(p).unwrap_or(&s.s)))
}

fn removesuffix(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("removesuffix", a, 2, 2)?;
    let s = this(it, a, "removesuffix")?;
    let p = str_arg(it, &a[1], "removesuffix")?;
    Ok(Value::str(if p.is_empty() { &s.s } else { s.s.strip_suffix(p).unwrap_or(&s.s) }))
}

fn all_chars(it: &mut Interp, a: &[Value], name: &str, nonempty: bool, f: fn(char) -> bool) -> R<Value> {
    it.check_args(&format!("str.{}", name), a, 1, 1)?;
    let s = this(it, a, name)?;
    Ok(Value::Bool((!nonempty || s.nchars > 0) && s.s.chars().all(f)))
}

fn is_decimal_char(c: char) -> bool {
    crate::unicode::is_decimal(c)
}

fn is_digit_char(c: char) -> bool {
    is_decimal_char(c) || matches!(c as u32, 0xB2 | 0xB3 | 0xB9 | 0x2070 | 0x2074..=0x2079 | 0x2080..=0x2089)
}

fn isalpha(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    all_chars(it, a, "isalpha", true, |c| c.is_alphabetic())
}
fn isalnum(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    all_chars(it, a, "isalnum", true, |c| c.is_alphanumeric())
}
fn isdigit(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    all_chars(it, a, "isdigit", true, is_digit_char)
}
fn isdecimal(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    all_chars(it, a, "isdecimal", true, is_decimal_char)
}
fn isnumeric(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    all_chars(it, a, "isnumeric", true, |c| c.is_numeric())
}
fn isspace(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    all_chars(it, a, "isspace", true, is_py_space)
}
fn isascii(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    all_chars(it, a, "isascii", false, |c| c.is_ascii())
}
fn isprintable(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    all_chars(it, a, "isprintable", false, crate::repr::is_printable)
}

fn isupper(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("str.isupper", a, 1, 1)?;
    let s = this(it, a, "isupper")?;
    let cased = s.s.chars().any(|c| c.is_uppercase() || c.is_lowercase());
    Ok(Value::Bool(cased && !s.s.chars().any(|c| c.is_lowercase())))
}

fn islower(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("str.islower", a, 1, 1)?;
    let s = this(it, a, "islower")?;
    let cased = s.s.chars().any(|c| c.is_uppercase() || c.is_lowercase());
    Ok(Value::Bool(cased && !s.s.chars().any(|c| c.is_uppercase())))
}

fn istitle(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("str.istitle", a, 1, 1)?;
    let s = this(it, a, "istitle")?;
    let mut prev_cased = false;
    let mut any = false;
    for c in s.s.chars() {
        if c.is_uppercase() {
            if prev_cased {
                return Ok(Value::Bool(false));
            }
            prev_cased = true;
            any = true;
        } else if c.is_lowercase() {
            if !prev_cased {
                return Ok(Value::Bool(false));
            }
            prev_cased = true;
            any = true;
        } else {
            prev_cased = false;
        }
    }
    Ok(Value::Bool(any))
}

fn isidentifier(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("str.isidentifier", a, 1, 1)?;
    let s = this(it, a, "isidentifier")?;
    let mut chars = s.s.chars();
    let ok = match chars.next() {
        Some(c) => crate::unicode::is_xid_start(c) && chars.all(crate::unicode::is_xid_continue),
        None => false,
    };
    Ok(Value::Bool(ok))
}

fn fill_arg(it: &mut Interp, a: &[Value], idx: usize) -> R<char> {
    match a.get(idx) {
        None => Ok(' '),
        Some(v) => match v.as_str() {
            Some(s) if s.chars().count() == 1 => Ok(s.chars().next().unwrap_or(' ')),
            _ => Err(it.type_error("The fill character must be exactly one character long")),
        },
    }
}

fn justify(it: &mut Interp, a: &[Value], name: &str, mode: u8) -> R<Value> {
    it.check_args(&format!("str.{}", name), a, 2, 3)?;
    let s = this(it, a, name)?;
    let width = it.index_of(&a[1])?;
    let fill = fill_arg(it, a, 2)?;
    let n = s.nchars as i64;
    if width <= n {
        return Ok(Value::str(&s.s));
    }
    let total = (width - n) as usize;
    it.check_str_len(total.saturating_mul(fill.len_utf8()).saturating_add(s.s.len()))?;
    let (l, r) = match mode {
        0 => (0, total),
        1 => (total, 0),
        _ => {
            let left = total / 2 + (total & width as usize & 1);
            (left, total - left)
        }
    };
    let mut out = String::new();
    out.extend(std::iter::repeat_n(fill, l));
    out.push_str(&s.s);
    out.extend(std::iter::repeat_n(fill, r));
    Ok(Value::string(out))
}

fn ljust(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    justify(it, a, "ljust", 0)
}
fn rjust(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    justify(it, a, "rjust", 1)
}
fn center(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    justify(it, a, "center", 2)
}

fn zfill(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("str.zfill", a, 2, 2)?;
    let s = this(it, a, "zfill")?;
    let width = it.index_of(&a[1])?;
    let n = s.nchars as i64;
    if width <= n {
        return Ok(Value::str(&s.s));
    }
    it.check_str_len((width - n) as usize + s.s.len())?;
    let pad = "0".repeat((width - n) as usize);
    let (sign, rest) = match s.s.chars().next() {
        Some(c @ ('+' | '-')) => (c.to_string(), &s.s[1..]),
        _ => (String::new(), &s.s[..]),
    };
    Ok(Value::string(format!("{}{}{}", sign, pad, rest)))
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
    let s = this(it, a, "expandtabs")?;
    let mut out = String::new();
    let mut col = 0i64;
    for c in s.s.chars() {
        match c {
            '\t' => {
                if ts > 0 {
                    let n = ts - (col % ts);
                    it.check_str_len(out.len() + n as usize)?;
                    out.extend(std::iter::repeat_n(' ', n as usize));
                    col += n;
                }
            }
            '\n' | '\r' => {
                out.push(c);
                col = 0;
            }
            c => {
                out.push(c);
                col += 1;
            }
        }
    }
    Ok(Value::string(out))
}

fn encode(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("encode", &a[1.min(a.len())..], kw, &["encoding", "errors"], 0)?;
    let s = this(it, a, "encode")?.s.clone();
    let enc = match &b[0] {
        Some(v) => it.str_arg(v, "encode() argument 'encoding'")?,
        None => "utf-8".into(),
    };
    let errs = match &b[1] {
        Some(v) => it.str_arg(v, "encode() argument 'errors'")?,
        None => "strict".into(),
    };
    Ok(Value::bytes(it.encode_str(&s, &enc, &errs)?))
}

fn format_m(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let s = this(it, a, "format")?.s.clone();
    Ok(Value::string(it.str_format(&s, &a[1..], kw)?))
}

fn format_map(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("format_map", a, 2, 2)?;
    let s = this(it, a, "format_map")?.s.clone();
    let mut kw: Vec<(Obj, Value)> = Vec::new();
    // Resolve names lazily by pre-collecting the mapping's keys referenced in the template.
    let keys = it.call_method(&a[1], "keys", Vec::new())?;
    for k in it.iterate_to_vec(&keys)? {
        if let Value::Obj(ko) = &k {
            if matches!(ko.kind, Kind::Str(_)) {
                let v = it.getitem(&a[1], &k)?;
                kw.push((ko.clone(), v));
            }
        }
    }
    Ok(Value::string(it.str_format(&s, &[], &kw)?))
}

fn maketrans(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("maketrans", a, 1, 3)?;
    let d = it.new_dict();
    if a.len() == 1 {
        let items = match &a[0] {
            Value::Obj(o) if matches!(o.kind, Kind::Dict(_)) => {
                let e: Vec<(Value, Value)> = match &o.kind {
                    Kind::Dict(p) => p.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect(),
                    _ => Vec::new(),
                };
                e
            }
            _ => return Err(it.type_error("if you give only one argument to maketrans it must be a dict")),
        };
        for (k, v) in items {
            let key = match k.as_str() {
                Some(s) if s.chars().count() == 1 => Value::Int(s.chars().next().map(|c| c as i64).unwrap_or(0)),
                Some(_) => return Err(it.value_error("string keys in translate table must be of length 1")),
                None => k,
            };
            it.dict_set(&d, key, v)?;
        }
        return Ok(Value::Obj(d));
    }
    let x: Vec<char> = a[0].as_str().unwrap_or("").chars().collect();
    let y: Vec<char> = a[1].as_str().unwrap_or("").chars().collect();
    if x.len() != y.len() {
        return Err(it.value_error("the first two maketrans arguments must have equal length"));
    }
    for (p, q) in x.iter().zip(y.iter()) {
        it.dict_set(&d, Value::Int(*p as i64), Value::Int(*q as i64))?;
    }
    if let Some(z) = a.get(2) {
        for c in z.as_str().unwrap_or("").chars() {
            it.dict_set(&d, Value::Int(c as i64), Value::None)?;
        }
    }
    Ok(Value::Obj(d))
}

fn translate(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("translate", a, 2, 2)?;
    let s = this(it, a, "translate")?.s.clone();
    let mut out = String::new();
    for c in s.chars() {
        match it.getitem(&a[1], &Value::Int(c as i64)) {
            Ok(Value::None) => {}
            Ok(Value::Int(i)) => match char::from_u32(i as u32) {
                Some(ch) => out.push(ch),
                None => return Err(it.value_error("character mapping must be in range(0x110000)")),
            },
            Ok(v) => match v.as_str() {
                Some(r) => out.push_str(r),
                None => return Err(it.type_error("character mapping must return integer, None or str")),
            },
            Err(e) => {
                if it.exc_is(&e, "LookupError") {
                    out.push(c);
                } else {
                    return Err(e);
                }
            }
        }
    }
    Ok(Value::string(out))
}

fn str_str(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__str__", a, 1, 1)?;
    it.str_value(&a[0])
}

fn str_getnewargs(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::tuple(vec![it.str_value(&a[0])?]))
}

pub fn init(it: &mut Interp) {
    let t = it.types.str_.clone();
    it.reg_new(&t, str_new);
    let methods: &[(&'static str, NativeFn)] = &[
        ("upper", upper),
        ("lower", lower),
        ("casefold", casefold),
        ("capitalize", capitalize),
        ("title", title),
        ("swapcase", swapcase),
        ("strip", strip),
        ("lstrip", lstrip),
        ("rstrip", rstrip),
        ("split", split),
        ("rsplit", rsplit),
        ("splitlines", splitlines),
        ("partition", partition),
        ("rpartition", rpartition),
        ("join", join),
        ("replace", replace),
        ("find", find),
        ("rfind", rfind),
        ("index", index),
        ("rindex", rindex),
        ("count", count),
        ("startswith", startswith),
        ("endswith", endswith),
        ("removeprefix", removeprefix),
        ("removesuffix", removesuffix),
        ("isalpha", isalpha),
        ("isalnum", isalnum),
        ("isdigit", isdigit),
        ("isdecimal", isdecimal),
        ("isnumeric", isnumeric),
        ("isspace", isspace),
        ("isascii", isascii),
        ("isprintable", isprintable),
        ("isupper", isupper),
        ("islower", islower),
        ("istitle", istitle),
        ("isidentifier", isidentifier),
        ("ljust", ljust),
        ("rjust", rjust),
        ("center", center),
        ("zfill", zfill),
        ("expandtabs", expandtabs),
        ("encode", encode),
        ("format", format_m),
        ("format_map", format_map),
        ("translate", translate),
        ("__str__", str_str),
        ("__getnewargs__", str_getnewargs),
    ];
    for (n, f) in methods {
        it.reg(&t, n, *f);
    }
    it.reg_static(&t, "maketrans", maketrans);
    reg_slots(it, &t, &["__getitem__", "__len__", "__contains__", "__iter__"]);
    reg_binops(it, &t, &["__add__", "__mul__", "__rmul__", "__mod__", "__rmod__"]);
    reg_compare(it, &t, true);
    let hash = |it: &mut Interp, a: &[Value], _kw: Kw| -> R<Value> { Ok(Value::Int(it.native_hash(&a[0])?)) };
    let _ = hash;
    it.reg(&t, "__hash__", str_hash);
    it.reg(&t, "__repr__", str_repr_m);
    it.reg(&t, "__format__", str_format_m);
}

fn str_hash(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(it.native_hash(&a[0])?))
}

fn str_repr_m(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::string(it.native_repr(&a[0])?))
}

fn str_format_m(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__format__", a, 2, 2)?;
    let spec = it.str_arg(&a[1], "format_spec")?;
    Ok(Value::string(it.native_format(&a[0], &spec)?))
}
