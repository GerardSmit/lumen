//! `_sre`: the native half of Python's `re`. `re._compiler` produces the opcode list; the
//! shared regex engine in `lumen-common` runs it (see `lumen_common::regex::sre`). This module
//! provides the `Pattern`, `Match`, scanner and template objects around that engine, with the
//! iteration rules of CPython's `Modules/_sre/sre.c`.

use super::native::with_opaque;
use crate::bind::{Py, This};
use crate::object::*;
use crate::vm::*;
use lumen_bind::Passed;
use lumen_common::limits::{Abort, StopFlags};
use lumen_common::regex::{
    self, sre, BacktrackLimit, Captures, ExecOptions, Mode, Regex, BACKTRACK_LIMIT_MSG,
};
use std::cell::RefCell;
use std::rc::Rc;

const FLAG_TEMPLATE: i64 = 1;
const FLAG_IGNORECASE: i64 = 2;
const FLAG_LOCALE: i64 = 4;
const FLAG_MULTILINE: i64 = 8;
const FLAG_DOTALL: i64 = 16;
const FLAG_UNICODE: i64 = 32;
const FLAG_VERBOSE: i64 = 64;
const FLAG_DEBUG: i64 = 128;
const FLAG_ASCII: i64 = 256;

/// Compiled regular expression object.
#[lumen_bind::class(name = "Pattern", module = "re", generic)]
pub struct Pattern {
    pattern: Value,
    flags: i64,
    groups: usize,
    groupindex: Value,
    indexgroup: Value,
    isbytes: i8,
    code: Vec<u32>,
    regex: Rc<Regex>,
}

#[derive(Clone)]
struct PatInfo {
    this: Value,
    regex: Rc<Regex>,
    groups: usize,
    isbytes: i8,
}

/// The result of re.match() and re.search().
/// Match objects always have a boolean value of True.
#[lumen_bind::class(name = "Match", module = "re", generic)]
pub struct Match {
    pattern: Value,
    string: Value,
    marks: Vec<(i64, i64)>,
    pos: usize,
    endpos: usize,
    lastindex: i64,
}

#[lumen_bind::class(name = "SRE_Scanner", module = "_sre")]
pub struct Scanner {
    pat: PatInfo,
    string: Value,
    input: Input,
    pos: usize,
    endpos: usize,
    start: Option<usize>,
    must_advance: bool,
    executing: bool,
}

#[lumen_bind::class(name = "SRE_Template", module = "_sre")]
#[derive(Clone)]
pub struct Template {
    literal: Value,
    items: Vec<(usize, Option<Value>)>,
}

#[lumen_bind::methods]
impl Template {}

thread_local! {
    static WIDE: RefCell<Vec<(Obj, Rc<[u32]>)>> = const { RefCell::new(Vec::new()) };
}

/// The decoded-text cache, moved out of this thread's slot when the thread gives up the GIL
/// (it holds objects, so it must follow the interpreter, not the OS thread).
pub(crate) fn tls_take() -> Box<dyn std::any::Any> {
    Box::new(WIDE.with(|w| std::mem::take(&mut *w.borrow_mut())))
}

pub(crate) fn tls_put(state: Box<dyn std::any::Any>) {
    if let Ok(v) = state.downcast::<Vec<(Obj, Rc<[u32]>)>>() {
        let old = WIDE.with(|w| std::mem::replace(&mut *w.borrow_mut(), *v));
        drop(old);
    }
}

/// The code points of a non-ASCII `str`, cached so scanning a long string with many matches does
/// not re-decode it for every call.
fn wide_text(w: &[u32]) -> String {
    let mut out = String::with_capacity(w.len());
    for &c in w {
        lumen_common::smuggle::push_code_point(&mut out, c);
    }
    out
}

fn wide_of(obj: &Obj) -> Rc<[u32]> {
    let Kind::Str(s) = &obj.kind else {
        unreachable!()
    };
    WIDE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(i) = cache.iter().position(|(o, _)| Rc::ptr_eq(o, obj)) {
            return cache[i].1.clone();
        }
        let cps: Rc<[u32]> = lumen_common::smuggle::code_points(&s.s).collect();
        cache.insert(0, (obj.clone(), cps.clone()));
        cache.truncate(4);
        cps
    })
}

#[derive(Clone)]
enum Subj {
    Ascii(Obj),
    Bytes(Obj),
    Owned(Rc<[u8]>),
    Wide(Rc<[u32]>),
}

#[derive(Clone)]
struct Input {
    subj: Subj,
    len: usize,
    isbytes: bool,
}

impl Input {
    fn new(it: &mut Interp, pat: &PatInfo, string: &Value) -> R<Input> {
        let input = match string {
            Value::Obj(o) => match &o.kind {
                Kind::Str(s) if s.ascii => Input {
                    subj: Subj::Ascii(o.clone()),
                    len: s.s.len(),
                    isbytes: false,
                },
                Kind::Str(s) => Input {
                    subj: Subj::Wide(wide_of(o)),
                    len: s.nchars,
                    isbytes: false,
                },
                Kind::Bytes(b) => Input {
                    subj: Subj::Bytes(o.clone()),
                    len: b.len(),
                    isbytes: true,
                },
                Kind::ByteArray(b) => {
                    let copy: Rc<[u8]> = (&*b.bytes()).into();
                    Input {
                        len: copy.len(),
                        subj: Subj::Owned(copy),
                        isbytes: true,
                    }
                }
                _ => return Input::from_buffer(it, string),
            },
            _ => return Input::from_buffer(it, string),
        };
        if input.isbytes && pat.isbytes == 0 {
            return Err(it.type_error("cannot use a string pattern on a bytes-like object"));
        }
        if !input.isbytes && pat.isbytes > 0 {
            return Err(it.type_error("cannot use a bytes pattern on a string-like object"));
        }
        Ok(input)
    }

    /// Any other bytes-like object, such as a `memoryview`: its bytes are copied.
    fn from_buffer(it: &mut Interp, string: &Value) -> R<Input> {
        match crate::builtins::memview::contiguous_bytes(it, string)? {
            Some(b) => {
                let copy: Rc<[u8]> = b.into();
                Ok(Input {
                    len: copy.len(),
                    subj: Subj::Owned(copy),
                    isbytes: true,
                })
            }
            None => Err(not_a_string(it, string)),
        }
    }

    fn exec(&self, re: &Regex, opts: ExecOptions) -> Result<Option<Captures>, BacktrackLimit> {
        match &self.subj {
            Subj::Ascii(o) => match &o.kind {
                Kind::Str(s) => re.exec(s.s.as_bytes(), opts),
                _ => unreachable!(),
            },
            Subj::Bytes(o) => match &o.kind {
                Kind::Bytes(b) => re.exec(&b[..], opts),
                _ => unreachable!(),
            },
            Subj::Owned(b) => re.exec(&b[..], opts),
            Subj::Wide(w) => re.exec(&w[..], opts),
        }
    }

    fn slice(&self, string: &Value, a: usize, b: usize) -> Value {
        if a == 0 && b == self.len {
            if let Value::Obj(o) = string {
                if o.cls.is_none() && matches!(o.kind, Kind::Str(_) | Kind::Bytes(_)) {
                    return string.clone();
                }
            }
        }
        match &self.subj {
            Subj::Ascii(o) => match &o.kind {
                Kind::Str(s) => Value::str(&s.s[a..b]),
                _ => unreachable!(),
            },
            Subj::Bytes(o) => match &o.kind {
                Kind::Bytes(v) => Value::bytes(v[a..b].to_vec()),
                _ => unreachable!(),
            },
            Subj::Owned(v) => Value::bytes(v[a..b].to_vec()),
            Subj::Wide(w) => Value::string(wide_text(&w[a..b])),
        }
    }

    fn push_slice(&self, out: &mut Joiner, a: usize, b: usize) {
        out.count += 1;
        match &self.subj {
            Subj::Ascii(o) => match &o.kind {
                Kind::Str(s) => out.text.push_str(&s.s[a..b]),
                _ => unreachable!(),
            },
            Subj::Bytes(o) => match &o.kind {
                Kind::Bytes(v) => out.bytes.extend_from_slice(&v[a..b]),
                _ => unreachable!(),
            },
            Subj::Owned(v) => out.bytes.extend_from_slice(&v[a..b]),
            Subj::Wide(w) => w[a..b].iter().for_each(|&c| {
                lumen_common::smuggle::push_code_point(&mut out.text, c);
            }),
        }
    }
}

fn not_a_string(it: &mut Interp, v: &Value) -> Obj {
    let t = it.type_name_of(v);
    it.type_error(&format!(
        "expected string or bytes-like object, got '{}'",
        t
    ))
}

/// Pieces of a `sub`/`expand` result, joined with the type rules of `str.join` / `bytes.join`.
struct Joiner {
    isbytes: bool,
    text: String,
    bytes: Vec<u8>,
    count: usize,
}

impl Joiner {
    fn new(isbytes: bool) -> Joiner {
        Joiner {
            isbytes,
            text: String::new(),
            bytes: Vec::new(),
            count: 0,
        }
    }

    fn push_value(&mut self, it: &mut Interp, v: &Value) -> R<()> {
        let index = self.count;
        self.count += 1;
        if let Value::Obj(o) = v {
            match (&o.kind, self.isbytes) {
                (Kind::Str(s), false) => {
                    self.text.push_str(&s.s);
                    return Ok(());
                }
                (Kind::Bytes(b), true) => {
                    self.bytes.extend_from_slice(b);
                    return Ok(());
                }
                (Kind::ByteArray(b), true) => {
                    self.bytes.extend_from_slice(&b.bytes());
                    return Ok(());
                }
                _ => {}
            }
        }
        let t = it.type_name_of(v);
        Err(it.type_error(&if self.isbytes {
            format!(
                "sequence item {}: expected a bytes-like object, {} found",
                index, t
            )
        } else {
            format!(
                "sequence item {}: expected str instance, {} found",
                index, t
            )
        }))
    }

    fn finish(self) -> Value {
        if self.isbytes {
            Value::bytes(self.bytes)
        } else {
            Value::string(self.text)
        }
    }
}

fn abort_error(it: &mut Interp) -> Obj {
    match regex::take_abort() {
        Abort::Interrupt | Abort::Deadline => it.interrupt_exc(),
        Abort::Heap => it.memory_error(),
        Abort::None => it.new_exc_str("RuntimeError", BACKTRACK_LIMIT_MSG),
    }
}

fn run(it: &mut Interp, pat: &PatInfo, input: &Input, opts: ExecOptions) -> R<Option<Captures>> {
    regex::set_host_poll(StopFlags::from_handle(&it.interrupt), it.heap);
    input.exec(&pat.regex, opts).map_err(|_| abort_error(it))
}

fn clamp(v: i64, len: usize) -> usize {
    v.clamp(0, len as i64) as usize
}

fn code_word(it: &mut Interp, v: &Value) -> R<u32> {
    let n = match v {
        Value::Int(n) => *n,
        Value::Bool(b) => *b as i64,
        _ => {
            if !it.has_index(v) {
                let t = it.type_name_of(v);
                return Err(it.type_error(&format!(
                    "'{}' object cannot be interpreted as an integer",
                    t
                )));
            }
            it.index_of(v)?
        }
    };
    if n < 0 {
        return Err(it.new_exc_str(
            "OverflowError",
            "can't convert negative value to unsigned int",
        ));
    }
    u32::try_from(n).map_err(|_| {
        it.new_exc_str(
            "OverflowError",
            "regular expression code size limit exceeded",
        )
    })
}

fn string_kind(it: &mut Interp, v: &Value) -> R<i8> {
    match v {
        Value::None => Ok(-1),
        Value::Obj(o) => match &o.kind {
            Kind::Str(_) => Ok(0),
            Kind::Bytes(_) | Kind::ByteArray(_) => Ok(1),
            _ => Err(not_a_string(it, v)),
        },
        _ => Err(not_a_string(it, v)),
    }
}

/// `Py_UNICODE_TOUPPER`: the first code point of the full uppercase mapping.
fn upper_first(c: u32) -> u32 {
    match char::from_u32(c) {
        Some(ch) if c >= 128 => ch.to_uppercase().next().map_or(c, |u| u as u32),
        Some(_) => (c as u8).to_ascii_uppercase() as u32,
        None => c,
    }
}

/// The text of capture group `g` in `caps`, or `None`/`''` when it did not participate.
fn group_text(input: &Input, string: &Value, caps: &Captures, g: usize, empty: bool) -> Value {
    match caps.get(g).copied().flatten() {
        Some((a, b)) => input.slice(string, a, b),
        None if empty => input.slice(string, 0, 0),
        None => Value::None,
    }
}

enum Filter {
    Literal(Value),
    Template(Template),
    Callable(Value),
}

fn compile_template(it: &mut Interp, pattern: &Value, template: &Value) -> R<Template> {
    let re = it.import_module("re")?;
    let func = it.get_attr_str(&Value::Obj(re), "_compile_template")?;
    let mut result = it.call(&func, vec![pattern.clone(), template.clone()], Vec::new());
    if let Err(e) = &result {
        if it.exc_is(e, "TypeError") {
            let plain = match template {
                Value::Obj(o) => match &o.kind {
                    Kind::Str(s) if o.cls.is_some() => Some(Value::str(&s.s)),
                    Kind::ByteArray(b) => Some(Value::bytes(b.to_vec())),
                    Kind::Bytes(b) if o.cls.is_some() => Some(Value::bytes(b.clone())),
                    _ => None,
                },
                _ => None,
            };
            if let Some(plain) = plain {
                result = it.call(&func, vec![pattern.clone(), plain], Vec::new());
            }
        }
    }
    let result = result?;
    match with_opaque::<Template, _>(&result, |t| t.clone()) {
        Some(t) => Ok(t),
        None => {
            let t = it.type_name_of(&result);
            Err(it.new_exc_str(
                "RuntimeError",
                &format!("the result of compiling a replacement string is {}", t),
            ))
        }
    }
}

fn is_literal_template(v: &Value) -> bool {
    match v {
        Value::Obj(o) => match &o.kind {
            Kind::Str(s) => !s.s.contains('\\'),
            Kind::Bytes(b) => !b.contains(&b'\\'),
            Kind::ByteArray(b) => !b.bytes().contains(&b'\\'),
            _ => false,
        },
        _ => false,
    }
}

/// The expansion of `template` for a match, joined into one str or bytes value.
fn expand(
    it: &mut Interp,
    template: &Template,
    groups: usize,
    input: &Input,
    string: &Value,
    caps: &Captures,
) -> R<Value> {
    if template.items.is_empty() {
        return Ok(template.literal.clone());
    }
    let isbytes = matches!(&template.literal, Value::Obj(o) if !matches!(o.kind, Kind::Str(_)));
    let mut out = Joiner::new(isbytes);
    out.push_value(it, &template.literal)?;
    for (index, literal) in &template.items {
        if *index > groups {
            return Err(it.new_exc_str("IndexError", "no such group"));
        }
        let item = group_text(input, string, caps, *index, false);
        if !item.is_none() {
            out.push_value(it, &item)?;
        }
        if let Some(l) = literal {
            out.push_value(it, l)?;
        }
    }
    Ok(out.finish())
}

/// A slice of the match's subject by character offsets, clamped to its current length.
fn subject_slice(it: &mut Interp, string: &Value, a: i64, b: i64) -> Value {
    let Value::Obj(o) = string else {
        return Value::None;
    };
    let (a, b) = (a.max(0) as usize, b.max(0) as usize);
    match &o.kind {
        Kind::Str(s) if s.ascii => {
            let (a, b) = (a.min(s.s.len()), b.min(s.s.len()));
            if a == 0 && b == s.s.len() && o.cls.is_none() {
                return string.clone();
            }
            Value::str(&s.s[a..b.max(a)])
        }
        Kind::Str(s) => {
            let (a, b) = (a.min(s.nchars), b.min(s.nchars));
            if a == 0 && b == s.nchars && o.cls.is_none() {
                return string.clone();
            }
            let w = wide_of(o);
            Value::string(wide_text(&w[a..b.max(a)]))
        }
        Kind::Bytes(v) => {
            let (a, b) = (a.min(v.len()), b.min(v.len()));
            if a == 0 && b == v.len() && o.cls.is_none() {
                return string.clone();
            }
            Value::bytes(v[a..b.max(a)].to_vec())
        }
        Kind::ByteArray(v) => {
            let v = v.bytes();
            let (a, b) = (a.min(v.len()), b.min(v.len()));
            Value::bytes(v[a..b.max(a)].to_vec())
        }
        _ => match crate::builtins::memview::contiguous_bytes(it, string) {
            Ok(Some(v)) => {
                let (a, b) = (a.min(v.len()), b.min(v.len()));
                Value::bytes(v[a..b.max(a)].to_vec())
            }
            _ => Value::None,
        },
    }
}

fn pat_info(it: &mut Interp, p: &Py<Pattern>) -> R<PatInfo> {
    let d = p.borrow(it)?;
    Ok(PatInfo {
        this: p.value().clone(),
        regex: d.regex.clone(),
        groups: d.groups,
        isbytes: d.isbytes,
    })
}

fn new_match(
    it: &mut Interp,
    pat: &PatInfo,
    string: &Value,
    caps: &Captures,
    pos: usize,
    endpos: usize,
) -> Value {
    let mut marks = Vec::with_capacity(pat.groups + 1);
    for g in 0..=pat.groups {
        marks.push(match caps.get(g).copied().flatten() {
            Some((a, b)) => (a as i64, b as i64),
            None => (-1, -1),
        });
    }
    let lastindex = caps.last_group().map_or(-1, |g| g as i64);
    Py::new(
        it,
        Match {
            pattern: pat.this.clone(),
            string: string.clone(),
            marks,
            pos,
            endpos,
            lastindex,
        },
    )
    .into_value()
}

/// `pos` and `endpos` as integers, converted before the subject is checked (as CPython's
/// argument parsing does); [`bounds`] clamps them to the subject.
fn indices(it: &mut Interp, pos: Passed<&Value>, endpos: Passed<&Value>) -> R<(i64, i64)> {
    let pos = match pos.0 {
        Some(v) => it.index_of(v)?,
        None => 0,
    };
    let endpos = match endpos.0 {
        Some(v) => it.index_of(v)?,
        None => i64::MAX,
    };
    Ok((pos, endpos))
}

fn bounds((pos, endpos): (i64, i64), len: usize) -> (usize, usize) {
    (clamp(pos, len), clamp(endpos, len))
}

fn match_like(
    it: &mut Interp,
    slf: &Py<Pattern>,
    string: &Value,
    pos: Passed<&Value>,
    endpos: Passed<&Value>,
    mode: Mode,
) -> R<Value> {
    let pat = pat_info(it, slf)?;
    let at = indices(it, pos, endpos)?;
    let input = Input::new(it, &pat, string)?;
    let (start, end) = bounds(at, input.len);
    let opts = ExecOptions {
        start,
        end: Some(end),
        mode,
        must_advance: false,
    };
    Ok(match run(it, &pat, &input, opts)? {
        Some(caps) => new_match(it, &pat, string, &caps, start, end),
        None => Value::None,
    })
}

fn new_scanner(
    it: &mut Interp,
    slf: &Py<Pattern>,
    string: &Value,
    pos: Passed<&Value>,
    endpos: Passed<&Value>,
) -> R<Py<Scanner>> {
    let pat = pat_info(it, slf)?;
    let at = indices(it, pos, endpos)?;
    let input = Input::new(it, &pat, string)?;
    let (pos, endpos) = bounds(at, input.len);
    let s = Scanner {
        pat,
        string: string.clone(),
        input,
        pos,
        endpos,
        start: Some(pos),
        must_advance: false,
        executing: false,
    };
    Ok(Py::new(it, s))
}

fn sub_like(
    it: &mut Interp,
    slf: &Py<Pattern>,
    repl: &Value,
    string: &Value,
    count: Passed<&Value>,
) -> R<(Value, i64)> {
    let pat = pat_info(it, slf)?;
    let count = match count.0 {
        Some(v) => it.index_of(v)?,
        None => 0,
    };
    let filter = if it.is_callable(repl) {
        Filter::Callable(repl.clone())
    } else if is_literal_template(repl) {
        Filter::Literal(match repl {
            Value::Obj(o) => match &o.kind {
                Kind::ByteArray(v) => Value::bytes(v.to_vec()),
                _ => repl.clone(),
            },
            _ => repl.clone(),
        })
    } else {
        let t = compile_template(it, &pat.this, repl)?;
        if t.items.is_empty() {
            Filter::Literal(t.literal)
        } else {
            Filter::Template(t)
        }
    };
    let input = Input::new(it, &pat, string)?;
    let end = input.len;
    let mut out = Joiner::new(input.isbytes);
    let (mut n, mut copied, mut start, mut must_advance) = (0i64, 0usize, 0usize, false);
    while count == 0 || n < count {
        it.poll()?;
        let opts = ExecOptions {
            start,
            end: None,
            mode: Mode::Search,
            must_advance,
        };
        let Some(caps) = run(it, &pat, &input, opts)? else {
            break;
        };
        let (ms, me) = caps[0].unwrap();
        if copied < ms {
            input.push_slice(&mut out, copied, ms);
        }
        match &filter {
            Filter::Literal(v) => out.push_value(it, v)?,
            Filter::Template(t) => {
                let item = expand(it, t, pat.groups, &input, string, &caps)?;
                out.push_value(it, &item)?;
            }
            Filter::Callable(f) => {
                let m = new_match(it, &pat, string, &caps, 0, end);
                let item = it.call(f, vec![m], Vec::new())?;
                if !item.is_none() {
                    out.push_value(it, &item)?;
                }
            }
        }
        copied = me;
        n += 1;
        must_advance = me == ms;
        start = me;
    }
    if copied < end {
        input.push_slice(&mut out, copied, end);
    }
    Ok((out.finish(), n))
}

/// The identity of a pattern for `==` and `hash()`.
fn pattern_key(it: &mut Interp, p: &Py<Pattern>) -> R<(Value, i64, i8, Vec<u32>)> {
    let d = p.borrow(it)?;
    Ok((d.pattern.clone(), d.flags, d.isbytes, d.code.clone()))
}

fn pattern_eq(it: &mut Interp, slf: &Py<Pattern>, other: &Value, ne: bool) -> R<Value> {
    let Some(other) = Py::<Pattern>::from_value(it, other) else {
        return Ok(Value::NotImplemented);
    };
    if slf.value().is(other.value()) {
        return Ok(Value::Bool(!ne));
    }
    let (l, r) = (pattern_key(it, slf)?, pattern_key(it, &other)?);
    let same = l.1 == r.1 && l.2 == r.2 && l.3 == r.3 && it.values_eq(&l.0, &r.0)?;
    Ok(Value::Bool(same != ne))
}

#[lumen_bind::methods]
impl Pattern {
    /// Matches zero or more characters at the beginning of the string.
    #[method(hint(py(text_signature = "($self, /, string, pos=0, endpos=sys.maxsize)")))]
    fn r#match(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] string: &Value,
        #[kw] pos: Passed<&Value>,
        #[kw] endpos: Passed<&Value>,
    ) -> R<Value> {
        match_like(it, &slf.0, string, pos, endpos, Mode::Match)
    }

    /// Matches against all of the string.
    #[method(hint(py(text_signature = "($self, /, string, pos=0, endpos=sys.maxsize)")))]
    fn fullmatch(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] string: &Value,
        #[kw] pos: Passed<&Value>,
        #[kw] endpos: Passed<&Value>,
    ) -> R<Value> {
        match_like(it, &slf.0, string, pos, endpos, Mode::FullMatch)
    }

    /// Scan through string looking for a match, and return a corresponding match object instance.
    ///
    /// Return None if no position in the string matches.
    #[method(hint(py(text_signature = "($self, /, string, pos=0, endpos=sys.maxsize)")))]
    fn search(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] string: &Value,
        #[kw] pos: Passed<&Value>,
        #[kw] endpos: Passed<&Value>,
    ) -> R<Value> {
        match_like(it, &slf.0, string, pos, endpos, Mode::Search)
    }

    /// Return the string obtained by replacing the leftmost non-overlapping occurrences of pattern in string by the replacement repl.
    #[method(hint(py(text_signature = "($self, /, repl, string, count=0)")))]
    fn sub(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] repl: &Value,
        #[kw] string: &Value,
        #[kw] count: Passed<&Value>,
    ) -> R<Value> {
        Ok(sub_like(it, &slf.0, repl, string, count)?.0)
    }

    /// Return the tuple (new_string, number_of_subs_made) found by replacing the leftmost non-overlapping occurrences of pattern with the replacement repl.
    #[method(hint(py(text_signature = "($self, /, repl, string, count=0)")))]
    fn subn(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] repl: &Value,
        #[kw] string: &Value,
        #[kw] count: Passed<&Value>,
    ) -> R<Value> {
        let (s, n) = sub_like(it, &slf.0, repl, string, count)?;
        Ok(Value::tuple(vec![s, Value::Int(n)]))
    }

    /// Return a list of all non-overlapping matches of pattern in string.
    #[method(hint(py(text_signature = "($self, /, string, pos=0, endpos=sys.maxsize)")))]
    fn findall(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] string: &Value,
        #[kw] pos: Passed<&Value>,
        #[kw] endpos: Passed<&Value>,
    ) -> R<Value> {
        let pat = pat_info(it, &slf.0)?;
        let at = indices(it, pos, endpos)?;
        let input = Input::new(it, &pat, string)?;
        let (mut start, end) = bounds(at, input.len);
        let mut out = Vec::new();
        let mut must_advance = false;
        while start <= end {
            it.poll()?;
            let opts = ExecOptions {
                start,
                end: Some(end),
                mode: Mode::Search,
                must_advance,
            };
            let Some(caps) = run(it, &pat, &input, opts)? else {
                break;
            };
            let (ms, me) = caps[0].unwrap();
            out.push(match pat.groups {
                0 => input.slice(string, ms, me),
                1 => group_text(&input, string, &caps, 1, true),
                n => Value::tuple(
                    (1..=n)
                        .map(|g| group_text(&input, string, &caps, g, true))
                        .collect(),
                ),
            });
            must_advance = me == ms;
            start = me;
        }
        Ok(Value::list(out))
    }

    /// Split string by the occurrences of pattern.
    #[method(hint(py(text_signature = "($self, /, string, maxsplit=0)")))]
    fn split(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] string: &Value,
        #[kw] maxsplit: Passed<&Value>,
    ) -> R<Value> {
        let pat = pat_info(it, &slf.0)?;
        let maxsplit = match maxsplit.0 {
            Some(v) => it.index_of(v)?,
            None => 0,
        };
        let input = Input::new(it, &pat, string)?;
        let end = input.len;
        let mut out = Vec::new();
        let (mut n, mut last, mut start, mut must_advance) = (0i64, 0usize, 0usize, false);
        while maxsplit == 0 || n < maxsplit {
            it.poll()?;
            let opts = ExecOptions {
                start,
                end: None,
                mode: Mode::Search,
                must_advance,
            };
            let Some(caps) = run(it, &pat, &input, opts)? else {
                break;
            };
            let (ms, me) = caps[0].unwrap();
            out.push(input.slice(string, last, ms));
            for g in 1..=pat.groups {
                out.push(group_text(&input, string, &caps, g, false));
            }
            n += 1;
            must_advance = me == ms;
            last = me;
            start = me;
        }
        out.push(input.slice(string, last, end));
        Ok(Value::list(out))
    }

    /// Return an iterator over all non-overlapping matches for the RE pattern in string.
    ///
    /// For each match, the iterator returns a match object.
    #[method(hint(py(text_signature = "($self, /, string, pos=0, endpos=sys.maxsize)")))]
    fn finditer(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] string: &Value,
        #[kw] pos: Passed<&Value>,
        #[kw] endpos: Passed<&Value>,
    ) -> R<Value> {
        let scanner = new_scanner(it, &slf.0, string, pos, endpos)?;
        let search = it.get_attr_str(scanner.value(), "search")?;
        Ok(it.mk_iter(IterState::CallIter {
            f: search,
            sentinel: Value::None,
            done: false,
        }))
    }

    #[method(hint(py(text_signature = "($self, /, string, pos=0, endpos=sys.maxsize)")))]
    fn scanner(
        slf: This<Py<Self>>,
        it: &mut Interp,
        #[kw] string: &Value,
        #[kw] pos: Passed<&Value>,
        #[kw] endpos: Passed<&Value>,
    ) -> R<Py<Scanner>> {
        new_scanner(it, &slf.0, string, pos, endpos)
    }

    #[method(name = "__copy__")]
    fn copy(slf: This<Py<Self>>) -> Value {
        slf.0.into_value()
    }

    #[method(name = "__deepcopy__")]
    fn deepcopy(slf: This<Py<Self>>, _memo: &Value) -> Value {
        slf.0.into_value()
    }

    #[proto(repr)]
    fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        let (pattern, mut flags, isbytes) = {
            let d = slf.0.borrow(it)?;
            (d.pattern.clone(), d.flags, d.isbytes)
        };
        if isbytes == 0 && flags & (FLAG_LOCALE | FLAG_UNICODE | FLAG_ASCII) == FLAG_UNICODE {
            flags &= !FLAG_UNICODE;
        }
        let names = [
            (FLAG_TEMPLATE, "re.TEMPLATE"),
            (FLAG_IGNORECASE, "re.IGNORECASE"),
            (FLAG_LOCALE, "re.LOCALE"),
            (FLAG_MULTILINE, "re.MULTILINE"),
            (FLAG_DOTALL, "re.DOTALL"),
            (FLAG_UNICODE, "re.UNICODE"),
            (FLAG_VERBOSE, "re.VERBOSE"),
            (FLAG_DEBUG, "re.DEBUG"),
            (FLAG_ASCII, "re.ASCII"),
        ];
        let mut parts: Vec<String> = Vec::new();
        for (bit, name) in names {
            if flags & bit != 0 {
                parts.push(name.to_string());
                flags &= !bit;
            }
        }
        if flags != 0 {
            parts.push(format!("0x{:x}", flags));
        }
        let shown: String = it.repr_of(&pattern)?.chars().take(200).collect();
        Ok(if parts.is_empty() {
            format!("re.compile({})", shown)
        } else {
            format!("re.compile({}, {})", shown, parts.join("|"))
        })
    }

    #[proto(hash)]
    fn hash(slf: This<Py<Self>>, it: &mut Interp) -> R<i64> {
        let (pattern, flags, isbytes, code) = pattern_key(it, &slf.0)?;
        let mut hash = it.hash_value(&pattern)?;
        let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_ne_bytes()).collect();
        hash ^= hash_bytes(&bytes);
        hash ^= flags ^ isbytes as i64 ^ code.len() as i64;
        Ok(if hash == -1 { -2 } else { hash })
    }

    #[proto(eq)]
    fn eq(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        pattern_eq(it, &slf.0, value, false)
    }

    #[proto(ne)]
    fn ne(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<Value> {
        pattern_eq(it, &slf.0, value, true)
    }

    #[proto(lt)]
    fn lt(_slf: This<Py<Self>>, _value: &Value) -> Value {
        Value::NotImplemented
    }

    #[proto(le)]
    fn le(_slf: This<Py<Self>>, _value: &Value) -> Value {
        Value::NotImplemented
    }

    #[proto(gt)]
    fn gt(_slf: This<Py<Self>>, _value: &Value) -> Value {
        Value::NotImplemented
    }

    #[proto(ge)]
    fn ge(_slf: This<Py<Self>>, _value: &Value) -> Value {
        Value::NotImplemented
    }

    /// The pattern string from which the RE object was compiled.
    #[getter]
    fn pattern(&self) -> Value {
        self.pattern.clone()
    }

    /// The regex matching flags.
    #[getter]
    fn flags(&self) -> i64 {
        self.flags
    }

    /// The number of capturing groups in the pattern.
    #[getter]
    fn groups(&self) -> i64 {
        self.groups as i64
    }

    /// A dictionary mapping group names to group numbers.
    #[getter]
    fn groupindex(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let gi = slf.0.borrow(it)?.groupindex.clone();
        Ok(match gi {
            Value::None => Value::Obj(it.new_dict()),
            d => it.new_mappingproxy(d),
        })
    }
}

// ---- Match --------------------------------------------------------------------------------------

fn md<X>(it: &mut Interp, m: &Py<Match>, f: impl FnOnce(&Match) -> X) -> R<X> {
    let d = m.borrow(it)?;
    Ok(f(&d))
}

/// The `groupindex`/`indexgroup` of a match's pattern.
fn pattern_field(pattern: &Value, f: impl FnOnce(&Pattern) -> Value) -> Value {
    with_opaque::<Pattern, _>(pattern, |d| f(d)).unwrap_or(Value::None)
}

fn group_index(it: &mut Interp, m: &Py<Match>, index: Option<&Value>) -> R<usize> {
    let (pattern, groups) = md(it, m, |d| (d.pattern.clone(), d.marks.len()))?;
    let found: Option<i64> = match index {
        None => Some(0),
        Some(v) if it.has_index(v) => match it.index_of(v) {
            Ok(i) => Some(i),
            Err(e) if it.exc_is(&e, "OverflowError") => None,
            Err(e) => return Err(e),
        },
        Some(v) => match &pattern_field(&pattern, |d| d.groupindex.clone()) {
            Value::Obj(d) => match it.dict_get(d, v)? {
                Some(Value::Int(n)) => Some(n),
                _ => None,
            },
            _ => None,
        },
    };
    match found {
        Some(i) if i >= 0 && (i as usize) < groups => Ok(i as usize),
        _ => Err(it.new_exc_str("IndexError", "no such group")),
    }
}

fn group_slice(it: &mut Interp, m: &Py<Match>, index: usize, default: &Value) -> R<Value> {
    let (string, mark) = md(it, m, |d| (d.string.clone(), d.marks[index]))?;
    if mark.0 < 0 {
        return Ok(default.clone());
    }
    Ok(subject_slice(it, &string, mark.0, mark.1))
}

fn match_getslice(
    it: &mut Interp,
    m: &Py<Match>,
    index: Option<&Value>,
    default: &Value,
) -> R<Value> {
    let i = group_index(it, m, index)?;
    group_slice(it, m, i, default)
}

fn mark_of(it: &mut Interp, m: &Py<Match>, group: Passed<&Value>) -> R<(i64, i64)> {
    let i = group_index(it, m, group.0)?;
    md(it, m, |d| d.marks[i])
}

#[lumen_bind::methods]
impl Match {
    /// group([group1, ...]) -> str or tuple.
    ///     Return subgroup(s) of the match by indices or names.
    ///     For 0 returns the entire match.
    #[method(hint(py(text_signature = "")))]
    fn group(slf: This<Py<Self>>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        match args {
            [] => match_getslice(it, &slf.0, None, &Value::None),
            [g] => match_getslice(it, &slf.0, Some(g), &Value::None),
            _ => {
                let mut out = Vec::with_capacity(args.len());
                for g in args {
                    out.push(match_getslice(it, &slf.0, Some(g), &Value::None)?);
                }
                Ok(Value::tuple(out))
            }
        }
    }

    #[proto(getitem)]
    fn getitem(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<Value> {
        match_getslice(it, &slf.0, Some(key), &Value::None)
    }

    /// Return index of the start of the substring matched by group.
    #[method(hint(py(text_signature = "($self, group=0, /)")))]
    fn start(slf: This<Py<Self>>, it: &mut Interp, group: Passed<&Value>) -> R<i64> {
        Ok(mark_of(it, &slf.0, group)?.0)
    }

    /// Return index of the end of the substring matched by group.
    #[method(hint(py(text_signature = "($self, group=0, /)")))]
    fn end(slf: This<Py<Self>>, it: &mut Interp, group: Passed<&Value>) -> R<i64> {
        Ok(mark_of(it, &slf.0, group)?.1)
    }

    /// For match object m, return the 2-tuple (m.start(group), m.end(group)).
    #[method(hint(py(text_signature = "($self, group=0, /)")))]
    fn span(slf: This<Py<Self>>, it: &mut Interp, group: Passed<&Value>) -> R<Value> {
        let m = mark_of(it, &slf.0, group)?;
        Ok(Value::tuple(vec![Value::Int(m.0), Value::Int(m.1)]))
    }

    /// Return a tuple containing all the subgroups of the match, from 1.
    ///
    ///   default
    ///     Is used for groups that did not participate in the match.
    #[method(hint(py(text_signature = "($self, /, default=None)")))]
    fn groups(slf: This<Py<Self>>, it: &mut Interp, #[kw] default: Option<&Value>) -> R<Value> {
        let default = default.unwrap_or(&Value::None);
        let n = md(it, &slf.0, |d| d.marks.len())?;
        let mut out = Vec::with_capacity(n.saturating_sub(1));
        for g in 1..n {
            out.push(group_slice(it, &slf.0, g, default)?);
        }
        Ok(Value::tuple(out))
    }

    /// Return a dictionary containing all the named subgroups of the match, keyed by the subgroup name.
    ///
    ///   default
    ///     Is used for groups that did not participate in the match.
    #[method(hint(py(text_signature = "($self, /, default=None)")))]
    fn groupdict(slf: This<Py<Self>>, it: &mut Interp, #[kw] default: Option<&Value>) -> R<Value> {
        let default = default.unwrap_or(&Value::None);
        let pattern = md(it, &slf.0, |d| d.pattern.clone())?;
        let result = it.new_dict();
        if let Value::Obj(d) = &pattern_field(&pattern, |d| d.groupindex.clone()) {
            let keys = crate::containers::pydict_of(d)
                .map(|p| p.borrow().keys())
                .unwrap_or_default();
            for key in keys {
                let v = match_getslice(it, &slf.0, Some(&key), default)?;
                it.dict_set(&result, key, v)?;
            }
        }
        Ok(Value::Obj(result))
    }

    /// Return the string obtained by doing backslash substitution on the string template, as done by the sub() method.
    #[method(hint(py(text_signature = "($self, /, template)")))]
    fn expand(slf: This<Py<Self>>, it: &mut Interp, #[kw] template: &Value) -> R<Value> {
        let (pattern, string, marks) = md(it, &slf.0, |d| {
            (d.pattern.clone(), d.string.clone(), d.marks.clone())
        })?;
        let t = compile_template(it, &pattern, template)?;
        if t.items.is_empty() {
            return Ok(t.literal);
        }
        let isbytes = matches!(&t.literal, Value::Obj(o) if !matches!(o.kind, Kind::Str(_)));
        let mut out = Joiner::new(isbytes);
        out.push_value(it, &t.literal)?;
        for (index, literal) in &t.items {
            if *index >= marks.len() {
                return Err(it.new_exc_str("IndexError", "no such group"));
            }
            let mark = marks[*index];
            if mark.0 >= 0 {
                let item = subject_slice(it, &string, mark.0, mark.1);
                out.push_value(it, &item)?;
            }
            if let Some(l) = literal {
                out.push_value(it, l)?;
            }
        }
        Ok(out.finish())
    }

    #[method(name = "__copy__")]
    fn copy(slf: This<Py<Self>>) -> Value {
        slf.0.into_value()
    }

    #[method(name = "__deepcopy__")]
    fn deepcopy(slf: This<Py<Self>>, _memo: &Value) -> Value {
        slf.0.into_value()
    }

    #[proto(repr)]
    fn repr(slf: This<Py<Self>>, it: &mut Interp) -> R<String> {
        let span = md(it, &slf.0, |d| d.marks[0])?;
        let group0 = group_slice(it, &slf.0, 0, &Value::None)?;
        let shown: String = it.repr_of(&group0)?.chars().take(50).collect();
        Ok(format!(
            "<re.Match object; span=({}, {}), match={}>",
            span.0, span.1, shown
        ))
    }

    /// The integer index of the last matched capturing group.
    #[getter]
    fn lastindex(&self) -> Value {
        if self.lastindex >= 0 {
            Value::Int(self.lastindex)
        } else {
            Value::None
        }
    }

    /// The name of the last matched capturing group.
    #[getter]
    fn lastgroup(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let (pattern, i) = md(it, &slf.0, |d| (d.pattern.clone(), d.lastindex))?;
        let ig = pattern_field(&pattern, |d| d.indexgroup.clone());
        Ok(match ig.tuple_items() {
            Some(t) if i >= 0 && (i as usize) < t.len() => t[i as usize].clone(),
            _ => Value::None,
        })
    }

    #[getter]
    fn regs(&self) -> Value {
        Value::tuple(
            self.marks
                .iter()
                .map(|&(s, e)| Value::tuple(vec![Value::Int(s), Value::Int(e)]))
                .collect(),
        )
    }

    /// The string passed to match() or search().
    #[getter]
    fn string(&self) -> Value {
        self.string.clone()
    }

    /// The regular expression object.
    #[getter]
    fn re(&self) -> Value {
        self.pattern.clone()
    }

    /// The index into the string at which the RE engine started looking for a match.
    #[getter]
    fn pos(&self) -> i64 {
        self.pos as i64
    }

    /// The index into the string beyond which the RE engine will not go.
    #[getter]
    fn endpos(&self) -> i64 {
        self.endpos as i64
    }
}

// ---- Scanner ------------------------------------------------------------------------------------

fn scanner_step(it: &mut Interp, slf: &Py<Scanner>, mode: Mode) -> R<Value> {
    let entered = slf.with(it, |s| {
        if s.executing {
            return None;
        }
        s.executing = true;
        Some((
            s.pat.clone(),
            s.string.clone(),
            s.input.clone(),
            s.pos,
            s.endpos,
            s.start,
            s.must_advance,
        ))
    })?;
    let Some((pat, string, input, pos, endpos, start, must_advance)) = entered else {
        return Err(it.value_error("regular expression scanner already executing"));
    };
    let outcome = match start {
        Some(start) => run(
            it,
            &pat,
            &input,
            ExecOptions {
                start,
                end: Some(endpos),
                mode,
                must_advance,
            },
        ),
        None => Ok(None),
    };
    let value = match &outcome {
        Ok(Some(caps)) => new_match(it, &pat, &string, caps, pos, endpos),
        _ => Value::None,
    };
    slf.with(it, |s| {
        s.executing = false;
        match &outcome {
            Ok(Some(caps)) => {
                let (ms, me) = caps[0].unwrap();
                s.must_advance = ms == me;
                s.start = Some(me);
            }
            Ok(None) => s.start = None,
            Err(_) => {}
        }
    })?;
    outcome.map(|_| value)
}

#[lumen_bind::methods]
impl Scanner {
    #[method(name = "match")]
    fn match_(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        scanner_step(it, &slf.0, Mode::Match)
    }

    #[method]
    fn search(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        scanner_step(it, &slf.0, Mode::Search)
    }

    #[getter]
    fn pattern(&self) -> Value {
        self.pat.this.clone()
    }
}

// The SRE engine: compiles the opcode lists `re._compiler` produces.
#[lumen_bind::module(name = "_sre")]
pub mod _sre {
    use super::*;

    #[constant(name = "MAGIC")]
    const MAGIC: i64 = sre::MAGIC as i64;
    #[constant(name = "CODESIZE")]
    const CODESIZE: i64 = 4;
    #[constant(name = "MAXREPEAT")]
    const MAXREPEAT: i64 = sre::MAXREPEAT as i64;
    #[constant(name = "MAXGROUPS")]
    const MAXGROUPS: i64 = sre::MAXGROUPS as i64;
    #[constant(name = "copyright")]
    const COPYRIGHT: &'static str = " SRE 2.2.2 Copyright (c) 1997-2002 by Secret Labs AB ";

    #[op(hint(py(
        text_signature = "($module, /, pattern, flags, code, groups, groupindex,\n        indexgroup)"
    )))]
    fn compile(
        it: &mut Interp,
        #[kw] pattern: &Value,
        #[kw] flags: &Value,
        #[kw] code: &Value,
        #[kw] groups: &Value,
        #[kw] groupindex: &Value,
        #[kw] indexgroup: &Value,
    ) -> R<Py<Pattern>> {
        let flags = it.index_of(flags)?;
        let Some(list) = list_of(code) else {
            let t = it.type_name_of(code);
            return Err(it.type_error(&format!(
                "compile() argument 'code' must be list, not {}",
                t
            )));
        };
        let items: Vec<Value> = list.borrow().clone();
        let mut words = Vec::with_capacity(items.len());
        for v in &items {
            words.push(code_word(it, v)?);
        }
        let groups = it.index_of(groups)?;
        if dict_of(groupindex).is_none() {
            let t = it.type_name_of(groupindex);
            return Err(it.type_error(&format!(
                "compile() argument 'groupindex' must be dict, not {}",
                t
            )));
        }
        if indexgroup.tuple_items().is_none() {
            let t = it.type_name_of(indexgroup);
            return Err(it.type_error(&format!(
                "compile() argument 'indexgroup' must be tuple, not {}",
                t
            )));
        }
        let isbytes = string_kind(it, pattern)?;
        if groups < 0 || groups as u64 > sre::MAXGROUPS as u64 {
            return Err(it.new_exc_str("RuntimeError", "invalid SRE code"));
        }
        let regex = match sre::build(&words, groups as usize) {
            Ok(r) => r,
            Err(sre::SreError::Invalid) => {
                return Err(it.new_exc_str("RuntimeError", "invalid SRE code"));
            }
            Err(sre::SreError::Limit(msg)) => return Err(it.new_exc_str("OverflowError", &msg)),
        };
        let named = dict_of(groupindex).map_or(0, |d| d.borrow().len());
        let (groupindex, indexgroup) = if named > 0 {
            let n = indexgroup.tuple_items().map_or(0, |t| t.len());
            (
                groupindex.clone(),
                if n > 0 {
                    indexgroup.clone()
                } else {
                    Value::None
                },
            )
        } else {
            (Value::None, Value::None)
        };
        let p = Pattern {
            pattern: pattern.clone(),
            flags,
            groups: groups as usize,
            groupindex,
            indexgroup,
            isbytes,
            code: words,
            regex: Rc::new(regex),
        };
        Ok(Py::new(it, p))
    }

    ///
    ///
    ///   template
    ///     A list containing interleaved literal strings (str or bytes) and group
    ///     indices (int), as returned by re._parser.parse_template():
    ///         [literal1, group1, ..., literalN, groupN]
    #[op]
    fn template(it: &mut Interp, _pattern: &Value, template: &Value) -> R<Py<Template>> {
        let Some(list) = list_of(template) else {
            let t = it.type_name_of(template);
            return Err(it.type_error(&format!("template() argument 2 must be list, not {}", t)));
        };
        let items: Vec<Value> = list.borrow().clone();
        if items.len() % 2 == 0 {
            return Err(it.type_error("invalid template"));
        }
        let mut out = Vec::new();
        for pair in items[1..].chunks(2) {
            let index = it.index_of(&pair[0])?;
            if index < 0 {
                return Err(it.type_error("invalid template"));
            }
            let literal = match &pair[1] {
                Value::Obj(o) => match &o.kind {
                    Kind::Str(s) if s.s.is_empty() => None,
                    Kind::Bytes(b) if b.is_empty() => None,
                    _ => Some(pair[1].clone()),
                },
                v => Some(v.clone()),
            };
            out.push((index as usize, literal));
        }
        Ok(Py::new(
            it,
            Template {
                literal: items[0].clone(),
                items: out,
            },
        ))
    }

    #[op]
    fn getcodesize() -> i64 {
        4
    }

    #[op]
    fn ascii_iscased(it: &mut Interp, character: &Value) -> R<bool> {
        let c = char_arg(it, character)?;
        Ok(c < 128 && (c as u8).is_ascii_alphabetic())
    }

    #[op]
    fn unicode_iscased(it: &mut Interp, character: &Value) -> R<bool> {
        let c = char_arg(it, character)?;
        Ok(c != regex::py_lower(c) || c != upper_first(c))
    }

    #[op]
    fn ascii_tolower(it: &mut Interp, character: &Value) -> R<i64> {
        let c = char_arg(it, character)?;
        Ok(if c < 128 {
            (c as u8).to_ascii_lowercase() as i64
        } else {
            c as i64
        })
    }

    #[op]
    fn unicode_tolower(it: &mut Interp, character: &Value) -> R<i64> {
        let c = char_arg(it, character)?;
        Ok(regex::py_lower(c) as i64)
    }

    #[init]
    fn init(it: &mut Interp, _m: &Value) {
        let p = crate::bind::type_object::<Pattern>(it);
        super::super::descr::install_getsets::<Pattern>(it, &p, &["pattern", "flags", "groups"]);
        let m = crate::bind::type_object::<Match>(it);
        super::super::descr::install_getsets::<Match>(it, &m, &["string", "re", "pos", "endpos"]);
        let s = crate::bind::type_object::<Scanner>(it);
        super::super::descr::install_getsets::<Scanner>(it, &s, &["pattern"]);
    }
}

fn char_arg(it: &mut Interp, v: &Value) -> R<u32> {
    let n = it.index_of(v)?;
    Ok(n.clamp(0, u32::MAX as i64) as u32)
}
