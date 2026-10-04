//! `_sre`: the native half of Python's `re`. `re._compiler` produces the opcode list; the
//! shared regex engine in `lumen-common` runs it (see `lumen_common::regex::sre`). This module
//! provides the `Pattern`, `Match`, scanner and template objects around that engine, with the
//! iteration rules of CPython's `Modules/_sre/sre.c`.

use super::native::*;
use crate::ast::CmpOp;
use crate::object::*;
use crate::vm::*;
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

struct PatternData {
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

struct MatchData {
    pattern: Value,
    string: Value,
    marks: Vec<(i64, i64)>,
    pos: usize,
    endpos: usize,
    lastindex: i64,
}

struct ScannerData {
    pat: PatInfo,
    string: Value,
    input: Input,
    pos: usize,
    endpos: usize,
    start: Option<usize>,
    must_advance: bool,
    executing: bool,
}

#[derive(Clone)]
struct TemplateData {
    literal: Value,
    items: Vec<(usize, Option<Value>)>,
}

struct Types {
    pattern: Obj,
    matched: Obj,
    scanner: Obj,
    template: Obj,
}

fn types(it: &mut Interp) -> Types {
    let m = match dict_get_str(&it.modules, "_sre") {
        Some(Value::Obj(m)) => m,
        _ => unreachable!("_sre is loaded"),
    };
    let d = it.module_dict(&m);
    let get = |name: &str| match dict_get_str(&d, name) {
        Some(Value::Obj(t)) => t,
        _ => unreachable!("_sre types are registered"),
    };
    Types {
        pattern: get("_Pattern"),
        matched: get("_Match"),
        scanner: get("_Scanner"),
        template: get("_Template"),
    }
}

thread_local! {
    static WIDE: RefCell<Vec<(Obj, Rc<[u32]>)>> = const { RefCell::new(Vec::new()) };
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
                _ => return Err(not_a_string(it, string)),
            },
            _ => return Err(not_a_string(it, string)),
        };
        if input.isbytes && pat.isbytes == 0 {
            return Err(it.type_error("cannot use a string pattern on a bytes-like object"));
        }
        if !input.isbytes && pat.isbytes > 0 {
            return Err(it.type_error("cannot use a bytes pattern on a string-like object"));
        }
        Ok(input)
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

fn pat_info(it: &mut Interp, v: &Value) -> R<PatInfo> {
    match with_opaque::<PatternData, _>(v, |d| PatInfo {
        this: v.clone(),
        regex: d.regex.clone(),
        groups: d.groups,
        isbytes: d.isbytes,
    }) {
        Some(p) => Ok(p),
        None => Err(it.self_state_err("re.Pattern")),
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

/// `pos` and `endpos` as integers, converted before the subject is checked (as CPython's
/// argument parsing does); [`bounds`] clamps them to the subject.
fn indices(it: &mut Interp, pos: &Option<Value>, endpos: &Option<Value>) -> R<(i64, i64)> {
    let pos = match pos {
        Some(v) => it.index_of(v)?,
        None => 0,
    };
    let endpos = match endpos {
        Some(v) => it.index_of(v)?,
        None => i64::MAX,
    };
    Ok((pos, endpos))
}

fn bounds((pos, endpos): (i64, i64), len: usize) -> (usize, usize) {
    (clamp(pos, len), clamp(endpos, len))
}

fn new_match(
    it: &mut Interp,
    pat: &PatInfo,
    string: &Value,
    caps: &Captures,
    pos: usize,
    endpos: usize,
) -> Value {
    let ty = types(it).matched;
    let mut marks = Vec::with_capacity(pat.groups + 1);
    for g in 0..=pat.groups {
        marks.push(match caps.get(g).copied().flatten() {
            Some((a, b)) => (a as i64, b as i64),
            None => (-1, -1),
        });
    }
    let lastindex = caps.last_group().map_or(-1, |g| g as i64);
    new_opaque(
        &ty,
        MatchData {
            pattern: pat.this.clone(),
            string: string.clone(),
            marks,
            pos,
            endpos,
            lastindex,
        },
    )
}

fn no_instances(it: &mut Interp, name: &str) -> Obj {
    it.type_error(&format!("cannot create '{}' instances", name))
}

fn pattern_new(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(no_instances(it, "re.Pattern"))
}

fn match_new(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(no_instances(it, "re.Match"))
}

fn scanner_new(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(no_instances(it, "_sre.SRE_Scanner"))
}

fn template_new(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(no_instances(it, "_sre.SRE_Template"))
}

// ---- module functions ---------------------------------------------------------------------------

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

fn sre_compile(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args(
        "compile",
        a,
        kw,
        &[
            "pattern",
            "flags",
            "code",
            "groups",
            "groupindex",
            "indexgroup",
        ],
        6,
    )?;
    let get = |i: usize| b[i].clone().unwrap();
    let (pattern, flags_v, code_v, groups_v, groupindex, indexgroup) =
        (get(0), get(1), get(2), get(3), get(4), get(5));
    let flags = it.index_of(&flags_v)?;
    let Some(list) = list_of(&code_v) else {
        let t = it.type_name_of(&code_v);
        return Err(it.type_error(&format!(
            "compile() argument 'code' must be list, not {}",
            t
        )));
    };
    let items: Vec<Value> = list.borrow().clone();
    let mut code = Vec::with_capacity(items.len());
    for v in &items {
        code.push(code_word(it, v)?);
    }
    let groups = it.index_of(&groups_v)?;
    if dict_of(&groupindex).is_none() {
        let t = it.type_name_of(&groupindex);
        return Err(it.type_error(&format!(
            "compile() argument 'groupindex' must be dict, not {}",
            t
        )));
    }
    if indexgroup.tuple_items().is_none() {
        let t = it.type_name_of(&indexgroup);
        return Err(it.type_error(&format!(
            "compile() argument 'indexgroup' must be tuple, not {}",
            t
        )));
    }
    let isbytes = string_kind(it, &pattern)?;
    if groups < 0 || groups as u64 > sre::MAXGROUPS as u64 {
        return Err(it.new_exc_str("RuntimeError", "invalid SRE code"));
    }
    let regex = match sre::build(&code, groups as usize) {
        Ok(r) => r,
        Err(sre::SreError::Invalid) => {
            return Err(it.new_exc_str("RuntimeError", "invalid SRE code"))
        }
        Err(sre::SreError::Limit(msg)) => return Err(it.new_exc_str("OverflowError", &msg)),
    };
    let named = dict_of(&groupindex).map_or(0, |d| d.borrow().len());
    let (groupindex, indexgroup) = if named > 0 {
        let n = indexgroup.tuple_items().map_or(0, |t| t.len());
        (groupindex, if n > 0 { indexgroup } else { Value::None })
    } else {
        (Value::None, Value::None)
    };
    let ty = types(it).pattern;
    Ok(new_opaque(
        &ty,
        PatternData {
            pattern,
            flags,
            groups: groups as usize,
            groupindex,
            indexgroup,
            isbytes,
            code,
            regex: Rc::new(regex),
        },
    ))
}

fn sre_template(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("template", a, 2, 2)?;
    let Some(list) = list_of(&a[1]) else {
        let t = it.type_name_of(&a[1]);
        return Err(it.type_error(&format!(
            "template() argument 'template' must be list, not {}",
            t
        )));
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
    let ty = types(it).template;
    Ok(new_opaque(
        &ty,
        TemplateData {
            literal: items[0].clone(),
            items: out,
        },
    ))
}

fn sre_getcodesize(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("getcodesize", a, 0, 0)?;
    Ok(Value::Int(4))
}

fn char_arg(it: &mut Interp, a: &[Value], name: &str) -> R<u32> {
    it.check_args(name, a, 1, 1)?;
    let n = it.index_of(&a[0])?;
    Ok(n.clamp(0, u32::MAX as i64) as u32)
}

fn sre_ascii_iscased(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let c = char_arg(it, a, "ascii_iscased")?;
    Ok(Value::Bool(c < 128 && (c as u8).is_ascii_alphabetic()))
}

/// `Py_UNICODE_TOUPPER`: the first code point of the full uppercase mapping.
fn upper_first(c: u32) -> u32 {
    match char::from_u32(c) {
        Some(ch) if c >= 128 => ch.to_uppercase().next().map_or(c, |u| u as u32),
        Some(_) => (c as u8).to_ascii_uppercase() as u32,
        None => c,
    }
}

fn sre_unicode_iscased(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let c = char_arg(it, a, "unicode_iscased")?;
    Ok(Value::Bool(c != regex::py_lower(c) || c != upper_first(c)))
}

fn sre_ascii_tolower(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let c = char_arg(it, a, "ascii_tolower")?;
    Ok(Value::Int(if c < 128 {
        (c as u8).to_ascii_lowercase() as i64
    } else {
        c as i64
    }))
}

fn sre_unicode_tolower(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let c = char_arg(it, a, "unicode_tolower")?;
    Ok(Value::Int(regex::py_lower(c) as i64))
}

// ---- Pattern ------------------------------------------------------------------------------------

fn match_like(it: &mut Interp, a: &[Value], kw: Kw, name: &str, mode: Mode) -> R<Value> {
    let b = it.bind_args(
        name,
        &a[1.min(a.len())..],
        kw,
        &["string", "pos", "endpos"],
        1,
    )?;
    let pat = pat_info(it, &a[0])?;
    let string = b[0].clone().unwrap();
    let at = indices(it, &b[1], &b[2])?;
    let input = Input::new(it, &pat, &string)?;
    let (start, end) = bounds(at, input.len);
    let opts = ExecOptions {
        start,
        end: Some(end),
        mode,
        must_advance: false,
    };
    Ok(match run(it, &pat, &input, opts)? {
        Some(caps) => new_match(it, &pat, &string, &caps, start, end),
        None => Value::None,
    })
}

fn pattern_match(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    match_like(it, a, kw, "match", Mode::Match)
}

fn pattern_fullmatch(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    match_like(it, a, kw, "fullmatch", Mode::FullMatch)
}

fn pattern_search(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    match_like(it, a, kw, "search", Mode::Search)
}

/// The text of capture group `g` in `caps`, or `None`/`''` when it did not participate.
fn group_text(input: &Input, string: &Value, caps: &Captures, g: usize, empty: bool) -> Value {
    match caps.get(g).copied().flatten() {
        Some((a, b)) => input.slice(string, a, b),
        None if empty => input.slice(string, 0, 0),
        None => Value::None,
    }
}

fn pattern_findall(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args(
        "findall",
        &a[1.min(a.len())..],
        kw,
        &["string", "pos", "endpos"],
        1,
    )?;
    let pat = pat_info(it, &a[0])?;
    let string = b[0].clone().unwrap();
    let at = indices(it, &b[1], &b[2])?;
    let input = Input::new(it, &pat, &string)?;
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
            0 => input.slice(&string, ms, me),
            1 => group_text(&input, &string, &caps, 1, true),
            n => Value::tuple(
                (1..=n)
                    .map(|g| group_text(&input, &string, &caps, g, true))
                    .collect(),
            ),
        });
        must_advance = me == ms;
        start = me;
    }
    Ok(Value::list(out))
}

fn pattern_scanner(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args(
        "scanner",
        &a[1.min(a.len())..],
        kw,
        &["string", "pos", "endpos"],
        1,
    )?;
    new_scanner(it, &a[0], &b)
}

fn new_scanner(it: &mut Interp, pattern: &Value, b: &[Option<Value>]) -> R<Value> {
    let pat = pat_info(it, pattern)?;
    let string = b[0].clone().unwrap();
    let at = indices(it, &b[1], &b[2])?;
    let input = Input::new(it, &pat, &string)?;
    let (pos, endpos) = bounds(at, input.len);
    let ty = types(it).scanner;
    Ok(new_opaque(
        &ty,
        ScannerData {
            pat,
            string,
            input,
            pos,
            endpos,
            start: Some(pos),
            must_advance: false,
            executing: false,
        },
    ))
}

fn pattern_finditer(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args(
        "finditer",
        &a[1.min(a.len())..],
        kw,
        &["string", "pos", "endpos"],
        1,
    )?;
    let scanner = new_scanner(it, &a[0], &b)?;
    let search = it.get_attr_str(&scanner, "search")?;
    Ok(it.mk_iter(IterState::CallIter {
        f: search,
        sentinel: Value::None,
        done: false,
    }))
}

fn pattern_split(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args(
        "split",
        &a[1.min(a.len())..],
        kw,
        &["string", "maxsplit"],
        1,
    )?;
    let pat = pat_info(it, &a[0])?;
    let string = b[0].clone().unwrap();
    let maxsplit = match &b[1] {
        Some(v) => it.index_of(v)?,
        None => 0,
    };
    let input = Input::new(it, &pat, &string)?;
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
        out.push(input.slice(&string, last, ms));
        for g in 1..=pat.groups {
            out.push(group_text(&input, &string, &caps, g, false));
        }
        n += 1;
        must_advance = me == ms;
        last = me;
        start = me;
    }
    out.push(input.slice(&string, last, end));
    Ok(Value::list(out))
}

enum Filter {
    Literal(Value),
    Template(TemplateData),
    Callable(Value),
}

fn compile_template(it: &mut Interp, pattern: &Value, template: &Value) -> R<TemplateData> {
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
    match with_opaque::<TemplateData, _>(&result, |t| t.clone()) {
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
    template: &TemplateData,
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

fn pattern_subx(it: &mut Interp, a: &[Value], kw: Kw, name: &str, subn: bool) -> R<Value> {
    let b = it.bind_args(
        name,
        &a[1.min(a.len())..],
        kw,
        &["repl", "string", "count"],
        2,
    )?;
    let pat = pat_info(it, &a[0])?;
    let (repl, string) = (b[0].clone().unwrap(), b[1].clone().unwrap());
    let count = match &b[2] {
        Some(v) => it.index_of(v)?,
        None => 0,
    };
    let filter = if it.is_callable(&repl) {
        Filter::Callable(repl)
    } else if is_literal_template(&repl) {
        Filter::Literal(match &repl {
            Value::Obj(o) if matches!(o.kind, Kind::ByteArray(_)) => {
                let Kind::ByteArray(v) = &o.kind else {
                    unreachable!()
                };
                Value::bytes(v.to_vec())
            }
            _ => repl,
        })
    } else {
        let t = compile_template(it, &pat.this, &repl)?;
        if t.items.is_empty() {
            Filter::Literal(t.literal)
        } else {
            Filter::Template(t)
        }
    };
    let input = Input::new(it, &pat, &string)?;
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
                let item = expand(it, t, pat.groups, &input, &string, &caps)?;
                out.push_value(it, &item)?;
            }
            Filter::Callable(f) => {
                let m = new_match(it, &pat, &string, &caps, 0, end);
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
    let result = out.finish();
    Ok(if subn {
        Value::tuple(vec![result, Value::Int(n)])
    } else {
        result
    })
}

fn pattern_sub(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    pattern_subx(it, a, kw, "sub", false)
}

fn pattern_subn(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    pattern_subx(it, a, kw, "subn", true)
}

fn return_self(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__copy__", a, 1, 2)?;
    Ok(a[0].clone())
}

fn class_getitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__class_getitem__", a, 2, 2)?;
    Ok(it.make_alias(a[0].clone(), &a[1]))
}

fn pattern_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let Some((pattern, mut flags, isbytes)) =
        with_opaque::<PatternData, _>(&a[0], |d| (d.pattern.clone(), d.flags, d.isbytes))
    else {
        return Err(it.self_state_err("re.Pattern"));
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
    Ok(Value::string(if parts.is_empty() {
        format!("re.compile({})", shown)
    } else {
        format!("re.compile({}, {})", shown, parts.join("|"))
    }))
}

fn pattern_hash(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let Some((pattern, flags, isbytes, code)) = with_opaque::<PatternData, _>(&a[0], |d| {
        (d.pattern.clone(), d.flags, d.isbytes, d.code.clone())
    }) else {
        return Err(it.self_state_err("re.Pattern"));
    };
    let mut hash = it.hash_value(&pattern)?;
    let bytes: Vec<u8> = code.iter().flat_map(|w| w.to_ne_bytes()).collect();
    hash ^= hash_bytes(&bytes);
    hash ^= flags ^ isbytes as i64 ^ code.len() as i64;
    Ok(Value::Int(if hash == -1 { -2 } else { hash }))
}

fn pattern_eq_impl(it: &mut Interp, a: &[Value], ne: bool) -> R<Value> {
    let key = |v: &Value| {
        with_opaque::<PatternData, _>(v, |d| {
            (d.pattern.clone(), d.flags, d.isbytes, d.code.clone())
        })
    };
    let (Some(l), Some(r)) = (key(&a[0]), key(&a[1])) else {
        return Ok(Value::NotImplemented);
    };
    if a[0].is(&a[1]) {
        return Ok(Value::Bool(!ne));
    }
    let same = l.1 == r.1 && l.2 == r.2 && l.3 == r.3 && it.values_eq(&l.0, &r.0)?;
    Ok(Value::Bool(same != ne))
}

fn pattern_eq(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__eq__", a, 2, 2)?;
    pattern_eq_impl(it, a, false)
}

fn pattern_ne(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__ne__", a, 2, 2)?;
    pattern_eq_impl(it, a, true)
}

fn pattern_cmp_other(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__lt__", a, 2, 2)?;
    Ok(Value::NotImplemented)
}

fn pattern_groupindex(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match with_opaque::<PatternData, _>(&a[0], |d| d.groupindex.clone()) {
        Some(Value::None) => Ok(Value::Obj(it.new_dict())),
        Some(d) => Ok(it.new_mappingproxy(d)),
        None => Err(it.self_state_err("re.Pattern")),
    }
}

fn pattern_pattern(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    with_opaque::<PatternData, _>(&a[0], |d| d.pattern.clone())
        .ok_or_else(|| it.self_state_err("re.Pattern"))
}

fn pattern_flags(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    with_opaque::<PatternData, _>(&a[0], |d| Value::Int(d.flags))
        .ok_or_else(|| it.self_state_err("re.Pattern"))
}

fn pattern_groups(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    with_opaque::<PatternData, _>(&a[0], |d| Value::Int(d.groups as i64))
        .ok_or_else(|| it.self_state_err("re.Pattern"))
}

// ---- Match --------------------------------------------------------------------------------------

fn md<X>(it: &mut Interp, v: &Value, f: impl FnOnce(&MatchData) -> X) -> R<X> {
    with_opaque::<MatchData, _>(v, |d| f(d)).ok_or_else(|| it.self_state_err("re.Match"))
}

/// A slice of the match's subject by character offsets, clamped to its current length.
fn subject_slice(string: &Value, a: i64, b: i64) -> Value {
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
        _ => Value::None,
    }
}

fn group_index(it: &mut Interp, m: &Value, index: Option<&Value>) -> R<usize> {
    let (pattern, groups) = md(it, m, |d| (d.pattern.clone(), d.marks.len()))?;
    let found: Option<i64> = match index {
        None => Some(0),
        Some(v) if it.has_index(v) => Some(it.index_of(v)?),
        Some(v) => {
            let gi = with_opaque::<PatternData, _>(&pattern, |d| d.groupindex.clone())
                .unwrap_or(Value::None);
            match &gi {
                Value::Obj(d) => match it.dict_get(d, v)? {
                    Some(Value::Int(n)) => Some(n),
                    _ => None,
                },
                _ => None,
            }
        }
    };
    match found {
        Some(i) if i >= 0 && (i as usize) < groups => Ok(i as usize),
        _ => Err(it.new_exc_str("IndexError", "no such group")),
    }
}

fn group_slice(it: &mut Interp, m: &Value, index: usize, default: &Value) -> R<Value> {
    let (string, mark) = md(it, m, |d| (d.string.clone(), d.marks[index]))?;
    if mark.0 < 0 {
        return Ok(default.clone());
    }
    Ok(subject_slice(&string, mark.0, mark.1))
}

fn match_getslice(it: &mut Interp, m: &Value, index: Option<&Value>, default: &Value) -> R<Value> {
    let i = group_index(it, m, index)?;
    group_slice(it, m, i, default)
}

fn match_group(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("group", kw)?;
    match a.len() {
        1 => match_getslice(it, &a[0], None, &Value::None),
        2 => match_getslice(it, &a[0], Some(&a[1]), &Value::None),
        _ => {
            let mut out = Vec::with_capacity(a.len() - 1);
            for g in &a[1..] {
                out.push(match_getslice(it, &a[0], Some(g), &Value::None)?);
            }
            Ok(Value::tuple(out))
        }
    }
}

fn match_getitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__getitem__", a, 2, 2)?;
    match_getslice(it, &a[0], Some(&a[1]), &Value::None)
}

fn match_groups(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("groups", &a[1.min(a.len())..], kw, &["default"], 0)?;
    let default = b[0].clone().unwrap_or(Value::None);
    let n = md(it, &a[0], |d| d.marks.len())?;
    let mut out = Vec::with_capacity(n.saturating_sub(1));
    for g in 1..n {
        out.push(group_slice(it, &a[0], g, &default)?);
    }
    Ok(Value::tuple(out))
}

fn match_groupdict(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("groupdict", &a[1.min(a.len())..], kw, &["default"], 0)?;
    let default = b[0].clone().unwrap_or(Value::None);
    let pattern = md(it, &a[0], |d| d.pattern.clone())?;
    let result = it.new_dict();
    let gi =
        with_opaque::<PatternData, _>(&pattern, |d| d.groupindex.clone()).unwrap_or(Value::None);
    if let Value::Obj(d) = &gi {
        let keys = crate::containers::pydict_of(d)
            .map(|p| p.borrow().keys())
            .unwrap_or_default();
        for key in keys {
            let v = match_getslice(it, &a[0], Some(&key), &default)?;
            it.dict_set(&result, key, v)?;
        }
    }
    Ok(Value::Obj(result))
}

fn mark_arg(it: &mut Interp, a: &[Value], name: &str) -> R<(usize, (i64, i64))> {
    it.check_args(name, a, 1, 2)?;
    let i = group_index(it, &a[0], a.get(1))?;
    let mark = md(it, &a[0], |d| d.marks[i])?;
    Ok((i, mark))
}

fn match_start(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("start", kw)?;
    Ok(Value::Int(mark_arg(it, a, "start")?.1 .0))
}

fn match_end(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("end", kw)?;
    Ok(Value::Int(mark_arg(it, a, "end")?.1 .1))
}

fn match_span(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("span", kw)?;
    let m = mark_arg(it, a, "span")?.1;
    Ok(Value::tuple(vec![Value::Int(m.0), Value::Int(m.1)]))
}

fn match_regs(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let marks = md(it, &a[0], |d| d.marks.clone())?;
    Ok(Value::tuple(
        marks
            .into_iter()
            .map(|(s, e)| Value::tuple(vec![Value::Int(s), Value::Int(e)]))
            .collect(),
    ))
}

fn match_expand(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("expand", &a[1.min(a.len())..], kw, &["template"], 1)?;
    let template = b[0].clone().unwrap();
    let (pattern, string, marks) = md(it, &a[0], |d| {
        (d.pattern.clone(), d.string.clone(), d.marks.clone())
    })?;
    let t = compile_template(it, &pattern, &template)?;
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
            let item = subject_slice(&string, mark.0, mark.1);
            out.push_value(it, &item)?;
        }
        if let Some(l) = literal {
            out.push_value(it, l)?;
        }
    }
    Ok(out.finish())
}

fn match_lastindex(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let i = md(it, &a[0], |d| d.lastindex)?;
    Ok(if i >= 0 { Value::Int(i) } else { Value::None })
}

fn match_lastgroup(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let (pattern, i) = md(it, &a[0], |d| (d.pattern.clone(), d.lastindex))?;
    let ig =
        with_opaque::<PatternData, _>(&pattern, |d| d.indexgroup.clone()).unwrap_or(Value::None);
    Ok(match ig.tuple_items() {
        Some(t) if i >= 0 && (i as usize) < t.len() => t[i as usize].clone(),
        _ => Value::None,
    })
}

fn match_string(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    md(it, &a[0], |d| d.string.clone())
}

fn match_re(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    md(it, &a[0], |d| d.pattern.clone())
}

fn match_pos(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    md(it, &a[0], |d| Value::Int(d.pos as i64))
}

fn match_endpos(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    md(it, &a[0], |d| Value::Int(d.endpos as i64))
}

fn match_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let span = md(it, &a[0], |d| d.marks[0])?;
    let group0 = group_slice(it, &a[0], 0, &Value::None)?;
    let shown: String = it.repr_of(&group0)?.chars().take(50).collect();
    Ok(Value::string(format!(
        "<re.Match object; span=({}, {}), match={}>",
        span.0, span.1, shown
    )))
}

// ---- Scanner ------------------------------------------------------------------------------------

fn scanner_step(it: &mut Interp, a: &[Value], mode: Mode) -> R<Value> {
    let Some(entered) = with_opaque::<ScannerData, _>(&a[0], |s| {
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
    }) else {
        return Err(it.self_state_err("_sre.SRE_Scanner"));
    };
    let Some((pat, string, input, pos, endpos, start, must_advance)) = entered else {
        return Err(it.value_error("regular expression scanner already executing"));
    };
    let finish = |it: &mut Interp, outcome: R<Option<Captures>>| -> R<Value> {
        let value = match &outcome {
            Ok(Some(caps)) => new_match(it, &pat, &string, caps, pos, endpos),
            _ => Value::None,
        };
        with_opaque::<ScannerData, _>(&a[0], |s| {
            s.executing = false;
            if outcome.is_ok() {
                match &outcome {
                    Ok(Some(caps)) => {
                        let (ms, me) = caps[0].unwrap();
                        s.must_advance = ms == me;
                        s.start = Some(me);
                    }
                    _ => s.start = None,
                }
            }
        });
        outcome.map(|_| value)
    };
    let Some(start) = start else {
        return finish(it, Ok(None)).map(|_| Value::None);
    };
    let opts = ExecOptions {
        start,
        end: Some(endpos),
        mode,
        must_advance,
    };
    let outcome = run(it, &pat, &input, opts);
    finish(it, outcome)
}

fn scanner_match(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("match", a, 1, 1)?;
    scanner_step(it, a, Mode::Match)
}

fn scanner_search(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("search", a, 1, 1)?;
    scanner_step(it, a, Mode::Search)
}

fn scanner_pattern(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    with_opaque::<ScannerData, _>(&a[0], |s| s.pat.this.clone())
        .ok_or_else(|| it.self_state_err("_sre.SRE_Scanner"))
}

pub fn make(it: &mut Interp) -> Obj {
    let m = it.new_module("_sre");
    let d = it.module_dict(&m);
    it.register_module("_sre", &m);

    dict_set_str(&d, "MAGIC", Value::Int(sre::MAGIC as i64));
    dict_set_str(&d, "CODESIZE", Value::Int(4));
    dict_set_str(&d, "MAXREPEAT", Value::Int(sre::MAXREPEAT as i64));
    dict_set_str(&d, "MAXGROUPS", Value::Int(sre::MAXGROUPS as i64));
    dict_set_str(
        &d,
        "copyright",
        Value::str(" SRE 2.2.2 Copyright (c) 1997-2002 by Secret Labs AB "),
    );
    let funcs: &[(&'static str, NativeFn)] = &[
        ("compile", sre_compile),
        ("template", sre_template),
        ("getcodesize", sre_getcodesize),
        ("ascii_iscased", sre_ascii_iscased),
        ("unicode_iscased", sre_unicode_iscased),
        ("ascii_tolower", sre_ascii_tolower),
        ("unicode_tolower", sre_unicode_tolower),
    ];
    for (name, f) in funcs {
        set_fn(it, &d, name, *f);
    }

    let pattern = new_type(it, "re", "Pattern", None, Layout::Other);
    it.reg_new(&pattern, pattern_new);
    let methods: &[(&'static str, NativeFn)] = &[
        ("match", pattern_match),
        ("fullmatch", pattern_fullmatch),
        ("search", pattern_search),
        ("sub", pattern_sub),
        ("subn", pattern_subn),
        ("findall", pattern_findall),
        ("split", pattern_split),
        ("finditer", pattern_finditer),
        ("scanner", pattern_scanner),
        ("__copy__", return_self),
        ("__deepcopy__", return_self),
        ("__repr__", pattern_repr),
        ("__hash__", pattern_hash),
        ("__eq__", pattern_eq),
        ("__ne__", pattern_ne),
        ("__lt__", pattern_cmp_other),
        ("__le__", pattern_cmp_other),
        ("__gt__", pattern_cmp_other),
        ("__ge__", pattern_cmp_other),
    ];
    for (name, f) in methods {
        it.reg(&pattern, name, *f);
    }
    it.reg_class(&pattern, "__class_getitem__", class_getitem);
    it.reg_prop(&pattern, "groupindex", pattern_groupindex);
    it.reg_prop(&pattern, "pattern", pattern_pattern);
    it.reg_prop(&pattern, "flags", pattern_flags);
    it.reg_prop(&pattern, "groups", pattern_groups);
    set_type(&d, "_Pattern", &pattern);

    let matched = new_type(it, "re", "Match", None, Layout::Other);
    it.reg_new(&matched, match_new);
    let methods: &[(&'static str, NativeFn)] = &[
        ("group", match_group),
        ("start", match_start),
        ("end", match_end),
        ("span", match_span),
        ("groups", match_groups),
        ("groupdict", match_groupdict),
        ("expand", match_expand),
        ("__getitem__", match_getitem),
        ("__copy__", return_self),
        ("__deepcopy__", return_self),
        ("__repr__", match_repr),
    ];
    for (name, f) in methods {
        it.reg(&matched, name, *f);
    }
    it.reg_class(&matched, "__class_getitem__", class_getitem);
    it.reg_prop(&matched, "lastindex", match_lastindex);
    it.reg_prop(&matched, "lastgroup", match_lastgroup);
    it.reg_prop(&matched, "regs", match_regs);
    it.reg_prop(&matched, "string", match_string);
    it.reg_prop(&matched, "re", match_re);
    it.reg_prop(&matched, "pos", match_pos);
    it.reg_prop(&matched, "endpos", match_endpos);
    set_type(&d, "_Match", &matched);

    let scanner = new_type(it, "_sre", "SRE_Scanner", None, Layout::Other);
    it.reg_new(&scanner, scanner_new);
    it.reg(&scanner, "match", scanner_match);
    it.reg(&scanner, "search", scanner_search);
    it.reg_prop(&scanner, "pattern", scanner_pattern);
    set_type(&d, "_Scanner", &scanner);

    let template = new_type(it, "_sre", "SRE_Template", None, Layout::Other);
    it.reg_new(&template, template_new);
    set_type(&d, "_Template", &template);
    m
}

#[allow(dead_code)]
fn unused(_: CmpOp) {}
