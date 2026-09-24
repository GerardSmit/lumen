//! V8-format error stack traces: `err.stack`, `Error.captureStackTrace`,
//! `Error.stackTraceLimit` and `Error.prepareStackTrace` with CallSite objects.
//!
//! ## Capture (at error construction: cheap)
//! Every tier stores the position of the call it is about to make in `Interp::cur_site` (the
//! tree-walker a source offset, the VM and JIT `SITE_PC | pc`), and every [`FnFrame`] keeps its
//! caller's ([`FnFrame::caller_site`]). An error snapshots up to `Error.stackTraceLimit` frames
//! into a hidden raw array, [`SLOTS`] values per frame: the callee function and its site,
//! nothing decoded. Top-level script, module and eval code runs under a *pseudo* frame
//! (`fn_ptr` 0, [`ScriptFrame`]); those are short-lived, so their line and column are resolved
//! at capture.
//!
//! ## Format (on the first `stack` read)
//! A function frame's site becomes a position through the function's chunk
//! (`Chunk::call_site_pos`, whose table is decoded only here) and a line/column through its
//! source's line table ([`Sources`]), built once per source on first use (or, for an
//! ahead-of-time unit whose text is not in the binary, read from the blob). The header is
//! `ErrorUtils::ToString` of the error at that moment, and the result is cached on the object —
//! V8's observable behaviour. With `Error.prepareStackTrace` set to a function, it is called
//! instead with the error and an array of CallSite objects, and its result (any value) becomes
//! `stack`.
//!
//! Lines and columns are 1-based, columns in UTF-16 units, counted from the source's registered
//! body start: a CommonJS module wrapper's header is not part of the file.
//!
//! ## Limitations
//! - A running builtin gets V8's `at Array.map (<anonymous>)` frame from its
//!   [`NativeCtx`] (named by its receiver and the realm's intrinsics; host-internal natives have
//!   none; until the first such trace an object receiver is not held, so that one trace names it
//!   by the method's home: `Array.map` for a subclass instance), but a JS function's receiver is not tracked (V8's `Object.m` is plain `m`), and
//!   awaiting callers get no `at async f` frames.
//! - A frame's position is the last call it made, so an error the engine throws outside a call
//!   (`null.x`) reports the frame's most recent call position, not the throwing expression.
//! - A callee the JIT inlined has no frame of its own.

use super::frames::{
    site_is_native, site_pos, FnFrame, FrameExtra, NativeCtx, NO_SITE, PRIM_NAMES,
    RECORD_RECEIVERS, RECV_NONE, RECV_OBJ, RECV_PRIM, SITE_PC,
};
use super::Interp;
use crate::ast::{FnSource, Function, NO_POS};
use crate::value::{Callable, Exotic, Gc, Object, Property, Value, WeakGc};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// Values per frame in a raw trace: `[callee | undefined, a, b, flags, extra]`. A function frame
/// is `[callee, site, 0, flags, undefined]`; a pseudo frame `[undefined, line | -1, column,
/// flags | F_PSEUDO, file (or, for eval code, its origin)]`.
const SLOTS: usize = 5;
const F_CONSTRUCT: u32 = 1;
const F_PSEUDO: u32 = 2;
const F_EVAL: u32 = 4;
/// A builtin's frame synthesized for an inlined array callback (`with F_PSEUDO`): `extra` is its
/// name, printed as `Array.map (<anonymous>)` (see `bytecode::inline_callback`).
const F_NATIVE: u32 = 8;

/// Hidden own key holding the raw trace of an object `Error.captureStackTrace` was called on.
const RAW_KEY: &str = "#\u{0}rawstack";
/// Hidden own key caching the formatted `stack` value.
const STACK_KEY: &str = "#\u{0}stack";
/// Hidden own key of a CallSite object's data (see [`callsite`]).
const CS_KEY: &str = "#\u{0}callsite";

thread_local! {
    /// The source the last top-level parse / precompiled decode on this thread read from (and,
    /// for a precompiled unit, its line table): what a caller about to run that code takes with
    /// [`take_parsed_source`] to name its pseudo frame and register the source.
    static PARSED: RefCell<Option<(Rc<str>, Option<Rc<LineTable>>)>> = const { RefCell::new(None) };
    /// Inside an `Error.prepareStackTrace` call: a nested format uses the default text (V8).
    static IN_PREPARE: Cell<bool> = const { Cell::new(false) };
}

/// Note the source a top-level parse read (see [`take_parsed_source`]).
pub(crate) fn note_parsed_source(src: Rc<str>, table: Option<Rc<LineTable>>) {
    PARSED.with(|p| *p.borrow_mut() = Some((src, table)));
}

/// The source of the parse / decode that just ran on this thread (see [`note_parsed_source`]).
pub(crate) fn take_parsed_source() -> Option<(Rc<str>, Option<Rc<LineTable>>)> {
    PARSED.with(|p| p.borrow_mut().take())
}

/// A top-level activation (script, module or eval code): a stack-trace line without a function.
pub(crate) struct ScriptFrame {
    /// The source its positions are offsets into (a [`Sources`] key).
    pub(crate) src: Rc<str>,
    pub(crate) eval: bool,
}

/// What a coroutine pushes as its frame each time it resumes (a resumed generator or async
/// body runs outside the call that created it, so it would otherwise have no frame).
#[derive(Default)]
pub(crate) enum ResumeFrame {
    #[default]
    None,
    /// The function whose body it is. `skip_first`: the first resume runs inside that
    /// function's own call, which already has the frame (an async function's first step).
    Fn { f: WeakGc, skip_first: bool },
    /// A module body with top-level `await`.
    Script(Rc<ScriptFrame>),
}

/// Line starts and UTF-16 column corrections of one source text.
pub(crate) struct LineTable {
    /// Byte offset of every line start (the first is 0).
    starts: Vec<u32>,
    /// `(byte offset just past a non-ASCII char, cumulative UTF-8 minus UTF-16 length)`.
    wide: Vec<(u32, u32)>,
    len: u32,
    /// An ahead-of-time blob's encoded table ([`LineTable::lazy`]), decoded into `decoded` on
    /// the first lookup: a large bundle's table is sizeable and most runs
    /// never format a stack position in it.
    encoded: Option<&'static [u8]>,
    decoded: std::cell::OnceCell<Option<Box<LineTable>>>,
}

impl LineTable {
    /// A table decoded from `b` ([`LineTable::encode`]'s form) on its first lookup; a malformed
    /// `b` then yields no positions.
    pub(crate) fn lazy(b: &'static [u8]) -> LineTable {
        LineTable {
            starts: Vec::new(),
            wide: Vec::new(),
            len: 0,
            encoded: Some(b),
            decoded: std::cell::OnceCell::new(),
        }
    }

    /// The table of `text`. Line terminators are ECMAScript's: LF, CR, CRLF, U+2028, U+2029.
    pub(crate) fn build(text: &str) -> LineTable {
        let _mem = crate::memstats::enter(crate::memstats::Cat::StackTrace);
        let b = text.as_bytes();
        let (mut starts, mut wide, mut extra) = (vec![0u32], Vec::new(), 0u32);
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c < 0x80 {
                i += 1;
                if c == b'\n' {
                    starts.push(i as u32);
                } else if c == b'\r' {
                    if b.get(i) == Some(&b'\n') {
                        i += 1;
                    }
                    starts.push(i as u32);
                }
                continue;
            }
            let ch = text[i..].chars().next().unwrap_or('\0');
            i += ch.len_utf8();
            extra += (ch.len_utf8() - ch.len_utf16()) as u32;
            wide.push((i as u32, extra));
            if matches!(ch, '\u{2028}' | '\u{2029}') {
                starts.push(i as u32);
            }
        }
        LineTable {
            starts,
            wide,
            len: b.len() as u32,
            encoded: None,
            decoded: std::cell::OnceCell::new(),
        }
    }

    fn extra_before(&self, p: u32) -> u32 {
        match self.wide.partition_point(|&(q, _)| q <= p) {
            0 => 0,
            k => self.wide[k - 1].1,
        }
    }

    /// 1-based line and 0-based UTF-16 column of byte offset `p`.
    pub(crate) fn line_col(&self, p: u32) -> Option<(u32, u32)> {
        if let Some(b) = self.encoded {
            return self
                .decoded
                .get_or_init(|| {
                    let _mem = crate::memstats::enter(crate::memstats::Cat::StackTrace);
                    LineTable::decode(b).map(Box::new)
                })
                .as_ref()?
                .line_col(p);
        }
        if p > self.len {
            return None;
        }
        let l = self.starts.partition_point(|&s| s <= p).max(1);
        let s = self.starts[l - 1];
        Some((
            l as u32,
            (p - s) - (self.extra_before(p) - self.extra_before(s)),
        ))
    }

    /// The compact form an ahead-of-time blob stores (LEB128: length, line count, line-start
    /// deltas, correction count, correction deltas).
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let put = |out: &mut Vec<u8>, mut v: u32| loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                break;
            }
            out.push(b | 0x80);
        };
        put(&mut out, self.len);
        put(&mut out, self.starts.len() as u32 - 1);
        let mut prev = 0;
        for &s in &self.starts[1..] {
            put(&mut out, s - prev);
            prev = s;
        }
        put(&mut out, self.wide.len() as u32);
        let (mut pp, mut pe) = (0, 0);
        for &(p, e) in &self.wide {
            put(&mut out, p - pp);
            put(&mut out, e - pe);
            (pp, pe) = (p, e);
        }
        out
    }

    /// Read [`LineTable::encode`]'s form (`None` when malformed).
    pub(crate) fn decode(b: &[u8]) -> Option<LineTable> {
        let mut at = 0usize;
        let mut get = || -> Option<u32> {
            let mut v = 0u32;
            for shift in (0..35).step_by(7) {
                let byte = *b.get(at)?;
                at += 1;
                v |= ((byte & 0x7f) as u32) << shift;
                if byte & 0x80 == 0 {
                    return Some(v);
                }
            }
            None
        };
        let len = get()?;
        let n = get()? as usize;
        let mut starts = Vec::with_capacity(n.min(1 << 20) + 1);
        starts.push(0);
        let mut prev = 0u32;
        for _ in 0..n {
            prev = prev.checked_add(get()?)?;
            starts.push(prev);
        }
        let m = get()? as usize;
        let mut wide = Vec::with_capacity(m.min(1 << 20));
        let (mut pp, mut pe) = (0u32, 0u32);
        for _ in 0..m {
            pp = pp.checked_add(get()?)?;
            pe = pe.checked_add(get()?)?;
            wide.push((pp, pe));
        }
        Some(LineTable {
            starts,
            wide,
            len,
            encoded: None,
            decoded: std::cell::OnceCell::new(),
        })
    }
}

/// A known source: its display name and where its own text starts.
pub(crate) struct SourceInfo {
    /// Pins the key's allocation (so the address cannot be reused while the entry exists) and
    /// tells whether the source is still alive.
    src: std::rc::Weak<str>,
    name: Value,
    /// Byte offset where the file's own text begins (after a synthesized wrapper header).
    body_start: u32,
    /// The CommonJS wrapper function of this source (`Gc::as_ptr`; 0 = none): V8 names its
    /// frame `Object.<anonymous>`.
    wrapper: usize,
    table: Option<Rc<LineTable>>,
}

/// The source registry, keyed by the address of a source's `Rc<str>` text (the `src` every
/// function parsed from it shares). Entries for sources nobody registered are created on first
/// use (named `<anonymous>`) to cache their line tables; dead entries are pruned as it grows.
#[derive(Default)]
pub(crate) struct Sources {
    map: HashMap<usize, SourceInfo>,
    prune_at: usize,
}

fn key_of(src: &Rc<str>) -> usize {
    Rc::as_ptr(src) as *const u8 as usize
}

impl Sources {
    fn entry(&mut self, src: &Rc<str>) -> &mut SourceInfo {
        let key = key_of(src);
        let live = self.map.get(&key).is_some_and(|e| e.src.strong_count() > 0);
        if !live {
            if self.map.len() >= self.prune_at {
                self.map.retain(|_, e| e.src.strong_count() > 0);
                self.prune_at = (self.map.len() * 2).max(64);
            }
            self.map.insert(
                key,
                SourceInfo {
                    src: Rc::downgrade(src),
                    name: Value::str("<anonymous>"),
                    body_start: 0,
                    wrapper: 0,
                    table: None,
                },
            );
        }
        self.map.get_mut(&key).unwrap()
    }

    /// Name `src`, say where its own text starts and which function wraps it, and give it a
    /// line table (each `None` / 0 keeps what the entry has).
    fn register(
        &mut self,
        src: &Rc<str>,
        name: Option<&str>,
        body_start: Option<u32>,
        wrapper: usize,
        table: Option<Rc<LineTable>>,
    ) {
        let e = self.entry(src);
        if let Some(name) = name {
            e.name = Value::from_string(name.to_string());
        }
        if let Some(b) = body_start {
            e.body_start = b;
        }
        if wrapper != 0 {
            e.wrapper = wrapper;
        }
        if table.is_some() {
            e.table = table;
        }
    }

    /// The display name of `src`, the line and column of `pos` in it (relative to its body
    /// start; `None` without a position) and its wrapper function.
    fn describe(&mut self, src: &Rc<str>, pos: u32) -> (Value, Option<(u32, u32)>, usize) {
        let e = self.entry(src);
        let lc = if pos == NO_POS {
            None
        } else {
            let t = e
                .table
                .get_or_insert_with(|| Rc::new(LineTable::build(src)))
                .clone();
            match (t.line_col(pos), t.line_col(e.body_start)) {
                (Some((l, c)), Some((bl, bc))) if pos >= e.body_start => Some(if l == bl {
                    (1, c - bc + 1)
                } else {
                    (l - bl + 1, c + 1)
                }),
                _ => None,
            }
        };
        (e.name.clone(), lc, e.wrapper)
    }
}

/// The source text a function's positions are offsets into.
fn source_of(func: &Function) -> Option<Rc<str>> {
    if let Some(l) = func.lazy.borrow().as_ref() {
        return Some(l.src.clone());
    }
    match &func.source {
        FnSource::Range { src, .. } => Some(src.clone()),
        _ => None,
    }
}

/// One frame, resolved for display.
struct Resolved {
    func: Value,
    name: Option<String>,
    file: Value,
    line: Option<(u32, u32)>,
    flags: u32,
    /// A CommonJS module wrapper (`Object.<anonymous>`).
    wrapper: bool,
    strict: bool,
}

/// V8's frame text for a running native (without ` (<anonymous>)`): `Array.map`,
/// `String.replace`, `JSON.parse`, `new Promise`, `String`. `None` for a call adaptor V8 does
/// not show and for a native with no name among the realm's intrinsics (host internals).
fn native_frame_name(i: &Interp, c: &NativeCtx) -> Option<String> {
    if c.hidden.get() {
        return None;
    }
    let (name, home) = native_fn_name(i, c.id)?;
    // A protocol method another builtin dispatched to (`String.prototype.replace` calling
    // `RegExp.prototype[@@replace]`): V8 shows only the outer builtin.
    if site_is_native(c.site) && name.starts_with("[Symbol.") {
        return None;
    }
    if c.construct {
        return Some(format!("new {name}"));
    }
    match c.recv_kind {
        RECV_NONE => Some(name.to_string()),
        RECV_OBJ if c.this.is_null() => {
            // Not held (see `RECORD_RECEIVERS`): name it by its home this time, and hold
            // receivers from now on.
            RECORD_RECEIVERS.store(true, std::sync::atomic::Ordering::Relaxed);
            Some(match home {
                Some(h) => format!("{h}.{name}"),
                None => name.to_string(),
            })
        }
        RECV_OBJ => Some(format!(
            "{}.{name}",
            receiver_type_name(i, unsafe { &*c.this })
        )),
        k => Some(format!("{}.{name}", PRIM_NAMES[(k - RECV_PRIM) as usize])),
    }
}

/// Whether the frame text names a builtin that creates the error whose trace is taken.
fn is_capturing_builtin(text: &str) -> bool {
    let name = text.strip_prefix("new ").unwrap_or(text);
    let last = name.rsplit('.').next().unwrap_or(name);
    last == "captureStackTrace" || (last.ends_with("Error") && !name.contains('.'))
}

/// V8's type name of a builtin's receiver: a function's own name (`Array.from`), a primitive's
/// wrapper (`String.split`), else its constructor's name (`Array.map`, `A.map` for a subclass
/// instance), with an `Object` that has a `@@toStringTag` named by the tag (`JSON.parse`,
/// `Math.max`). Reads data properties only (no getter runs while a trace is taken).
fn receiver_type_name(i: &Interp, v: &Value) -> String {
    fn own_name(o: &Gc, key: &str) -> Option<String> {
        let b = o.try_borrow().ok()?;
        match b
            .props
            .get(key)
            .filter(|p| !p.accessor())
            .map(|p| p.value())
        {
            Some(Value::Str(s)) if !s.is_empty() => Some(s.to_string()),
            _ => None,
        }
    }
    /// The first data property `key` on `o`'s prototype chain.
    fn inherited(o: &Gc, key: &str) -> Option<Value> {
        let mut cur = Some(o.clone());
        let mut hops = 0;
        while let Some(c) = cur {
            let b = c.try_borrow().ok()?;
            if let Some(p) = b.props.get(key) {
                return (!p.accessor()).then(|| p.value());
            }
            hops += 1;
            if hops > 64 {
                return None;
            }
            cur = b.proto.clone();
        }
        None
    }
    let o = match v {
        Value::Str(_) => return "String".into(),
        Value::Num(_) => return "Number".into(),
        Value::Bool(_) => return "Boolean".into(),
        Value::Sym(_) => return "Symbol".into(),
        Value::BigInt(_) => return "BigInt".into(),
        Value::Obj(o) => o,
        _ => return "Object".into(),
    };
    if o.try_borrow().is_ok_and(|b| b.call.is_fn()) {
        return own_name(o, "name").unwrap_or_else(|| "Function".into());
    }
    let ctor = match inherited(o, "constructor") {
        Some(Value::Obj(f)) => own_name(&f, "name"),
        _ => None,
    };
    match ctor {
        Some(n) if n != "Object" => n,
        _ => {
            let tag = crate::builtins::to_string_tag_key(i).and_then(|k| inherited(o, &k));
            match tag {
                Some(Value::Str(t)) if !t.is_empty() => t.to_string(),
                _ => "Object".into(),
            }
        }
    }
}

thread_local! {
    /// `NativeFn` address -> function name and home (the type name of the object it was found
    /// on, `None` for a global function), for the realm's intrinsics (see [`native_fn_name`]),
    /// with the global object's property count it was last scanned at.
    static NATIVE_NAMES: RefCell<(usize, NameMap)> = RefCell::new((usize::MAX, HashMap::new()));
}

type NameMap = HashMap<usize, (Rc<str>, Option<Rc<str>>)>;

/// The name of the native function behind `id` (a `NativeFn` address): found among the global
/// object's functions, their own properties and prototypes' own properties (and
/// %TypedArray%'s), scanned on first need and again when the global object has grown.
fn native_fn_name(i: &Interp, id: usize) -> Option<(Rc<str>, Option<Rc<str>>)> {
    if id == 0 {
        return None;
    }
    NATIVE_NAMES.with(|t| {
        let mut t = t.borrow_mut();
        if let Some(n) = t.1.get(&id) {
            return Some(n.clone());
        }
        let len = i.global.try_borrow().ok()?.props.values().count();
        if t.0 == len {
            return None;
        }
        t.0 = len;
        scan_native_names(i, &mut t.1);
        t.1.get(&id).cloned()
    })
}

fn scan_native_names(i: &Interp, map: &mut NameMap) {
    fn data_objs(o: &Gc) -> Vec<Gc> {
        let Ok(b) = o.try_borrow() else {
            return Vec::new();
        };
        let mut v = Vec::new();
        for p in b.props.values() {
            for x in [Some(p.value()), p.getter().cloned(), p.setter().cloned()]
                .into_iter()
                .flatten()
            {
                if let Value::Obj(g) = x {
                    v.push(g);
                }
            }
        }
        v
    }
    fn record(o: &Gc, map: &mut NameMap, home: &Option<Rc<str>>) {
        let Ok(b) = o.try_borrow() else { return };
        if let Callable::Native(f) = &b.call {
            let name = match b
                .props
                .get("name")
                .filter(|p| !p.accessor())
                .map(|p| p.value())
            {
                Some(Value::Str(s)) if !s.is_empty() => Rc::from(s.to_string()),
                _ => return,
            };
            map.entry(*f as usize).or_insert((name, home.clone()));
        }
    }
    fn proto_of(o: &Gc) -> Option<Gc> {
        let b = o.try_borrow().ok()?;
        match b
            .props
            .get("prototype")
            .filter(|p| !p.accessor())
            .map(|p| p.value())
        {
            Some(Value::Obj(p)) => Some(p),
            _ => None,
        }
    }
    let mut holders = vec![i.global.clone()];
    for g in data_objs(&i.global) {
        holders.push(g.clone());
        if let Some(p) = proto_of(&g) {
            holders.push(p);
        }
    }
    // %TypedArray% and its prototype (the typed array methods live there).
    let ta = i
        .global
        .try_borrow()
        .ok()
        .and_then(|b| b.props.get("Int8Array").map(|p| p.value()));
    if let Some(Value::Obj(i8)) = ta {
        if let Some(t) = i8.try_borrow().ok().and_then(|b| b.proto.clone()) {
            if let Some(p) = proto_of(&t) {
                holders.push(p);
            }
            holders.push(t);
        }
    }
    for h in holders {
        let home = (!Gc::ptr_eq(&h, &i.global))
            .then(|| Rc::from(receiver_type_name(i, &Value::Obj(h.clone()))));
        record(&h, map, &None);
        for f in data_objs(&h) {
            record(&f, map, &home);
        }
    }
}

fn val_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        _ => String::new(),
    }
}

impl Resolved {
    fn location(&self) -> String {
        let file = if self.flags & F_EVAL != 0 {
            "<anonymous>".to_string()
        } else {
            val_str(&self.file)
        };
        match self.line {
            Some((l, c)) => format!("{file}:{l}:{c}"),
            None => file,
        }
    }

    /// The frame's text after `at `.
    fn text(&self) -> String {
        let loc = self.location();
        if self.flags & F_EVAL != 0 {
            return format!("eval ({}, {loc})", val_str(&self.file));
        }
        if self.flags & F_NATIVE != 0 {
            return format!("{} (<anonymous>)", val_str(&self.file));
        }
        if self.flags & F_PSEUDO != 0 {
            return loc;
        }
        let name = if self.wrapper {
            Some("Object.<anonymous>")
        } else {
            self.name.as_deref()
        };
        match (self.flags & F_CONSTRUCT != 0, name) {
            (true, n) => format!("new {} ({loc})", n.unwrap_or("<anonymous>")),
            (false, Some(n)) => format!("{n} ({loc})"),
            (false, None) => loc,
        }
    }
}

fn num(v: &Value) -> f64 {
    match v {
        Value::Num(n) => *n,
        _ => -1.0,
    }
}

/// How a module key prints in a stack trace: an absolute file path as a `file://` URL (as V8
/// shows ES modules), anything else (`aot:/…`, `node:…`, URLs) as is.
pub(crate) fn module_display_name(key: &str) -> String {
    let path = key.strip_prefix(r"\\?\").unwrap_or(key);
    let b = path.as_bytes();
    let drive =
        b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'/' | b'\\');
    if drive {
        format!("file:///{}", path.replace('\\', "/"))
    } else if path.starts_with('/') {
        format!("file://{path}")
    } else {
        key.to_string()
    }
}

impl Interp {
    /// Register `src` for stack traces (see [`Sources::register`]).
    pub(crate) fn register_source(
        &self,
        src: &Rc<str>,
        name: Option<&str>,
        body_start: Option<u32>,
        wrapper: usize,
        table: Option<Rc<LineTable>>,
    ) {
        self.sources
            .borrow_mut()
            .register(src, name, body_start, wrapper, table);
    }

    /// Take the source the parse / decode that just ran read from ([`take_parsed_source`]),
    /// registering it under `name` (`None`: keep its name) with its own text at `body_start`.
    pub(crate) fn adopt_parsed_source(
        &self,
        name: Option<&str>,
        body_start: u32,
    ) -> Option<Rc<str>> {
        let (src, table) = take_parsed_source()?;
        if name.is_some() || table.is_some() || body_start != 0 {
            self.register_source(&src, name, Some(body_start), 0, table);
        }
        Some(src)
    }

    /// Run a script body just parsed (or decoded) under its top-level frame.
    pub(crate) fn run_program_parsed(&mut self, body: &[crate::ast::Stmt]) -> Result<Value, Value> {
        self.run_program_named(body, None)
    }

    /// [`Interp::run_program_parsed`], naming the source `name`.
    pub(crate) fn run_program_named(
        &mut self,
        body: &[crate::ast::Stmt],
        name: Option<&str>,
    ) -> Result<Value, Value> {
        let src = self.adopt_parsed_source(name, 0);
        self.with_script_frame(src, false, |i| i.run_program(body))
    }

    /// Compile a CommonJS module as `function (params…) { src }` with no synthesized header
    /// (see `parser::parse_cjs_function`): positions are the file's, and its frames print as
    /// `filename`. TypeScript source (strip-only semantics) when `ts`; a TypeScript error
    /// carries Node's `code`.
    pub fn compile_cjs_function(
        &mut self,
        src: &str,
        params: &[&str],
        filename: &str,
        ts: bool,
    ) -> Result<Value, Value> {
        let f = crate::parser::parse_cjs_function(src, params, ts).map_err(|e| {
            if ts {
                match self.throw_ts_syntax(e, filename, src) {
                    crate::interpreter::Abrupt::Throw(v) => v,
                    _ => Value::Undefined,
                }
            } else {
                self.make_error("SyntaxError", e.message)
            }
        })?;
        let f = Rc::new(f);
        let env = self.global_env.clone();
        let func = self.make_function(f.clone(), env);
        if let (FnSource::Range { src, .. }, Value::Obj(o)) = (&f.source, &func) {
            self.register_source(src, Some(filename), Some(0), Gc::as_ptr(o) as usize, None);
        }
        Ok(func)
    }

    /// Name the source of the dynamic function `f` (`new Function(...)`, as a CommonJS loader
    /// compiles a module) after `filename`: its frames print as that file, with lines and
    /// columns counted from the body the caller passed (not the synthesized header), and a call
    /// of `f` itself as V8's `Object.<anonymous>`. `false` when `f` is not such a function.
    pub fn name_function_source(&mut self, f: &Value, filename: &str) -> bool {
        let Value::Obj(o) = f else { return false };
        let func = match &o.borrow().call {
            Callable::User(u) => u.func.clone(),
            _ => return false,
        };
        let Some(src) = source_of(&func) else {
            return false;
        };
        // The body starts after the synthesized `function anonymous(<params>\n) {\n` (a
        // precompiled wrapper, whose text is not kept, registered its own start at load).
        let body_start = src.find("\n) {\n").map(|at| (at + 5) as u32);
        self.register_source(
            &src,
            Some(filename),
            body_start,
            Gc::as_ptr(o) as usize,
            None,
        );
        true
    }

    /// Run `f` under a pseudo frame for top-level code of `src` (nothing pushed without one).
    pub(crate) fn with_script_frame<T>(
        &mut self,
        src: Option<Rc<str>>,
        eval: bool,
        f: impl FnOnce(&mut Interp) -> T,
    ) -> T {
        let Some(src) = src else { return f(self) };
        let depth = self.push_script_frame(Rc::new(ScriptFrame { src, eval }));
        let r = f(self);
        self.pop_pushed_frame(depth, 0);
        r
    }

    fn push_script_frame(&mut self, sf: Rc<ScriptFrame>) -> usize {
        crate::bytecode::jit::sync_frames(self);
        let depth = self.fn_frames.len();
        self.fn_frames.push(FnFrame {
            fn_ptr: 0,
            coro: self.cur_coro,
            caller_site: std::mem::replace(&mut self.cur_site, NO_SITE),
            strict: false,
            construct: false,
            extra: Some(Box::new(FrameExtra {
                script: Some(sf),
                ..Default::default()
            })),
        });
        depth
    }

    /// Pop the frame pushed at `depth` for `fn_ptr` (left alone if something unbalanced the
    /// stack meanwhile) and put its caller's site back.
    fn pop_pushed_frame(&mut self, depth: usize, fn_ptr: usize) {
        if self
            .fn_frames
            .get(depth)
            .is_some_and(|f| f.fn_ptr == fn_ptr)
        {
            let f = self.fn_frames.remove(depth);
            self.cur_site = f.caller_site;
        }
    }

    /// Push a resuming coroutine's frame (see [`ResumeFrame`]); `started`: it has run before.
    /// Returns what [`Interp::leave_resume_frame`] needs.
    pub(crate) fn enter_resume_frame(
        &mut self,
        frame: &ResumeFrame,
        started: bool,
    ) -> Option<(usize, usize, Option<Gc>)> {
        match frame {
            ResumeFrame::None => None,
            ResumeFrame::Fn { skip_first, .. } if *skip_first && !started => None,
            ResumeFrame::Fn { f, .. } => {
                let g = f.upgrade()?;
                let strict = match &g.borrow().call {
                    Callable::User(u) => u.func.is_strict,
                    _ => return None,
                };
                crate::bytecode::jit::sync_frames(self);
                let depth = self.fn_frames.len();
                let ptr = Gc::as_ptr(&g) as usize;
                self.fn_frames.push(FnFrame {
                    fn_ptr: ptr,
                    coro: self.cur_coro,
                    caller_site: std::mem::replace(&mut self.cur_site, NO_SITE),
                    strict,
                    construct: false,
                    extra: None,
                });
                // The handle keeps the callee alive while the frame is up (`FnFrame::fn_ptr`).
                Some((depth, ptr, Some(g)))
            }
            ResumeFrame::Script(sf) => Some((self.push_script_frame(sf.clone()), 0, None)),
        }
    }

    pub(crate) fn leave_resume_frame(&mut self, pushed: Option<(usize, usize, Option<Gc>)>) {
        if let Some((depth, ptr, keep)) = pushed {
            self.pop_pushed_frame(depth, ptr);
            drop(keep);
        }
    }

    /// The resume frame for a coroutine created inside the call of `func` (its frame is on
    /// top): that function, weakly.
    pub(crate) fn resume_frame_for(&self, func: &Rc<Function>, skip_first: bool) -> ResumeFrame {
        let Some(top) = self.fn_frames.last().filter(|f| f.fn_ptr != 0) else {
            return ResumeFrame::None;
        };
        let g = top.callee();
        let same = matches!(&g.borrow().call, Callable::User(u) if Rc::ptr_eq(&u.func, func));
        if !same {
            return ResumeFrame::None;
        }
        ResumeFrame::Fn {
            f: Gc::downgrade(&g),
            skip_first,
        }
    }

    /// The resume frame for a module body with top-level `await`.
    pub(crate) fn resume_frame_script(src: Option<Rc<str>>) -> ResumeFrame {
        match src {
            Some(src) => ResumeFrame::Script(Rc::new(ScriptFrame { src, eval: false })),
            None => ResumeFrame::None,
        }
    }

    /// `Error.stackTraceLimit` as a frame count; `None` (no stack at all) when it is not a
    /// number, as in V8.
    fn stack_trace_limit(&self) -> Option<usize> {
        let Some(ctor) = self.extra_protos.get("%Error%") else {
            return Some(10);
        };
        let v = ctor
            .borrow()
            .props
            .get("stackTraceLimit")
            .filter(|p| !p.accessor())
            .map(|p| p.value());
        match v {
            Some(Value::Num(n)) if n.is_nan() || n <= 0.0 => Some(0),
            Some(Value::Num(n)) => Some(if n >= 1e9 { usize::MAX } else { n as usize }),
            _ => None,
        }
    }

    /// Snapshot the call stack as a raw trace (`undefined` when `Error.stackTraceLimit` is not a
    /// number). `skip`: frames up to and including the innermost call of that function are left
    /// out; `must` = when it is not on the stack, leave out everything (`captureStackTrace`),
    /// else nothing (an error constructor's `new.target`).
    pub(crate) fn capture_trace(&self, skip: Option<(usize, bool)>) -> Value {
        let Some(limit) = self.stack_trace_limit() else {
            return Value::Undefined;
        };
        // The pending records of directly called JIT frames go on top of `fn_frames`.
        let pending = crate::bytecode::jit::pending_frames(self);
        let frames: Vec<&FnFrame> = self.fn_frames.iter().chain(pending.iter()).collect();
        let n = frames.len();
        let mut skipping = match skip {
            Some((p, must)) if must || frames.iter().any(|f| f.fn_ptr == p) => Some(p),
            _ => None,
        };
        let mut out: Vec<Value> = Vec::new();
        let mut count = 0;
        // The running natives, innermost first (see `frames::NativeCtx`).
        let mut natives: Vec<&NativeCtx> = Vec::new();
        let mut at = self.native_top;
        while at != 0 {
            let c = unsafe { &*(at as *const NativeCtx) };
            natives.push(c);
            at = c.prev;
        }
        let mut cursor = 0;
        for k in (0..n).rev() {
            let fr = frames[k];
            // The natives this frame is calling, innermost first: runs of natives another native
            // called (their sites carry the tag), each ending in one this frame called at the
            // position it is at. A call adaptor clears the tag, so its callee ends a run too.
            if cursor < natives.len() {
                let pos = site_pos(if k + 1 == n {
                    self.cur_site
                } else {
                    frames[k + 1].caller_site
                });
                let start = cursor;
                if pos != NO_SITE {
                    loop {
                        let mut e = cursor;
                        while e < natives.len() && site_is_native(natives[e].site) {
                            e += 1;
                        }
                        if e < natives.len() && site_pos(natives[e].site) == pos {
                            cursor = e + 1;
                        } else {
                            break;
                        }
                    }
                }
                if skipping.is_none() {
                    for (j, c) in natives[start..cursor].iter().enumerate() {
                        if count >= limit {
                            break;
                        }
                        let Some(name) = native_frame_name(self, c) else {
                            continue;
                        };
                        // The builtin creating the error (an Error constructor, or
                        // `Error.captureStackTrace`) is not part of its trace.
                        if start + j == 0 && k + 1 == n && is_capturing_builtin(&name) {
                            continue;
                        }
                        out.extend([
                            Value::Undefined,
                            Value::Num(-1.0),
                            Value::Num(0.0),
                            Value::Num((F_PSEUDO | F_NATIVE) as f64),
                            Value::from_string(name),
                        ]);
                        count += 1;
                    }
                }
            }
            if let Some(p) = skipping {
                if fr.fn_ptr == p {
                    skipping = None;
                }
                continue;
            }
            if count >= limit {
                break;
            }
            let site = super::frames::site_pos(if k + 1 == n {
                self.cur_site
            } else {
                frames[k + 1].caller_site
            });
            if fr.fn_ptr == 0 {
                let Some(sf) = fr.extra.as_ref().and_then(|x| x.script.clone()) else {
                    continue;
                };
                let pos = if site == NO_SITE || site & SITE_PC != 0 {
                    NO_POS
                } else {
                    site
                };
                let (file, lc, _) = self.sources.borrow_mut().describe(&sf.src, pos);
                let (line, col) = lc.map_or((-1.0, 0.0), |(l, c)| (l as f64, c as f64));
                let (flags, extra) = if sf.eval {
                    (F_PSEUDO | F_EVAL, self.eval_origin(k))
                } else {
                    (F_PSEUDO, file)
                };
                out.extend([
                    Value::Undefined,
                    Value::Num(line),
                    Value::Num(col),
                    Value::Num(flags as f64),
                    extra,
                ]);
            } else {
                // A call site inside inlined array callbacks: the arrow's and the builtin's
                // frames first, then this one at the method call (see `inline_callback`).
                let mut site = site;
                if site != NO_SITE
                    && site & SITE_PC != 0
                    && crate::bytecode::inline_callback::used()
                {
                    let callee = fr.callee();
                    if let Some((func, mut pos, regions)) = crate::bytecode::inline_callback::expand(
                        &callee,
                        (site & !SITE_PC) as usize,
                    ) {
                        let src = source_of(&func);
                        for (call_pos, name) in regions {
                            if count >= limit {
                                break;
                            }
                            let (file, lc) = match &src {
                                Some(s) => {
                                    let (f, lc, _) = self.sources.borrow_mut().describe(s, pos);
                                    (f, lc)
                                }
                                None => (Value::str("<anonymous>"), None),
                            };
                            let (line, col) = lc.map_or((-1.0, 0.0), |(l, c)| (l as f64, c as f64));
                            out.extend([
                                Value::Undefined,
                                Value::Num(line),
                                Value::Num(col),
                                Value::Num(F_PSEUDO as f64),
                                file,
                            ]);
                            count += 1;
                            if count >= limit {
                                break;
                            }
                            out.extend([
                                Value::Undefined,
                                Value::Num(-1.0),
                                Value::Num(0.0),
                                Value::Num((F_PSEUDO | F_NATIVE) as f64),
                                Value::str(name),
                            ]);
                            count += 1;
                            pos = call_pos;
                        }
                        if count >= limit {
                            break;
                        }
                        site = if pos == NO_POS { NO_SITE } else { pos };
                    }
                }
                let flags = if fr.construct { F_CONSTRUCT } else { 0 };
                out.extend([
                    Value::Obj(fr.callee()),
                    Value::Num(site as f64),
                    Value::Num(0.0),
                    Value::Num(flags as f64),
                    Value::Undefined,
                ]);
            }
            count += 1;
        }
        Value::Obj(Object::new_array_from_vec(None, out))
    }

    /// `eval at <caller> (<location>)` for the eval pseudo frame at index `k`.
    fn eval_origin(&self, k: usize) -> Value {
        let site = super::frames::site_pos(self.fn_frames[k].caller_site);
        let caller = match k.checked_sub(1).map(|j| &self.fn_frames[j]) {
            Some(fr) if fr.fn_ptr != 0 => Some(self.resolve_fn(&Value::Obj(fr.callee()), site, 0)),
            Some(fr) => fr.extra.as_ref().and_then(|x| x.script.clone()).map(|sf| {
                let pos = if site == NO_SITE || site & SITE_PC != 0 {
                    NO_POS
                } else {
                    site
                };
                let (file, line, _) = self.sources.borrow_mut().describe(&sf.src, pos);
                Resolved {
                    func: Value::Undefined,
                    name: None,
                    file,
                    line,
                    flags: F_PSEUDO,
                    wrapper: false,
                    strict: false,
                }
            }),
            None => None,
        };
        let text = match caller {
            Some(r) => {
                let name = if r.wrapper {
                    "Object.<anonymous>".to_string()
                } else {
                    r.name.clone().unwrap_or_else(|| "<anonymous>".into())
                };
                format!("eval at {name} ({})", r.location())
            }
            None => "eval at <anonymous>".to_string(),
        };
        Value::from_string(text)
    }

    /// Resolve a function frame: `callee` and the site it was at.
    fn resolve_fn(&self, callee: &Value, site: u32, flags: u32) -> Resolved {
        let mut r = Resolved {
            func: callee.clone(),
            name: None,
            file: Value::str("<anonymous>"),
            line: None,
            flags,
            wrapper: false,
            strict: false,
        };
        let Value::Obj(g) = callee else { return r };
        let func = {
            let b = g.borrow();
            r.name = match b
                .props
                .get("name")
                .filter(|p| !p.accessor())
                .map(|p| p.value())
            {
                Some(Value::Str(s)) if !s.is_empty() => Some(s.to_string()),
                _ => None,
            };
            match &b.call {
                Callable::User(u) => u.func.clone(),
                _ => return r,
            }
        };
        r.strict = func.is_strict;
        let Some(src) = source_of(&func) else {
            return r;
        };
        let pos = if site == NO_SITE {
            NO_POS
        } else if site & SITE_PC != 0 {
            match func.code.get() {
                Some(Some(chunk)) => chunk.call_site_pos((site & !SITE_PC) as usize),
                _ => NO_POS,
            }
        } else {
            site
        };
        let (file, line, wrapper) = self.sources.borrow_mut().describe(&src, pos);
        r.file = file;
        r.line = line;
        r.wrapper = wrapper != 0 && wrapper == Gc::as_ptr(g) as usize;
        r
    }

    fn resolve_raw(&self, raw: &Value) -> Vec<Resolved> {
        let Value::Obj(arr) = raw else {
            return Vec::new();
        };
        let items = self.dense_array_values(arr).unwrap_or_default();
        items
            .as_chunks::<SLOTS>().0.iter()
            .map(|f| {
                let flags = num(&f[3]) as u32;
                if flags & F_PSEUDO != 0 {
                    let line = num(&f[1]);
                    Resolved {
                        func: Value::Undefined,
                        name: None,
                        file: f[4].clone(),
                        line: (line >= 0.0).then(|| (line as u32, num(&f[2]) as u32)),
                        flags,
                        wrapper: false,
                        strict: false,
                    }
                } else {
                    self.resolve_fn(&f[0], num(&f[1]) as u32, flags)
                }
            })
            .collect()
    }

    /// V8's ErrorUtils::ToString: the `Name: message` header of a stack.
    fn stack_header(&mut self, err: &Value) -> Result<String, Value> {
        let ab = |r: Result<Value, super::Abrupt>| r.map_err(super::abrupt_value);
        let name = match ab(self.get_member(err, "name"))? {
            Value::Undefined => "Error".to_string(),
            v => self.to_string(&v).map_err(super::abrupt_value)?.to_string(),
        };
        let msg = match ab(self.get_member(err, "message"))? {
            Value::Undefined => String::new(),
            v => self.to_string(&v).map_err(super::abrupt_value)?.to_string(),
        };
        Ok(if msg.is_empty() {
            name
        } else if name.is_empty() {
            msg
        } else {
            format!("{name}: {msg}")
        })
    }

    /// Format the raw trace of `err` (see the module docs).
    fn format_trace(&mut self, err: &Value, raw: &Value) -> Result<Value, Value> {
        let frames = self.resolve_raw(raw);
        if !IN_PREPARE.get() {
            if let Some(ctor) = self.extra_protos.get("%Error%").cloned() {
                let ctor = Value::Obj(ctor);
                let prep = self
                    .get_member(&ctor, "prepareStackTrace")
                    .map_err(super::abrupt_value)?;
                if prep.is_callable() {
                    let sites: Vec<Value> = frames.iter().map(|r| self.callsite(r)).collect();
                    let arr = self.make_array(sites);
                    IN_PREPARE.set(true);
                    let r = self.call(prep, ctor, &[err.clone(), arr]);
                    IN_PREPARE.set(false);
                    return r.map_err(super::abrupt_value);
                }
            }
        }
        let mut s = self.stack_header(err)?;
        for r in &frames {
            s.push_str("\n    at ");
            s.push_str(&r.text());
        }
        Ok(Value::from_string(s))
    }

    /// The `stack` of object `o` (`this` of the accessor): the cached value, else its raw
    /// trace formatted now and cached; `undefined` without one.
    pub(crate) fn read_stack(&mut self, o: &Gc, this: &Value) -> Result<Value, Value> {
        let (raw, own_raw) = {
            let b = o.borrow();
            if let Some(p) = b.props.get(STACK_KEY) {
                return Ok(p.value());
            }
            match b.props.get(RAW_KEY) {
                Some(p) => (p.value(), true),
                None if b.exotic == Exotic::Error => {
                    (b.exotic_payload().unwrap_or(Value::Undefined), false)
                }
                None => return Ok(Value::Undefined),
            }
        };
        if !matches!(raw, Value::Obj(_)) {
            return Ok(Value::Undefined);
        }
        let v = self.format_trace(this, &raw)?;
        let mut b = o.borrow_mut();
        b.props
            .insert(STACK_KEY, Property::data(v.clone(), false, false, false));
        // The raw frames hold their functions alive: let them go.
        if own_raw {
            b.props.remove(RAW_KEY);
        } else {
            b.set_exotic(Exotic::Error, Some(Value::Undefined));
        }
        Ok(v)
    }

    /// `Error.captureStackTrace(target, fn)`.
    pub(crate) fn capture_stack_trace(
        &mut self,
        target: &Value,
        until: &Value,
    ) -> Result<(), Value> {
        let Value::Obj(o) = target else {
            return Err(self.make_error("TypeError", "Invalid argument"));
        };
        let skip = match until {
            Value::Obj(f) if f.borrow().call.is_fn() => Some((Gc::as_ptr(f) as usize, true)),
            _ => None,
        };
        let raw = self.capture_trace(skip);
        let (get, set) = match (
            self.extra_protos.get("%StackGetter%"),
            self.extra_protos.get("%StackSetter%"),
        ) {
            (Some(g), Some(s)) => (g.clone(), s.clone()),
            _ => return Ok(()),
        };
        let mut b = o.borrow_mut();
        match b.props.get("stack") {
            Some(p) if !p.configurable() => {
                drop(b);
                return Err(self.make_error("TypeError", "Cannot redefine property: stack"));
            }
            None if !b.extensible => {
                drop(b);
                return Err(self.make_error(
                    "TypeError",
                    "Cannot define property stack, object is not extensible",
                ));
            }
            _ => {}
        }
        b.props.remove(STACK_KEY);
        b.props
            .insert(RAW_KEY, Property::data(raw, false, false, false));
        b.props.insert(
            "stack",
            Property::accessor_prop(Some(Value::Obj(get)), Some(Value::Obj(set)), false, true),
        );
        Ok(())
    }

    /// A CallSite object for one frame (`Error.prepareStackTrace`'s second argument).
    fn callsite(&mut self, r: &Resolved) -> Value {
        let proto = match self.extra_protos.get("%CallSite%") {
            Some(p) => p.clone(),
            None => {
                let p = self.callsite_proto();
                self.extra_protos.insert("%CallSite%", p.clone());
                p
            }
        };
        let o = Object::new(Some(proto));
        let (line, col) = match r.line {
            Some((l, c)) => (Value::Num(l as f64), Value::Num(c as f64)),
            None => (Value::Null, Value::Null),
        };
        let pseudo = r.flags & F_PSEUDO != 0;
        let eval = r.flags & F_EVAL != 0;
        let data = vec![
            // 0 function (hidden for strict code, as V8)
            if r.strict {
                Value::Undefined
            } else {
                r.func.clone()
            },
            // 1 file name
            if eval {
                Value::Undefined
            } else {
                r.file.clone()
            },
            line,
            col,
            // 4 function name
            match &r.name {
                Some(n) if !r.wrapper && !pseudo => Value::from_string(n.clone()),
                _ => Value::Null,
            },
            // 5 flags: 1 constructor, 2 top-level, 4 eval, 8 CommonJS wrapper
            Value::Num(
                (r.flags & F_CONSTRUCT
                    | if pseudo { 2 } else { 0 }
                    | if eval { 4 } else { 0 }
                    | if r.wrapper { 8 } else { 0 }) as f64,
            ),
            // 6 eval origin
            if eval {
                r.file.clone()
            } else {
                Value::Undefined
            },
            // 7 toString
            Value::from_string(r.text()),
        ];
        let arr = Object::new_array_from_vec(None, data);
        o.borrow_mut()
            .props
            .insert(CS_KEY, Property::data(Value::Obj(arr), false, false, false));
        Value::Obj(o)
    }

    fn callsite_proto(&mut self) -> Gc {
        let p = self.new_object();
        fn data(i: &mut Interp, this: &Value, k: usize) -> Value {
            let Value::Obj(o) = this else {
                return Value::Undefined;
            };
            let arr = match o.borrow().props.get(CS_KEY) {
                Some(p) => p.value(),
                None => return Value::Undefined,
            };
            match &arr {
                Value::Obj(a) => i
                    .dense_array_values(a)
                    .and_then(|v| v.get(k).cloned())
                    .unwrap_or(Value::Undefined),
                _ => Value::Undefined,
            }
        }
        fn flag(i: &mut Interp, this: &Value, bit: u32) -> bool {
            (num(&data(i, this, 5)) as u32) & bit != 0
        }
        self.def_method(&p, "getThis", 0, |_, _, _| Ok(Value::Undefined));
        self.def_method(&p, "getTypeName", 0, |i, t, _| {
            Ok(if flag(i, &t, 8) {
                Value::str("Object")
            } else {
                Value::Null
            })
        });
        self.def_method(&p, "getFunction", 0, |i, t, _| Ok(data(i, &t, 0)));
        self.def_method(&p, "getFunctionName", 0, |i, t, _| Ok(data(i, &t, 4)));
        self.def_method(&p, "getMethodName", 0, |_, _, _| Ok(Value::Null));
        self.def_method(&p, "getFileName", 0, |i, t, _| Ok(data(i, &t, 1)));
        self.def_method(&p, "getScriptNameOrSourceURL", 0, |i, t, _| {
            Ok(data(i, &t, 1))
        });
        self.def_method(&p, "getScriptHash", 0, |_, _, _| Ok(Value::str("")));
        self.def_method(&p, "getLineNumber", 0, |i, t, _| Ok(data(i, &t, 2)));
        self.def_method(&p, "getColumnNumber", 0, |i, t, _| Ok(data(i, &t, 3)));
        self.def_method(&p, "getEnclosingLineNumber", 0, |_, _, _| Ok(Value::Null));
        self.def_method(&p, "getEnclosingColumnNumber", 0, |_, _, _| Ok(Value::Null));
        self.def_method(&p, "getPosition", 0, |_, _, _| Ok(Value::Num(0.0)));
        self.def_method(&p, "getPromiseIndex", 0, |_, _, _| Ok(Value::Null));
        self.def_method(&p, "getEvalOrigin", 0, |i, t, _| {
            Ok(match data(i, &t, 6) {
                Value::Undefined => data(i, &t, 1),
                v => v,
            })
        });
        self.def_method(&p, "isToplevel", 0, |i, t, _| {
            Ok(Value::Bool(flag(i, &t, 2)))
        });
        self.def_method(&p, "isEval", 0, |i, t, _| Ok(Value::Bool(flag(i, &t, 4))));
        self.def_method(&p, "isNative", 0, |_, _, _| Ok(Value::Bool(false)));
        self.def_method(&p, "isConstructor", 0, |i, t, _| {
            Ok(Value::Bool(flag(i, &t, 1)))
        });
        self.def_method(&p, "isAsync", 0, |_, _, _| Ok(Value::Bool(false)));
        self.def_method(&p, "isPromiseAll", 0, |_, _, _| Ok(Value::Bool(false)));
        self.def_method(&p, "toString", 0, |i, t, _| Ok(data(i, &t, 7)));
        p
    }

    /// The source text the innermost frame's call-site positions are offsets into: a function
    /// frame's source, or a script/eval pseudo frame's.
    pub(crate) fn innermost_source(&self) -> Option<Rc<str>> {
        let fr = self.fn_frames.last()?;
        if fr.fn_ptr == 0 {
            return fr.extra.as_ref()?.script.as_ref().map(|sf| sf.src.clone());
        }
        let callee = fr.callee();
        let b = callee.borrow();
        match &b.call {
            Callable::User(u) => source_of(&u.func),
            _ => None,
        }
    }
}

/// The tree-walker's description of a call's callee in its "… is not a function" TypeError
/// (`eval.rs` `describe_callee`: a name, `(intermediate value).name` for a property, else
/// `expression`), read back from `src` at the call's recorded position `pos` — the callee's
/// last identifier token when there is one, else the `(`. `None` when the text there is not
/// understood (the caller keeps its generic message).
pub(crate) fn describe_callee_text(src: &str, pos: u32) -> Option<String> {
    fn id_char(c: char) -> bool {
        c.is_alphanumeric() || matches!(c, '_' | '$' | '\u{200c}' | '\u{200d}')
    }
    // The identifier ending `text` (with what precedes it), as the tree-walker names it.
    fn named(text: &str) -> String {
        let start = text
            .char_indices()
            .rev()
            .take_while(|&(_, c)| id_char(c) || c == '#')
            .last()
            .map_or(text.len(), |(k, _)| k);
        let name = &text[start..];
        if name.is_empty()
            || name.starts_with(|c: char| c.is_ascii_digit())
            || matches!(name, "this" | "null" | "true" | "false")
        {
            return "expression".to_string();
        }
        if text[..start].trim_end().ends_with('.') {
            format!("(intermediate value).{name}")
        } else {
            name.to_string()
        }
    }
    let p = pos as usize;
    if pos == NO_POS || p >= src.len() || !src.is_char_boundary(p) {
        return None;
    }
    let rest = &src[p..];
    if rest.starts_with('(') {
        // No identifier right before the `(`: `f?.(…)` names `f`, a parenthesized reference
        // `(f)(…)` / `(a.f)(…)` its name, anything else (`a[0]()`, `f()()`, `(0, f)()`) is an
        // expression.
        let before = src[..p].trim_end();
        if let Some(b) = before.strip_suffix("?.") {
            return Some(named(b.trim_end()));
        }
        if let Some(inner) = before.strip_suffix(')') {
            let inner = inner.trim_end();
            let chain = inner
                .char_indices()
                .rev()
                .take_while(|&(_, c)| id_char(c) || matches!(c, '.' | '#' | ' ' | '\t'))
                .last()
                .map_or(inner.len(), |(k, _)| k);
            let outer = inner[..chain].trim_end();
            let grouping = outer.strip_suffix('(').map(str::trim_end).is_some_and(|o| {
                // A `(` right after a name, `)` or `]` is a call's (`g(x)()`), unless the name
                // is an operator keyword (`return (f)()`).
                let word_at = o
                    .char_indices()
                    .rev()
                    .take_while(|&(_, c)| id_char(c))
                    .last()
                    .map_or(o.len(), |(k, _)| k);
                let word = &o[word_at..];
                !(o.ends_with([')', ']']) || !word.is_empty())
                    || matches!(
                        word,
                        "return"
                            | "typeof"
                            | "void"
                            | "await"
                            | "case"
                            | "in"
                            | "of"
                            | "delete"
                            | "throw"
                            | "yield"
                            | "else"
                            | "do"
                            | "instanceof"
                    )
            });
            let text = inner[chain..].trim();
            if grouping && !text.is_empty() && !text.starts_with(['.', '#']) && !text.ends_with('.')
            {
                return Some(named(inner));
            }
        }
        return Some("expression".to_string());
    }
    let len = rest
        .char_indices()
        .find(|&(k, c)| !(id_char(c) || (k == 0 && c == '#')))
        .map_or(rest.len(), |(k, _)| k);
    // The token must be the whole callee name, right before the arguments.
    if len == 0 || !rest[len..].trim_start().starts_with('(') {
        return None;
    }
    Some(named(&src[..p + len]))
}

#[cfg(test)]
mod tests {
    use super::LineTable;
    use crate::{bytecode::Tier, Completion, Engine};

    fn run(src: &str, tier: Tier) -> String {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        match engine.eval(src, false).unwrap() {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
    }

    #[test]
    fn frames_carry_call_positions_on_every_tier() {
        let src = "function leaf() { return new Error('x').stack; }\n\
                   function mid() { return leaf(); }\n\
                   class K { constructor() { this.s = mid(); } }\n\
                   let s; for (let i = 0; i < 50; i++) s = new K().s;\n\
                   s";
        let want = "Error: x\n    at leaf (<anonymous>:1:26)\n    at mid (<anonymous>:2:25)\n    \
                    at new K (<anonymous>:3:36)\n    at <anonymous>:4:41";
        for tier in [Tier::Interp, Tier::Bytecode] {
            assert_eq!(run(src, tier), want, "{tier:?}");
        }
    }

    /// A builtin that calls back into JS has V8's `at <Type>.<name> (<anonymous>)` frame, named
    /// by its receiver; call adaptors have none.
    #[test]
    fn builtins_calling_back_have_frames() {
        let src = r#"
            // The frame that called the caller of `up`.
            function up() { return new Error('x').stack.split('\n')[3].trim().split(' (')[0]; }
            function st() { return up(); }
            class A extends Array {}
            [[1].map(st)[0], Array.prototype.map.call('a', st)[0], A.from([1]).map(st)[0],
             Array.from([1], st)[0],
             (() => { let r; new Promise(function () { r = up(); }); return r; })(),
             (() => { let r; Math.max({ valueOf() { r = up(); return 1; } }); return r; })(),
             st.call(null).startsWith('at <anonymous>')].join('|')
        "#;
        let want = "at Array.map|at String.map|at A.map|at Array.from|at new Promise|\
                    at Math.max|true";
        for tier in [Tier::Interp, Tier::Bytecode] {
            assert_eq!(run(src, tier), want, "{tier:?}");
        }
    }

    #[test]
    fn capture_stack_trace_limit_and_prepare_stack_trace() {
        let src = r#"
            function a() { return b(); }
            function b() { const o = {}; Error.captureStackTrace(o, a); return o.stack; }
            const out = [a().split("\n").length];
            const o = { message: "m" }; Error.captureStackTrace(o);
            out.push(o.stack.split("\n")[0], typeof Object.getOwnPropertyDescriptor(o, "stack").get);
            Error.stackTraceLimit = 0; out.push(new Error("z").stack);
            Error.stackTraceLimit = "x"; out.push(String(new Error("z").stack));
            Error.stackTraceLimit = 10;
            class E extends Error {}
            function mk() { return new E("e"); }
            out.push(mk().stack.split("\n")[1].trim().split(" ")[1]);
            Error.prepareStackTrace = (e, cs) => cs.map(c => [c.getFunctionName(), c.getLineNumber(),
                c.getColumnNumber(), c.isToplevel(), c.toString()].join(":")).join("|");
            function f() { return new Error("p").stack; }
            out.push(f());
            Error.prepareStackTrace = () => { throw 7; };
            try { new Error("t").stack; } catch (e) { out.push("threw " + e); }
            Error.prepareStackTrace = undefined;
            const e = new Error("h"); e.message = "late"; out.push(e.stack.split("\n")[0]);
            out.join(";")
        "#;
        let want = "2;Error: m;function;Error: z;undefined;mk;\
                    f:15:35:false:f (<anonymous>:15:35)|:16:22:true:<anonymous>:16:22;\
                    threw 7;Error: late";
        for tier in [Tier::Interp, Tier::Bytecode] {
            assert_eq!(run(src, tier), want, "{tier:?}");
        }
    }

    #[test]
    fn line_table_counts_every_terminator_and_utf16_columns() {
        let src = "a\nb\r\nc\rd\u{2028}e\u{2029}f é😀x";
        let t = LineTable::build(src);
        let at = |needle: &str| src.find(needle).unwrap() as u32;
        assert_eq!(t.line_col(at("a")), Some((1, 0)));
        assert_eq!(t.line_col(at("b")), Some((2, 0)));
        assert_eq!(t.line_col(at("c")), Some((3, 0)));
        assert_eq!(t.line_col(at("d")), Some((4, 0)));
        assert_eq!(t.line_col(at("e")), Some((5, 0)));
        // `f é😀x`: é is one UTF-16 unit, 😀 two.
        assert_eq!(t.line_col(at("x")), Some((6, 5)));
        let back = LineTable::decode(&t.encode()).unwrap();
        assert_eq!(back.line_col(at("x")), Some((6, 5)));
        assert_eq!(back.line_col(at("d")), Some((4, 0)));
        assert_eq!(t.line_col(src.len() as u32 + 1), None);
    }
}
