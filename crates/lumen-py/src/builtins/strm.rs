//! `str` methods.

use super::slots::{reg_binops, reg_compare, reg_slots};
use crate::bind::{KwArgs, PyCx, PyHost, This};
use crate::object::*;
use crate::unicode::Case;
use crate::vm::*;
use lumen_bind::{FromArg, Host, Passed, Slot};
use lumen_common::search;
use lumen_common::smuggle::{code_points, count_code_points, may_contain, push_code_point};
use lumen_common::ucd::{char_type, flag};
use std::rc::Rc;

/// A `str` (or subclass instance): the receiver of the str methods.
#[derive(Clone, Copy)]
pub struct StrRef<'a>(pub &'a Value, pub &'a PyStr);

impl<'a> FromArg<'a, PyHost> for StrRef<'a> {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v.as_pystr() {
            Some(s) => Ok(StrRef(v, s)),
            None => Err(cx.arg_error(at, "str", v)),
        }
    }
}

/// The `fillchar` of `ljust` / `rjust` / `center`: exactly one character.
#[derive(Clone, Copy)]
pub struct Fill<'a>(&'a str);

impl<'a> FromArg<'a, PyHost> for Fill<'a> {
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, _: Slot) -> Result<Self, Obj> {
        match v.as_str() {
            Some(s) if count_code_points(s) == 1 => Ok(Fill(s)),
            Some(_) => Err(PyHost::with_ctx(cx, |it| it.type_error("The fill character must be exactly one character long"))),
            None => Err(PyHost::with_ctx(cx, |it| {
                let t = it.type_name_of(v);
                it.type_error(&format!("The fill character must be a unicode character, not {}", t))
            })),
        }
    }
}

fn is_py_space(c: char) -> bool {
    crate::unicode::is_space(c as u32)
}

fn must_be_str<'a>(it: &mut Interp, v: &'a Value) -> R<&'a str> {
    match v.as_str() {
        Some(s) => Ok(s),
        None => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("must be str, not {}", t)))
        }
    }
}

impl Interp {
    pub fn decode_bytes(&mut self, data: &[u8], enc: &str, errors: &str) -> R<String> {
        crate::codecs::decode(self, data, enc, errors)
    }

    pub fn encode_str(&mut self, s: &str, enc: &str, errors: &str) -> R<Vec<u8>> {
        crate::codecs::encode(self, s, enc, errors)
    }
}

fn strip_impl(it: &mut Interp, slf: StrRef<'_>, chars: Option<&Value>, name: &str, left: bool, right: bool) -> R<Value> {
    let StrRef(v, s) = slf;
    let chars: Option<Vec<u32>> = match chars {
        Some(Value::None) | None => None,
        Some(c) => match c.as_str() {
            Some(x) => Some(code_points(x).collect()),
            None => return Err(it.type_error(&format!("{} arg must be None or str", name))),
        },
    };
    let pred = |c: u32| match &chars {
        None => char::from_u32(c).is_some_and(is_py_space),
        Some(set) => set.contains(&c),
    };
    let mut t: &str = &s.s;
    if may_contain(t) {
        let (mut a, mut b) = (0, t.len());
        let mut cps = code_points(t);
        let mut first = true;
        while let Some(c) = cps.next() {
            if pred(c) {
                if first && left {
                    a = cps.offset();
                }
            } else {
                first = false;
                b = cps.offset();
            }
        }
        if first {
            b = a;
        }
        if !right {
            b = t.len();
        }
        t = &t[a..b];
    } else {
        let pred = |c: char| pred(c as u32);
        if left {
            t = t.trim_start_matches(pred);
        }
        if right {
            t = t.trim_end_matches(pred);
        }
    }
    if t.len() == s.s.len() && matches!(v, Value::Obj(o) if o.cls.is_none()) {
        return Ok(v.clone());
    }
    Ok(Value::str(t))
}

fn split_impl(it: &mut Interp, s: &PyStr, sep: Option<&Value>, max: isize, right: bool) -> R<Value> {
    let sep: Option<&str> = match sep {
        None | Some(Value::None) => None,
        Some(v) => match v.as_str() {
            Some(x) => Some(x),
            None => {
                let t = it.type_name_of(v);
                return Err(it.type_error(&format!("must be str or None, not {}", t)));
            }
        },
    };
    let max = max as i64;
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
                    match rest.find(sep) {
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
                while max < 0 || n < max {
                    match rest.rfind(sep) {
                        Some(i) => {
                            out.push(Value::str(&rest[i + sep.len()..]));
                            rest = &rest[..i];
                            n += 1;
                        }
                        None => break,
                    }
                }
                out.push(Value::str(rest));
                out.reverse();
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
                loop {
                    rest = rest.trim_end_matches(is_py_space);
                    if rest.is_empty() {
                        break;
                    }
                    if max >= 0 && n >= max {
                        out.push(Value::str(rest));
                        break;
                    }
                    let start = rest.rfind(is_py_space).map(|i| i + rest[i..].chars().next().map_or(1, |c| c.len_utf8())).unwrap_or(0);
                    out.push(Value::str(&rest[start..]));
                    rest = &rest[..start];
                    n += 1;
                }
                out.reverse();
            }
        }
    }
    Ok(Value::list(out))
}

fn partition_impl(it: &mut Interp, s: &PyStr, sep: &Value, right: bool) -> R<Value> {
    let sep = must_be_str(it, sep)?;
    if sep.is_empty() {
        return Err(it.value_error("empty separator"));
    }
    let pos = if right { s.s.rfind(sep) } else { s.s.find(sep) };
    Ok(match pos {
        Some(i) => Value::tuple(vec![Value::str(&s.s[..i]), Value::str(sep), Value::str(&s.s[i + sep.len()..])]),
        None if right => Value::tuple(vec![Value::str(""), Value::str(""), Value::str(&s.s)]),
        None => Value::tuple(vec![Value::str(&s.s), Value::str(""), Value::str("")]),
    })
}

/// The optional `start` / `end` of `find` and friends (`_PyEval_SliceIndex`), clamped to
/// `0..=nchars` (`start` may reach `nchars + 1`, so a search past the end finds nothing).
fn opt_range(it: &mut Interp, start: Option<&Value>, end: Option<&Value>, nchars: usize) -> R<(usize, usize)> {
    let get = |it: &mut Interp, v: Option<&Value>, dflt: i64| -> R<i64> {
        match v {
            None | Some(Value::None) => Ok(dflt),
            Some(Value::Int(i)) => Ok(*i),
            Some(v) if !it.has_index(v) => Err(it.type_error("slice indices must be integers or None or have an __index__ method")),
            Some(v) => it.slice_index(v),
        }
    };
    let n = nchars as i64;
    let mut s = get(it, start, 0)?;
    let mut e = get(it, end, n)?;
    if s < 0 {
        s = s.saturating_add(n).max(0);
    }
    if e < 0 {
        e = e.saturating_add(n).max(0);
    }
    Ok((s.min(n + 1) as usize, e.min(n) as usize))
}

fn find_impl(it: &mut Interp, s: &PyStr, sub: &Value, start: Option<&Value>, end: Option<&Value>, right: bool, raise: bool) -> R<i64> {
    let sub = must_be_str(it, sub)?;
    let (st, en) = opt_range(it, start, end, s.nchars)?;
    let found = if st > en || st > s.nchars {
        None
    } else {
        let bs = s.byte_offset(st);
        let be = s.byte_offset(en);
        let hay = &s.s[bs..be];
        let pos = if right { hay.rfind(sub) } else { search::find(hay.as_bytes(), sub.as_bytes()) };
        pos.map(|p| if s.ascii { bs + p } else { count_code_points(&s.s[..bs + p]) })
    };
    match found {
        Some(i) => Ok(i as i64),
        None if raise => Err(it.value_error("substring not found")),
        None => Ok(-1),
    }
}

fn startswith_impl(it: &mut Interp, s: &PyStr, prefix: &Value, start: Option<&Value>, end: Option<&Value>, name: &str, at_end: bool) -> R<bool> {
    let (st, en) = opt_range(it, start, end, s.nchars)?;
    let hay = if st > en || st > s.nchars { None } else { Some(s.slice(st, en)) };
    let test = |p: &str| hay.is_some_and(|h| if at_end { h.ends_with(p) } else { h.starts_with(p) });
    if let Some(p) = prefix.as_str() {
        return Ok(test(p));
    }
    match prefix.tuple_items() {
        Some(items) => {
            for x in items {
                match x.as_str() {
                    Some(p) => {
                        if test(p) {
                            return Ok(true);
                        }
                    }
                    None => {
                        let t = it.type_name_of(x);
                        return Err(it.type_error(&format!("tuple for {} must only contain str, not {}", name, t)));
                    }
                }
            }
            Ok(false)
        }
        None => {
            let t = it.type_name_of(prefix);
            Err(it.type_error(&format!("{} first arg must be str or a tuple of str, not {}", name, t)))
        }
    }
}

fn all_chars(s: &PyStr, nonempty: bool, f: impl Fn(u32) -> bool) -> bool {
    (!nonempty || s.nchars > 0) && code_points(&s.s).all(f)
}

fn all_have(s: &PyStr, f: u16) -> bool {
    all_chars(s, true, |c| crate::unicode::has(c, f))
}

/// `str.isupper` (`upper`) or `str.islower`: some cased character, and none of the other case
/// or titlecase.
fn is_one_case(s: &PyStr, upper: bool) -> bool {
    let (want, other) = if upper { (flag::UPPER, flag::LOWER) } else { (flag::LOWER, flag::UPPER) };
    let mut cased = false;
    for c in code_points(&s.s) {
        let t = char_type(c);
        if t.is(other | flag::TITLE) {
            return false;
        }
        cased |= t.is(want);
    }
    cased
}

fn justify(it: &mut Interp, slf: StrRef<'_>, width: isize, fill: Passed<Fill<'_>>, mode: u8) -> R<Value> {
    let StrRef(v, s) = slf;
    let fill = fill.0.map_or(" ", |f| f.0);
    let n = s.nchars as i64;
    let width = width as i64;
    if width <= n {
        return Ok(if matches!(v, Value::Obj(o) if o.cls.is_none()) { v.clone() } else { Value::str(&s.s) });
    }
    let total = (width - n) as usize;
    it.check_str_len(total.saturating_mul(fill.len()).saturating_add(s.s.len()))?;
    let (l, r) = match mode {
        0 => (0, total),
        1 => (total, 0),
        _ => {
            let left = total / 2 + (total & width as usize & 1);
            (left, total - left)
        }
    };
    let mut out = fill.repeat(l);
    out.push_str(&s.s);
    out.push_str(&fill.repeat(r));
    Ok(Value::string(out))
}

#[lumen_bind::class(name = "str")]
/// str(object='') -> str
/// str(bytes_or_buffer[, encoding[, errors]]) -> str
///
/// Create a new string object from the given object. If encoding or
/// errors is specified, then the object must expose a data buffer
/// that will be decoded using the given encoding and error handler.
/// Otherwise, returns the result of object.__str__() (if defined)
/// or repr(object).
/// encoding defaults to sys.getdefaultencoding().
/// errors defaults to 'strict'.
pub struct Str;

#[lumen_bind::methods]
impl Str {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, #[kw] object: Passed<&Value>, #[kw] encoding: Passed<&str>, #[kw] errors: Passed<&str>) -> R<Value> {
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        let v = match object.0 {
            None => Value::str(""),
            Some(x) if encoding.0.is_none() && errors.0.is_none() => it.str_value(x)?,
            Some(x) => {
                let data = match x {
                    Value::Obj(o) if matches!(o.kind, Kind::Bytes(_) | Kind::ByteArray(_)) => it.bytes_of(x)?,
                    Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => return Err(it.type_error("decoding str is not supported")),
                    _ => {
                        let t = it.type_name_of(x);
                        return Err(it.type_error(&format!("decoding to str: need a bytes-like object, {} found", t)));
                    }
                };
                Value::string(it.decode_bytes(&data, encoding.0.unwrap_or("utf-8"), errors.0.unwrap_or("strict"))?)
            }
        };
        if Rc::ptr_eq(cls, &it.types.str_) {
            return Ok(v);
        }
        let s = v.as_str().unwrap_or("").to_string();
        Ok(Value::Obj(Object::with_cls(cls.clone(), Kind::Str(PyStr::from_box(s.into_boxed_str())))))
    }

    /// Return a copy of the string converted to uppercase.
    #[method]
    fn upper(slf: This<StrRef<'_>>) -> String {
        crate::unicode::convert(&slf.0 .1.s, Case::Upper)
    }

    /// Return a copy of the string converted to lowercase.
    #[method]
    fn lower(slf: This<StrRef<'_>>) -> String {
        crate::unicode::convert(&slf.0 .1.s, Case::Lower)
    }

    /// Return a version of the string suitable for caseless comparisons.
    #[method]
    fn casefold(slf: This<StrRef<'_>>) -> String {
        crate::unicode::convert(&slf.0 .1.s, Case::Fold)
    }

    /// Return a capitalized version of the string.
    ///
    /// More specifically, make the first character have upper case and the rest lower
    /// case.
    #[method]
    fn capitalize(slf: This<StrRef<'_>>) -> String {
        crate::unicode::convert(&slf.0 .1.s, Case::Capitalize)
    }

    /// Return a version of the string where each word is titlecased.
    ///
    /// More specifically, words start with uppercased characters and all remaining
    /// cased characters have lower case.
    #[method]
    fn title(slf: This<StrRef<'_>>) -> String {
        crate::unicode::convert(&slf.0 .1.s, Case::Title)
    }

    /// Convert uppercase characters to lowercase and lowercase characters to uppercase.
    #[method]
    fn swapcase(slf: This<StrRef<'_>>) -> String {
        crate::unicode::convert(&slf.0 .1.s, Case::SwapCase)
    }

    /// Return a copy of the string with leading and trailing whitespace removed.
    ///
    /// If chars is given and not None, remove characters in chars instead.
    #[method]
    fn strip(slf: This<StrRef<'_>>, it: &mut Interp, chars: Option<&Value>) -> R<Value> {
        strip_impl(it, slf.0, chars, "strip", true, true)
    }

    /// Return a copy of the string with leading whitespace removed.
    ///
    /// If chars is given and not None, remove characters in chars instead.
    #[method]
    fn lstrip(slf: This<StrRef<'_>>, it: &mut Interp, chars: Option<&Value>) -> R<Value> {
        strip_impl(it, slf.0, chars, "lstrip", true, false)
    }

    /// Return a copy of the string with trailing whitespace removed.
    ///
    /// If chars is given and not None, remove characters in chars instead.
    #[method]
    fn rstrip(slf: This<StrRef<'_>>, it: &mut Interp, chars: Option<&Value>) -> R<Value> {
        strip_impl(it, slf.0, chars, "rstrip", false, true)
    }

    /// Return a list of the substrings in the string, using sep as the separator string.
    ///
    ///   sep
    ///     The separator used to split the string.
    ///
    ///     When set to None (the default value), will split on any whitespace
    ///     character (including \n \r \t \f and spaces) and will discard
    ///     empty strings from the result.
    ///   maxsplit
    ///     Maximum number of splits.
    ///     -1 (the default value) means no limit.
    ///
    /// Splitting starts at the front of the string and works to the end.
    ///
    /// Note, str.split() is mainly useful for data that has been intentionally
    /// delimited.  With natural text that includes punctuation, consider using
    /// the regular expression module.
    #[method]
    fn split(slf: This<StrRef<'_>>, it: &mut Interp, #[kw] sep: Option<&Value>, #[kw] #[default(-1)] maxsplit: isize) -> R<Value> {
        split_impl(it, slf.0 .1, sep, maxsplit, false)
    }

    /// Return a list of the substrings in the string, using sep as the separator string.
    ///
    ///   sep
    ///     The separator used to split the string.
    ///
    ///     When set to None (the default value), will split on any whitespace
    ///     character (including \n \r \t \f and spaces) and will discard
    ///     empty strings from the result.
    ///   maxsplit
    ///     Maximum number of splits.
    ///     -1 (the default value) means no limit.
    ///
    /// Splitting starts at the end of the string and works to the front.
    #[method]
    fn rsplit(slf: This<StrRef<'_>>, it: &mut Interp, #[kw] sep: Option<&Value>, #[kw] #[default(-1)] maxsplit: isize) -> R<Value> {
        split_impl(it, slf.0 .1, sep, maxsplit, true)
    }

    /// Return a list of the lines in the string, breaking at line boundaries.
    ///
    /// Line breaks are not included in the resulting list unless keepends is given and
    /// true.
    #[method]
    fn splitlines(slf: This<StrRef<'_>>, #[kw] #[default(false)] keepends: bool) -> Value {
        let s = &slf.0 .1.s;
        let mut out = Vec::new();
        let mut start = 0;
        let mut chars = s.char_indices().peekable();
        while let Some((bi, c)) = chars.next() {
            if matches!(c, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}') {
                let mut end = bi + c.len_utf8();
                if c == '\r' && chars.peek().is_some_and(|&(_, d)| d == '\n') {
                    chars.next();
                    end += 1;
                }
                out.push(Value::str(&s[start..if keepends { end } else { bi }]));
                start = end;
            }
        }
        if start < s.len() {
            out.push(Value::str(&s[start..]));
        }
        Value::list(out)
    }

    /// Partition the string into three parts using the given separator.
    ///
    /// This will search for the separator in the string.  If the separator is found,
    /// returns a 3-tuple containing the part before the separator, the separator
    /// itself, and the part after it.
    ///
    /// If the separator is not found, returns a 3-tuple containing the original string
    /// and two empty strings.
    #[method]
    fn partition(slf: This<StrRef<'_>>, it: &mut Interp, sep: &Value) -> R<Value> {
        partition_impl(it, slf.0 .1, sep, false)
    }

    /// Partition the string into three parts using the given separator.
    ///
    /// This will search for the separator in the string, starting at the end. If
    /// the separator is found, returns a 3-tuple containing the part before the
    /// separator, the separator itself, and the part after it.
    ///
    /// If the separator is not found, returns a 3-tuple containing two empty strings
    /// and the original string.
    #[method]
    fn rpartition(slf: This<StrRef<'_>>, it: &mut Interp, sep: &Value) -> R<Value> {
        partition_impl(it, slf.0 .1, sep, true)
    }

    /// Concatenate any number of strings.
    ///
    /// The string whose method is called is inserted in between each given string.
    /// The result is returned as a new string.
    ///
    /// Example: '.'.join(['ab', 'pq', 'rs']) -> 'ab.pq.rs'
    #[method]
    fn join(slf: This<StrRef<'_>>, it: &mut Interp, iterable: &Value) -> R<Value> {
        let sep = &slf.0 .1.s;
        let items = match iterable {
            Value::Obj(o) if o.cls.is_none() && matches!(o.kind, Kind::List(_) | Kind::Tuple(_)) => it.iterate_to_vec(iterable)?,
            _ => {
                let iter = match it.get_iter(iterable) {
                    Ok(i) => i,
                    Err(e) if it.exc_is(&e, "TypeError") => return Err(it.type_error("can only join an iterable")),
                    Err(e) => return Err(e),
                };
                it.iterate_to_vec(&iter)?
            }
        };
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
                        out.push_str(sep);
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

    /// Return a copy with all occurrences of substring old replaced by new.
    ///
    ///   count
    ///     Maximum number of occurrences to replace.
    ///     -1 (the default value) means replace all occurrences.
    ///
    /// If the optional argument count is given, only the first count occurrences are
    /// replaced.
    #[method]
    fn replace(slf: This<StrRef<'_>>, it: &mut Interp, old: &str, new: &str, #[kw] #[default(-1)] count: isize) -> R<Value> {
        let s = slf.0 .1;
        let count = count as i64;
        let growth = new.len().saturating_sub(old.len());
        if growth > 0 {
            let n = if old.is_empty() { s.nchars + 1 } else { s.s.matches(old).count() };
            let hits = if count < 0 { n } else { n.min(count as usize) };
            let extra = if old.is_empty() { new.len().saturating_mul(hits) } else { growth.saturating_mul(hits) };
            it.check_str_len(s.s.len().saturating_add(extra))?;
        }
        if old.is_empty() {
            let limit = if count < 0 { usize::MAX } else { count as usize };
            let mut out = String::new();
            let mut n = 0;
            for c in s.s.chars() {
                if n < limit {
                    out.push_str(new);
                    n += 1;
                }
                out.push(c);
            }
            if n < limit {
                out.push_str(new);
            }
            return Ok(Value::string(out));
        }
        if count < 0 {
            return Ok(Value::string(s.s.replace(old, new)));
        }
        Ok(Value::string(s.s.replacen(old, new, count as usize)))
    }

    /// S.find(sub[, start[, end]]) -> int
    ///
    /// Return the lowest index in S where substring sub is found,
    /// such that sub is contained within S[start:end].  Optional
    /// arguments start and end are interpreted as in slice notation.
    ///
    /// Return -1 on failure.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn find(slf: This<StrRef<'_>>, it: &mut Interp, sub: &Value, start: Option<&Value>, end: Option<&Value>) -> R<i64> {
        find_impl(it, slf.0 .1, sub, start, end, false, false)
    }

    /// S.rfind(sub[, start[, end]]) -> int
    ///
    /// Return the highest index in S where substring sub is found,
    /// such that sub is contained within S[start:end].  Optional
    /// arguments start and end are interpreted as in slice notation.
    ///
    /// Return -1 on failure.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn rfind(slf: This<StrRef<'_>>, it: &mut Interp, sub: &Value, start: Option<&Value>, end: Option<&Value>) -> R<i64> {
        find_impl(it, slf.0 .1, sub, start, end, true, false)
    }

    /// S.index(sub[, start[, end]]) -> int
    ///
    /// Return the lowest index in S where substring sub is found,
    /// such that sub is contained within S[start:end].  Optional
    /// arguments start and end are interpreted as in slice notation.
    ///
    /// Raises ValueError when the substring is not found.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn index(slf: This<StrRef<'_>>, it: &mut Interp, sub: &Value, start: Option<&Value>, end: Option<&Value>) -> R<i64> {
        find_impl(it, slf.0 .1, sub, start, end, false, true)
    }

    /// S.rindex(sub[, start[, end]]) -> int
    ///
    /// Return the highest index in S where substring sub is found,
    /// such that sub is contained within S[start:end].  Optional
    /// arguments start and end are interpreted as in slice notation.
    ///
    /// Raises ValueError when the substring is not found.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn rindex(slf: This<StrRef<'_>>, it: &mut Interp, sub: &Value, start: Option<&Value>, end: Option<&Value>) -> R<i64> {
        find_impl(it, slf.0 .1, sub, start, end, true, true)
    }

    /// S.count(sub[, start[, end]]) -> int
    ///
    /// Return the number of non-overlapping occurrences of substring sub in
    /// string S[start:end].  Optional arguments start and end are
    /// interpreted as in slice notation.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn count(slf: This<StrRef<'_>>, it: &mut Interp, sub: &Value, start: Option<&Value>, end: Option<&Value>) -> R<i64> {
        let s = slf.0 .1;
        let sub = must_be_str(it, sub)?;
        let (st, en) = opt_range(it, start, end, s.nchars)?;
        if st > en {
            return Ok(0);
        }
        let hay = s.slice(st, en);
        if sub.is_empty() {
            return Ok(count_code_points(hay) as i64 + 1);
        }
        Ok(search::count(hay.as_bytes(), sub.as_bytes(), usize::MAX) as i64)
    }

    /// S.startswith(prefix[, start[, end]]) -> bool
    ///
    /// Return True if S starts with the specified prefix, False otherwise.
    /// With optional start, test S beginning at that position.
    /// With optional end, stop comparing S at that position.
    /// prefix can also be a tuple of strings to try.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn startswith(slf: This<StrRef<'_>>, it: &mut Interp, prefix: &Value, start: Option<&Value>, end: Option<&Value>) -> R<bool> {
        startswith_impl(it, slf.0 .1, prefix, start, end, "startswith", false)
    }

    /// S.endswith(suffix[, start[, end]]) -> bool
    ///
    /// Return True if S ends with the specified suffix, False otherwise.
    /// With optional start, test S beginning at that position.
    /// With optional end, stop comparing S at that position.
    /// suffix can also be a tuple of strings to try.
    #[method(hint(py(arg_style = "parse", text_signature = "")))]
    fn endswith(slf: This<StrRef<'_>>, it: &mut Interp, suffix: &Value, start: Option<&Value>, end: Option<&Value>) -> R<bool> {
        startswith_impl(it, slf.0 .1, suffix, start, end, "endswith", true)
    }

    /// Return a str with the given prefix string removed if present.
    ///
    /// If the string starts with the prefix string, return string[len(prefix):].
    /// Otherwise, return a copy of the original string.
    #[method]
    fn removeprefix(slf: This<StrRef<'_>>, prefix: &str) -> Value {
        let s = &slf.0 .1.s;
        Value::str(s.strip_prefix(prefix).unwrap_or(s))
    }

    /// Return a str with the given suffix string removed if present.
    ///
    /// If the string ends with the suffix string and that suffix is not empty,
    /// return string[:-len(suffix)]. Otherwise, return a copy of the original
    /// string.
    #[method]
    fn removesuffix(slf: This<StrRef<'_>>, suffix: &str) -> Value {
        let s = &slf.0 .1.s;
        Value::str(if suffix.is_empty() { s } else { s.strip_suffix(suffix).unwrap_or(s) })
    }

    /// Return True if the string is an alphabetic string, False otherwise.
    ///
    /// A string is alphabetic if all characters in the string are alphabetic and there
    /// is at least one character in the string.
    #[method]
    fn isalpha(slf: This<StrRef<'_>>) -> bool {
        all_have(slf.0 .1, flag::ALPHA)
    }

    /// Return True if the string is an alpha-numeric string, False otherwise.
    ///
    /// A string is alpha-numeric if all characters in the string are alpha-numeric and
    /// there is at least one character in the string.
    #[method]
    fn isalnum(slf: This<StrRef<'_>>) -> bool {
        all_have(slf.0 .1, flag::ALPHA | flag::DECIMAL | flag::DIGIT | flag::NUMERIC)
    }

    /// Return True if the string is a digit string, False otherwise.
    ///
    /// A string is a digit string if all characters in the string are digits and there
    /// is at least one character in the string.
    #[method]
    fn isdigit(slf: This<StrRef<'_>>) -> bool {
        all_have(slf.0 .1, flag::DIGIT)
    }

    /// Return True if the string is a decimal string, False otherwise.
    ///
    /// A string is a decimal string if all characters in the string are decimal and
    /// there is at least one character in the string.
    #[method]
    fn isdecimal(slf: This<StrRef<'_>>) -> bool {
        all_have(slf.0 .1, flag::DECIMAL)
    }

    /// Return True if the string is a numeric string, False otherwise.
    ///
    /// A string is numeric if all characters in the string are numeric and there is at
    /// least one character in the string.
    #[method]
    fn isnumeric(slf: This<StrRef<'_>>) -> bool {
        all_have(slf.0 .1, flag::NUMERIC)
    }

    /// Return True if the string is a whitespace string, False otherwise.
    ///
    /// A string is whitespace if all characters in the string are whitespace and there
    /// is at least one character in the string.
    #[method]
    fn isspace(slf: This<StrRef<'_>>) -> bool {
        all_chars(slf.0 .1, true, crate::unicode::is_space)
    }

    /// Return True if all characters in the string are ASCII, False otherwise.
    ///
    /// ASCII characters have code points in the range U+0000-U+007F.
    /// Empty string is ASCII too.
    #[method]
    fn isascii(slf: This<StrRef<'_>>) -> bool {
        slf.0 .1.ascii || all_chars(slf.0 .1, false, |c| c < 0x80)
    }

    /// Return True if all characters in the string are printable, False otherwise.
    ///
    /// A character is printable if repr() may use it in its output.
    #[method]
    fn isprintable(slf: This<StrRef<'_>>) -> bool {
        all_chars(slf.0 .1, false, crate::unicode::is_printable)
    }

    /// Return True if the string is an uppercase string, False otherwise.
    ///
    /// A string is uppercase if all cased characters in the string are uppercase and
    /// there is at least one cased character in the string.
    #[method]
    fn isupper(slf: This<StrRef<'_>>) -> bool {
        is_one_case(slf.0 .1, true)
    }

    /// Return True if the string is a lowercase string, False otherwise.
    ///
    /// A string is lowercase if all cased characters in the string are lowercase and
    /// there is at least one cased character in the string.
    #[method]
    fn islower(slf: This<StrRef<'_>>) -> bool {
        is_one_case(slf.0 .1, false)
    }

    /// Return True if the string is a title-cased string, False otherwise.
    ///
    /// In a title-cased string, upper- and title-case characters may only
    /// follow uncased characters and lowercase characters only cased ones.
    #[method]
    fn istitle(slf: This<StrRef<'_>>) -> bool {
        let mut prev_cased = false;
        let mut cased = false;
        for c in code_points(&slf.0 .1.s) {
            let t = char_type(c);
            if t.is(flag::UPPER | flag::TITLE) {
                if prev_cased {
                    return false;
                }
                prev_cased = true;
                cased = true;
            } else if t.is(flag::LOWER) {
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

    /// Return True if the string is a valid Python identifier, False otherwise.
    ///
    /// Call keyword.iskeyword(s) to test whether string s is a reserved identifier,
    /// such as "def" or "class".
    #[method]
    fn isidentifier(slf: This<StrRef<'_>>) -> bool {
        let mut cps = code_points(&slf.0 .1.s);
        match cps.next() {
            Some(c) => (c == '_' as u32 || crate::unicode::has(c, flag::XID_START)) && cps.all(|c| crate::unicode::has(c, flag::XID_CONTINUE)),
            None => false,
        }
    }

    /// Return a left-justified string of length width.
    ///
    /// Padding is done using the specified fill character (default is a space).
    #[method(hint(py(text_signature = "($self, width, fillchar=' ', /)")))]
    fn ljust(slf: This<StrRef<'_>>, it: &mut Interp, width: isize, fillchar: Passed<Fill<'_>>) -> R<Value> {
        justify(it, slf.0, width, fillchar, 0)
    }

    /// Return a right-justified string of length width.
    ///
    /// Padding is done using the specified fill character (default is a space).
    #[method(hint(py(text_signature = "($self, width, fillchar=' ', /)")))]
    fn rjust(slf: This<StrRef<'_>>, it: &mut Interp, width: isize, fillchar: Passed<Fill<'_>>) -> R<Value> {
        justify(it, slf.0, width, fillchar, 1)
    }

    /// Return a centered string of length width.
    ///
    /// Padding is done using the specified fill character (default is a space).
    #[method(hint(py(text_signature = "($self, width, fillchar=' ', /)")))]
    fn center(slf: This<StrRef<'_>>, it: &mut Interp, width: isize, fillchar: Passed<Fill<'_>>) -> R<Value> {
        justify(it, slf.0, width, fillchar, 2)
    }

    /// Pad a numeric string with zeros on the left, to fill a field of the given width.
    ///
    /// The string is never truncated.
    #[method]
    fn zfill(slf: This<StrRef<'_>>, it: &mut Interp, width: isize) -> R<Value> {
        let StrRef(v, s) = slf.0;
        let (n, width) = (s.nchars as i64, width as i64);
        if width <= n {
            return Ok(if matches!(v, Value::Obj(o) if o.cls.is_none()) { v.clone() } else { Value::str(&s.s) });
        }
        it.check_str_len((width - n) as usize + s.s.len())?;
        let pad = "0".repeat((width - n) as usize);
        let (sign, rest) = match s.s.as_bytes().first() {
            Some(b'+' | b'-') => (&s.s[..1], &s.s[1..]),
            _ => ("", &s.s[..]),
        };
        Ok(Value::string(format!("{}{}{}", sign, pad, rest)))
    }

    /// Return a copy where all tab characters are expanded using spaces.
    ///
    /// If tabsize is not given, a tab size of 8 characters is assumed.
    #[method]
    fn expandtabs(slf: This<StrRef<'_>>, it: &mut Interp, #[kw] #[default(8)] tabsize: i32) -> R<Value> {
        let out = lumen_common::text::expand_tabs(&slf.0 .1.s, tabsize as i64, |n| it.check_str_len(n))?;
        Ok(Value::string(out))
    }

    /// Encode the string using the codec registered for encoding.
    ///
    ///   encoding
    ///     The encoding in which to encode the string.
    ///   errors
    ///     The error handling scheme to use for encoding errors.
    ///     The default is 'strict' meaning that encoding errors raise a
    ///     UnicodeEncodeError.  Other possible values are 'ignore', 'replace' and
    ///     'xmlcharrefreplace' as well as any other name registered with
    ///     codecs.register_error that can handle UnicodeEncodeErrors.
    #[method]
    fn encode(slf: This<StrRef<'_>>, it: &mut Interp, #[kw] #[default("utf-8")] encoding: &str, #[kw] #[default("strict")] errors: &str) -> R<Value> {
        Ok(Value::bytes(it.encode_str(&slf.0 .1.s, encoding, errors)?))
    }

    /// S.format(*args, **kwargs) -> str
    ///
    /// Return a formatted version of S, using substitutions from args and kwargs.
    /// The substitutions are identified by braces ('{' and '}').
    #[method(hint(py(text_signature = "")))]
    fn format(slf: This<StrRef<'_>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<String> {
        it.str_format(&slf.0 .1.s, args, &kwargs.to_vec())
    }

    /// S.format_map(mapping) -> str
    ///
    /// Return a formatted version of S, using substitutions from mapping.
    /// The substitutions are identified by braces ('{' and '}').
    #[method(hint(py(text_signature = "")))]
    fn format_map(slf: This<StrRef<'_>>, it: &mut Interp, mapping: &Value) -> R<String> {
        let mut kw: Vec<(Obj, Value)> = Vec::new();
        // The names are looked up front: every str key of the mapping.
        let keys = it.call_method(mapping, "keys", Vec::new())?;
        for k in it.iterate_to_vec(&keys)? {
            if let Value::Obj(ko) = &k {
                if matches!(ko.kind, Kind::Str(_)) {
                    let v = it.getitem(mapping, &k)?;
                    kw.push((ko.clone(), v));
                }
            }
        }
        it.str_format(&slf.0 .1.s, &[], &kw)
    }

    /// Return a translation table usable for str.translate().
    ///
    /// If there is only one argument, it must be a dictionary mapping Unicode
    /// ordinals (integers) or characters to Unicode ordinals, strings or None.
    /// Character keys will be then converted to ordinals.
    /// If there are two arguments, they must be strings of equal length, and
    /// in the resulting dictionary, each character in x will be mapped to the
    /// character at the same position in y. If there is a third argument, it
    /// must be a string, whose characters will be mapped to None in the result.
    #[method]
    fn maketrans(it: &mut Interp, x: &Value, y: Passed<&str>, z: Passed<&str>) -> R<Value> {
        let d = it.new_dict();
        let Some(y) = y.0 else {
            let items: Vec<(Value, Value)> = match x {
                Value::Obj(o) => match &o.kind {
                    Kind::Dict(p) => p.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect(),
                    _ => return Err(it.type_error("if you give only one argument to maketrans it must be a dict")),
                },
                _ => return Err(it.type_error("if you give only one argument to maketrans it must be a dict")),
            };
            for (k, v) in items {
                let key = match k.as_str() {
                    Some(s) if count_code_points(s) == 1 => Value::Int(code_points(s).next().unwrap_or(0) as i64),
                    Some(_) => return Err(it.value_error("string keys in translate table must be of length 1")),
                    None => k,
                };
                it.dict_set(&d, key, v)?;
            }
            return Ok(Value::Obj(d));
        };
        let Some(x) = x.as_str() else {
            return Err(it.type_error("first maketrans argument must be a string if there is a second argument"));
        };
        let xs: Vec<u32> = code_points(x).collect();
        let ys: Vec<u32> = code_points(y).collect();
        if xs.len() != ys.len() {
            return Err(it.value_error("the first two maketrans arguments must have equal length"));
        }
        for (p, q) in xs.iter().zip(ys.iter()) {
            it.dict_set(&d, Value::Int(*p as i64), Value::Int(*q as i64))?;
        }
        if let Some(z) = z.0 {
            for c in code_points(z) {
                it.dict_set(&d, Value::Int(c as i64), Value::None)?;
            }
        }
        Ok(Value::Obj(d))
    }

    /// Replace each character in the string using the given translation table.
    ///
    ///   table
    ///     Translation table, which must be a mapping of Unicode ordinals to
    ///     Unicode ordinals, strings, or None.
    ///
    /// The table must implement lookup/indexing via __getitem__, for instance a
    /// dictionary or list.  If this operation raises LookupError, the character is
    /// left untouched.  Characters mapped to None are deleted.
    #[method]
    fn translate(slf: This<StrRef<'_>>, it: &mut Interp, table: &Value) -> R<Value> {
        let mut out = String::new();
        for c in code_points(&slf.0 .1.s) {
            match it.getitem(table, &Value::Int(c as i64)) {
                Ok(Value::None) => {}
                Ok(Value::Int(i)) => {
                    if !u32::try_from(i).is_ok_and(|i| push_code_point(&mut out, i)) {
                        return Err(it.value_error("character mapping must be in range(0x110000)"));
                    }
                }
                Ok(v) => match v.as_str() {
                    Some(r) => out.push_str(r),
                    None => return Err(it.type_error("character mapping must return integer, None or str")),
                },
                Err(e) => {
                    if it.exc_is(&e, "LookupError") {
                        push_code_point(&mut out, c);
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        Ok(Value::string(out))
    }

    #[proto(str)]
    fn str(slf: This<StrRef<'_>>) -> Value {
        let StrRef(v, s) = slf.0;
        match v {
            Value::Obj(o) if o.cls.is_some() => Value::str(&s.s),
            _ => v.clone(),
        }
    }

    #[method(name = "__getnewargs__", hint(py(text_signature = "")))]
    fn getnewargs(slf: This<StrRef<'_>>) -> Value {
        Value::tuple(vec![Value::str(&slf.0 .1.s)])
    }

    #[proto(hash)]
    fn hash(slf: This<&Value>, it: &mut Interp) -> R<i64> {
        it.native_hash(&slf)
    }

    #[proto(repr)]
    fn repr(slf: This<&Value>, it: &mut Interp) -> R<String> {
        it.native_repr(&slf)
    }

    /// Return a formatted version of the string as described by format_spec.
    #[method(name = "__format__")]
    fn dunder_format(slf: This<StrRef<'_>>, it: &mut Interp, format_spec: &str) -> R<String> {
        it.native_format(slf.0 .0, format_spec)
    }
}

pub fn init(it: &mut Interp) {
    let t = it.types.str_.clone();
    crate::bind::extend_type_documented::<Str>(it, &t);
    reg_slots(it, &t, &["__getitem__", "__len__", "__contains__", "__iter__"]);
    reg_binops(it, &t, &["__add__", "__mul__", "__rmul__", "__mod__", "__rmod__"]);
    reg_compare(it, &t, true);
}
