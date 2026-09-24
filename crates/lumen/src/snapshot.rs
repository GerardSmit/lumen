//! A compact binary (de)serializer for the parse AST, so the runtime's static JS glue can be
//! parsed once at build time and *decoded* (much cheaper than lex+parse) on every boot.
//!
//! Design points:
//! - **Optimization, not source of truth.** [`decode`] can fail (version skew, truncation); the
//!   caller falls back to re-parsing the original source, so a codec bug can never miscompile —
//!   at worst it costs a parse. A `MAGIC`+`VERSION` header makes skew a clean decode error.
//! - **Only parser output is encoded.** `Function`'s `scan`/`hoist`/`calls`/`code` are lazy
//!   runtime caches (`Cell`/`OnceCell`); decode initializes them empty, exactly as the parser
//!   leaves them, so a decoded tree is indistinguishable from a freshly parsed one.
//!   (An ahead-of-time blob may fill `code` right after decode, from its bytecode section:
//!   see [`decode_with_functions`] and `crate::precompiled`.)
//! - **Lazy bodies stay lazy.** A function whose body the parser skipped (see
//!   [`Function::ensure_body`]) is written as its [`LazyBody`] — a byte range into the source
//!   plus the parse context — and no statements; the decoder needs the same source text, which
//!   the glue crates already carry as a `&'static str`, and hands one shared `Rc<str>` of it to
//!   every decoded range. So a decoded glue function costs its name, params and a few integers
//!   until it is first called, exactly like one parsed from source. A source-length + hash in
//!   the header makes a mismatched source a clean decode error.
//! - Interned `&'static str` operators are re-interned from [`KEYWORDS`]/[`PUNCTUATORS`] on
//!   decode (every op the parser emits comes from those tables).
//!
//! The format is deliberately dumb — a preorder walk with a `u8` tag per enum, LEB128 lengths,
//! little-endian `f64`. It is an internal build/runtime contract, never persisted across builds.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::ast::*;
use crate::bigint::JsBigInt;
use crate::token::{KEYWORDS, PUNCTUATORS};

const MAGIC: u32 = 0x4c_53_4e_31; // "LSN1"
/// The split ahead-of-time form (see [`encode_split`]).
const MAGIC_SPLIT: u32 = 0x4c53_4e32; // "LSN2"
/// Bump on any AST or format change. A mismatch makes `decode` fail → caller re-parses.
pub(crate) const VERSION: u32 = 5;

/// A call/`new` source position (stack traces): LEB128 of `pos + 1`, 0 = none (`NO_POS`).
fn enc_pos(w: &mut Writer, pos: u32) {
    w.uv(pos.wrapping_add(1) as u64);
}

fn dec_pos(r: &mut Reader) -> R<u32> {
    Ok((r.uv()? as u32).wrapping_sub(1))
}

/// FNV-1a over the source, eight bytes at a time: the header's check that the source handed to
/// [`decode`] is the one the ranges were recorded against.
fn source_hash(src: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let (chunks, rest) = src.as_bytes().as_chunks::<8>();
    for c in chunks {
        h = (h ^ u64::from_le_bytes(*c)).wrapping_mul(0x0100_0000_01b3);
    }
    for &b in rest {
        h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
    }
    h
}

// ---- writer / reader --------------------------------------------------------------------------

struct Writer {
    buf: Vec<u8>,
    /// Private-name scopes already written, by identity: a later reference is an index into the
    /// order they were first defined in, which the reader rebuilds.
    scopes: HashMap<*const PrivateScope, u32>,
    /// Ahead-of-time mode (see [`encode_stripped`]): no source text or source ranges are
    /// written — every `FnSource` becomes `None` and every function carries its statements.
    strip: bool,
    /// When collecting (see [`encode_stripped_with_functions`]): every function in the order
    /// it is entered — its *function index*, which the decoder reproduces.
    funcs: Option<Vec<Rc<Function>>>,
    /// Stripped mode with kept function text (see [`encode_stripped_keep`]): the first pass
    /// records every function/class source range here ...
    ranges: Option<Vec<(u32, u32)>>,
    /// ... and the second pass writes those ranges remapped into the kept text.
    keep: Option<KeepMap>,
    /// When collecting: the string-literal specifiers of `import("x")` and `require("x")`.
    deps: Option<Deps>,
    /// The unit's source, to tell its ranges from those into another text (see
    /// `enc_fnsource`), with a per-`Rc` verdict cache. `None`: every range is the unit's.
    unit_src: Option<(Rc<str>, HashMap<*const u8, bool>)>,
    /// Split mode (see [`encode_split`]): each function is written where it occurs as its
    /// local index, and its header and body go to buffers of their own.
    split: Option<Split>,
}

/// The per-function output of split mode, by function index, plus the stream being written.
/// A *stream* is where a function reference can occur: 0 = the unit's top level, `2i + 1` =
/// the body of function `i`, `2i + 2` = the header (parameter list) of function `i`. A
/// reference is the function's rank among the functions of its stream (its *local index*).
#[derive(Default)]
struct Split {
    headers: Vec<Vec<u8>>,
    bodies: Vec<Vec<u8>>,
    /// Each function's stream (where it is referenced from) and the end of its descendants'
    /// index range (its own index + 1 + its descendant count).
    owners: Vec<u64>,
    ends: Vec<u32>,
    cur: u64,
    next_local: HashMap<u64, u64>,
}

impl Writer {
    fn is_unit_src(&mut self, src: &Rc<str>) -> bool {
        let Some((unit, seen)) = &mut self.unit_src else {
            return true;
        };
        *seen
            .entry(src.as_ptr())
            .or_insert_with(|| Rc::ptr_eq(unit, src) || **unit == **src)
    }
}

/// The string-literal module references found in an encoded body, beyond its static imports:
/// dynamic `import("x")` (no import attributes) and CommonJS `require("x")` calls.
#[derive(Default, Debug, Clone)]
pub struct Deps {
    pub dynamic_imports: Vec<String>,
    pub requires: Vec<String>,
}

/// The kept-text layout: the maximal source intervals (original coordinates) that were kept,
/// each with its offset in the kept text.
struct KeepMap {
    /// (orig_start, orig_end, kept_offset), sorted, disjoint.
    spans: Vec<(u32, u32, u32)>,
}

impl KeepMap {
    /// Build from every recorded range (nested/overlapping ones collapse into their enclosing
    /// span). `skip` is a range that is not kept (a synthesized wrapper around the whole unit).
    fn build(src: &str, ranges: &[(u32, u32)], skip: Option<(u32, u32)>) -> (KeepMap, String) {
        let mut rs: Vec<(u32, u32)> = ranges
            .iter()
            .copied()
            .filter(|r| Some(*r) != skip && r.0 < r.1 && (r.1 as usize) <= src.len())
            .collect();
        rs.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::new();
        for (s, e) in rs {
            match merged.last_mut() {
                Some(last) if s < last.1 => last.1 = last.1.max(e),
                _ => merged.push((s, e)),
            }
        }
        let mut text = String::new();
        let mut spans = Vec::with_capacity(merged.len());
        for (s, e) in merged {
            let Some(slice) = src.get(s as usize..e as usize) else {
                continue;
            };
            spans.push((s, e, text.len() as u32));
            text.push_str(slice);
        }
        (KeepMap { spans }, text)
    }

    /// `start..end` in kept-text coordinates (`None`: not inside one kept span).
    fn map(&self, start: u32, end: u32) -> Option<(u32, u32)> {
        let i = self.spans.partition_point(|s| s.0 <= start).checked_sub(1)?;
        let (s, e, off) = self.spans[i];
        (end <= e).then(|| (off + (start - s), off + (end - s)))
    }
}

impl Writer {
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    /// LEB128 unsigned varint (small counts/tags stay one byte).
    fn uv(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(byte);
                break;
            }
            self.buf.push(byte | 0x80);
        }
    }
    fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }
    fn str(&mut self, s: &str) {
        self.uv(s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    /// The source every decoded range points into.
    src: Rc<str>,
    /// Private-name scopes decoded so far, in definition order (see `Writer::scopes`).
    scopes: Vec<Rc<PrivateScope>>,
    /// When collecting: every decoded function by function index (a slot is reserved when the
    /// function is entered, filled once it is built — the writer's preorder numbering).
    funcs: Option<Vec<Option<Rc<Function>>>>,
    /// Split form: the functions of the stream being decoded, by local index (a function
    /// reference is an index into this), and the unit's kept text for `toString` ranges.
    split: Option<SplitRead>,
}

struct SplitRead {
    /// The function indices of the stream being read, by local index.
    children: Vec<usize>,
    unit: Rc<SplitUnit>,
    kept: Option<Rc<crate::precompiled::KeptRef>>,
}

type R<T> = Result<T, String>;

impl Reader<'_> {
    fn u8(&mut self) -> R<u8> {
        let b = *self.buf.get(self.pos).ok_or("snapshot: truncated")?;
        self.pos += 1;
        Ok(b)
    }
    fn uv(&mut self) -> R<u64> {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            let byte = self.u8()?;
            result |= ((byte & 0x7f) as u64) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
            if shift >= 64 {
                return Err("snapshot: varint overflow".into());
            }
        }
    }
    fn f64(&mut self) -> R<f64> {
        let end = self.pos + 8;
        let bytes = self
            .buf
            .get(self.pos..end)
            .ok_or("snapshot: truncated f64")?;
        self.pos = end;
        Ok(f64::from_le_bytes(bytes.try_into().unwrap()))
    }
    fn bool(&mut self) -> R<bool> {
        Ok(self.u8()? != 0)
    }
    fn str(&mut self) -> R<String> {
        let len = self.uv()? as usize;
        let end = self.pos + len;
        let bytes = self
            .buf
            .get(self.pos..end)
            .ok_or("snapshot: truncated str")?;
        self.pos = end;
        String::from_utf8(bytes.to_vec()).map_err(|_| "snapshot: bad utf8".into())
    }
    fn rcstr(&mut self) -> R<Rc<str>> {
        Ok(Rc::from(self.str()?.as_str()))
    }
    fn u64(&mut self) -> R<u64> {
        let end = self.pos + 8;
        let bytes = self
            .buf
            .get(self.pos..end)
            .ok_or("snapshot: truncated u64")?;
        self.pos = end;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }
    /// A byte range into `src`, checked to lie on char boundaries so a slice can never panic.
    fn range(&mut self) -> R<(u32, u32)> {
        let start = self.uv()?;
        let end = self.uv()?;
        let ok = start <= end
            && end <= self.src.len() as u64
            && self.src.is_char_boundary(start as usize)
            && self.src.is_char_boundary(end as usize);
        if !ok {
            return Err("snapshot: source range out of bounds".into());
        }
        Ok((start as u32, end as u32))
    }
}

/// Re-intern an operator string to the `&'static str` the parser would have used.
fn intern_op(s: &str) -> R<&'static str> {
    KEYWORDS
        .iter()
        .chain(PUNCTUATORS.iter())
        .find(|k| **k == s)
        .copied()
        .ok_or_else(|| format!("snapshot: unknown operator {s:?}"))
}

// ---- public API -------------------------------------------------------------------------------

/// Encode a script body parsed from `src` to a snapshot blob. Function source ranges and lazy
/// bodies are byte offsets into `src`, which [`decode`] must be handed again.
pub fn encode(body: &[Stmt], src: &str) -> Vec<u8> {
    let mut w = Writer {
        buf: Vec::with_capacity(body.len() * 32),
        scopes: HashMap::new(),
        strip: false,
        funcs: None,
        ranges: None,
        keep: None,
        deps: None,
        unit_src: Some((Rc::from(src), HashMap::new())),
        split: None,
    };
    w.uv(MAGIC as u64);
    w.uv(VERSION as u64);
    w.uv(src.len() as u64);
    w.u64(source_hash(src));
    enc_stmts(&mut w, body);
    w.buf
}

/// Encode `body` with no trace of its source text — the ahead-of-time (`lumen::precompile`)
/// form. Every function's `toString` text is dropped (it renders as a NativeFunction
/// placeholder) and every function body is written as statements, never as a lazy byte range,
/// so the blob decodes against the empty source (`decode(bytes, "")`). `body` should come from
/// an eager parse; a lazy body left in it is materialised here.
#[allow(dead_code)] // the AST-only form; blobs use `encode_stripped_with_functions`
pub fn encode_stripped(body: &[Stmt]) -> Vec<u8> {
    encode_stripped_inner(body, false).0
}

/// [`encode_stripped`], also returning every function of `body` by *function index*: the
/// order the encoder enters them (preorder — a function before the functions in its params and
/// body). [`decode_with_functions`] numbers the decoded tree identically, which is how the
/// ahead-of-time bytecode section ([`crate::precompiled`]) keys its chunks.
#[allow(dead_code)] // tests; blobs use `encode_stripped_keep`
pub(crate) fn encode_stripped_with_functions(body: &[Stmt]) -> (Vec<u8>, Vec<Rc<Function>>) {
    let (buf, funcs) = encode_stripped_inner(body, true);
    (buf, funcs.unwrap_or_default())
}

fn encode_stripped_inner(body: &[Stmt], collect: bool) -> (Vec<u8>, Option<Vec<Rc<Function>>>) {
    let mut w = Writer {
        buf: Vec::with_capacity(body.len() * 32),
        scopes: HashMap::new(),
        strip: true,
        funcs: collect.then(Vec::new),
        ranges: None,
        keep: None,
        deps: None,
        unit_src: None,
        split: None,
    };
    w.uv(MAGIC as u64);
    w.uv(VERSION as u64);
    w.uv(0);
    w.u64(source_hash(""));
    enc_stmts(&mut w, body);
    (w.buf, w.funcs)
}

/// What [`encode_stripped_keep`] produces.
pub(crate) struct StrippedUnit {
    pub ast: Vec<u8>,
    /// Split form only: every function body, concatenated in function-index order.
    pub bodies: Vec<u8>,
    pub funcs: Vec<Rc<Function>>,
    /// The kept function text the AST's source ranges point into (empty without `keep`). The
    /// AST decodes against exactly this string: `decode_with_functions(&ast, &kept)`.
    pub kept: String,
    pub deps: Deps,
}

/// [`encode_stripped_with_functions`], optionally keeping the exact source text of every
/// function and class (`keep = Some((src, skip))`, `src` the text `body` was parsed from): the
/// maximal function/class slices of `src` are concatenated into [`StrippedUnit::kept`] — text
/// between top-level functions (comments, module-level statements) is not — and each
/// function's `toString` range is remapped into it. `skip` excludes one range (a synthesized
/// wrapper spanning the whole unit, whose own text must not be kept). Also collects the unit's
/// literal `import("x")` / `require("x")` specifiers.
#[allow(dead_code)] // tests; blobs use `encode_split`
pub(crate) fn encode_stripped_keep(
    body: &[Stmt],
    keep: Option<(&str, Option<(u32, u32)>)>,
) -> StrippedUnit {
    encode_stripped_inner_keep(body, keep, false)
}

fn encode_stripped_inner_keep(
    body: &[Stmt],
    keep: Option<(&str, Option<(u32, u32)>)>,
    split: bool,
) -> StrippedUnit {
    let mut kept = String::new();
    let mut map = None;
    let unit_src: Option<Rc<str>> = keep.map(|(src, _)| Rc::from(src));
    if let Some((src, skip)) = keep {
        // Pass 1: record every range (the buffer is thrown away).
        let mut w = Writer {
            buf: Vec::new(),
            scopes: HashMap::new(),
            strip: true,
            funcs: None,
            ranges: Some(Vec::new()),
            keep: None,
            deps: None,
            unit_src: unit_src.clone().map(|s| (s, HashMap::new())),
            split: None,
        };
        enc_stmts(&mut w, body);
        let (m, text) = KeepMap::build(src, w.ranges.as_deref().unwrap_or(&[]), skip);
        map = Some(m);
        kept = text;
    }
    let mut w = Writer {
        buf: Vec::with_capacity(body.len() * 32),
        scopes: HashMap::new(),
        strip: true,
        funcs: Some(Vec::new()),
        ranges: None,
        keep: map,
        deps: Some(Deps::default()),
        unit_src: unit_src.map(|s| (s, HashMap::new())),
        split: split.then(Split::default),
    };
    if !split {
        w.uv(MAGIC as u64);
        w.uv(VERSION as u64);
        w.uv(kept.len() as u64);
        w.u64(source_hash(&kept));
        enc_stmts(&mut w, body);
        return StrippedUnit {
            ast: w.buf,
            bodies: Vec::new(),
            funcs: w.funcs.unwrap_or_default(),
            kept,
            deps: w.deps.unwrap_or_default(),
        };
    }
    enc_stmts(&mut w, body);
    let top = std::mem::take(&mut w.buf);
    let sp = w.split.take().unwrap_or_default();
    let n = sp.headers.len();
    w.uv(MAGIC_SPLIT as u64);
    w.uv(VERSION as u64);
    w.uv(n as u64);
    let u32le = |w: &mut Writer, v: usize| {
        let v = u32::try_from(v).expect("split unit over 4 GiB");
        w.buf.extend_from_slice(&v.to_le_bytes());
    };
    let (mut header_at, mut body_at) = (0, 0);
    for i in 0..n {
        u32le(&mut w, sp.owners[i] as usize);
        u32le(&mut w, sp.ends[i] as usize);
        u32le(&mut w, header_at);
        u32le(&mut w, body_at);
        header_at += sp.headers[i].len();
        body_at += sp.bodies[i].len();
    }
    u32le(&mut w, header_at);
    u32le(&mut w, body_at);
    for h in &sp.headers {
        w.buf.extend_from_slice(h);
    }
    w.buf.extend_from_slice(&top);
    StrippedUnit {
        ast: w.buf,
        bodies: sp.bodies.concat(),
        funcs: w.funcs.unwrap_or_default(),
        kept,
        deps: w.deps.unwrap_or_default(),
    }
}

/// [`encode_stripped_keep`] in *split* form, the ahead-of-time blob's: [`StrippedUnit::ast`]
/// is what a load decodes at once — the header of every function (name, parameters, flags,
/// `toString` range, body facts) and the unit's top-level statements — and
/// [`StrippedUnit::bodies`] the function bodies, each decoded only when its function first
/// needs it (see [`SplitUnit`]).
///
/// Layout of `ast`: `MAGIC_SPLIT`, `VERSION`, `fn_count` (LEB128), then a fixed-width table
/// of `fn_count` rows of four little-endian `u32`s — the function's owner stream (see
/// `Split`), the end of its descendants' index range, and the offsets of its header and of its
/// body (bodies are concatenated in function-index order) — then the headers' and bodies'
/// total lengths (`u32` each), the headers in function-index order, and the top-level
/// statements. The table lets a load decode a function's header only when something first
/// refers to it (see [`SplitUnit`]).
/// A header is: name, params, flags byte, `toString` source, [`Function::scan_flags`] byte.
pub(crate) fn encode_split(
    body: &[Stmt],
    keep: Option<(&str, Option<(u32, u32)>)>,
) -> StrippedUnit {
    encode_stripped_inner_keep(body, keep, true)
}

/// Decode a snapshot blob back into a script body. `src` is the source it was encoded from; one
/// `Rc<str>` of it is shared by every decoded function's `toString` text and lazy body. `Err`
/// (skew/truncation/corruption/another source) tells the caller to fall back to parsing `src`.
pub fn decode(bytes: &[u8], src: &str) -> R<Vec<Stmt>> {
    let _mem = crate::memstats::enter(crate::memstats::Cat::AstDecode);
    decode_inner(bytes, src, false).map(|(body, _)| body)
}

/// [`decode`], also returning every decoded function by function index (see
/// [`encode_stripped_with_functions`]).
#[allow(dead_code)] // the non-split form; blobs use `SplitUnit`
pub(crate) fn decode_with_functions(bytes: &[u8], src: &str) -> R<(Vec<Stmt>, Vec<Rc<Function>>)> {
    let (body, funcs) = decode_inner(bytes, src, true)?;
    let funcs = funcs
        .unwrap_or_default()
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or("snapshot: unfinished function")?;
    Ok((body, funcs))
}

type Decoded = (Vec<Stmt>, Option<Vec<Option<Rc<Function>>>>);

fn decode_inner(bytes: &[u8], src: &str, collect: bool) -> R<Decoded> {
    let mut r = Reader {
        buf: bytes,
        pos: 0,
        src: Rc::from(""),
        scopes: Vec::new(),
        funcs: collect.then(Vec::new),
        split: None,
    };
    if r.uv()? != MAGIC as u64 {
        return Err("snapshot: bad magic".into());
    }
    if r.uv()? != VERSION as u64 {
        return Err("snapshot: version mismatch".into());
    }
    if r.uv()? != src.len() as u64 || r.u64()? != source_hash(src) {
        return Err("snapshot: source mismatch".into());
    }
    r.src = Rc::from(src);
    let body = dec_stmts(&mut r)?;
    crate::interpreter::stack_trace::note_parsed_source(r.src.clone(), None);
    Ok((body, r.funcs))
}

/// Where a split unit's function bodies are: `(start, len)` within its body store, as bytes.
pub(crate) type BodyBytes = Box<dyn Fn(usize, usize) -> R<std::borrow::Cow<'static, [u8]>>>;

/// Called with each function the unit decodes, by function index, once (see
/// [`SplitUnit::set_hook`]).
pub(crate) type FunctionHook = Box<dyn Fn(usize, &Rc<Function>)>;

/// One [`encode_split`] unit, decoded on demand: a function's header (name, parameters, flags)
/// is decoded when something first refers to it — the unit's top level, the body of the
/// function it is written in, or a precompiled chunk that creates its closure — and its body
/// when it first needs one (see [`crate::precompiled::decode_body`]). A program that never
/// reaches most of a large bundle never pays for most of its function nodes.
pub(crate) struct SplitUnit {
    ast: &'static [u8],
    n: usize,
    /// Byte offsets within `ast`: the per-function table, the headers, the top-level
    /// statements.
    table: usize,
    headers: usize,
    top: usize,
    /// Total length of the unit's bodies.
    bodies_len: usize,
    bodies: BodyBytes,
    /// The unit's source marker (every decoded function shares it: the key of its stack-trace
    /// line table), and its kept function text for `toString` ranges.
    pub(crate) src: Rc<str>,
    kept: Option<Rc<crate::precompiled::KeptRef>>,
    /// Decoded functions by index. Weak: a function lives as long as the AST or closures that
    /// hold it; one that died is decoded again if referred to again.
    funcs: RefCell<Vec<std::rc::Weak<Function>>>,
    hook: RefCell<Option<Rc<dyn Fn(usize, &Rc<Function>)>>>,
}

impl SplitUnit {
    /// Read a split unit's framing. `src` is the unit's (empty) source marker.
    pub(crate) fn new(
        ast: &'static [u8],
        src: Rc<str>,
        kept: Option<Rc<crate::precompiled::KeptRef>>,
        bodies: BodyBytes,
    ) -> R<Rc<SplitUnit>> {
        let mut r = Reader {
            buf: ast,
            pos: 0,
            src: src.clone(),
            scopes: Vec::new(),
            funcs: None,
            split: None,
        };
        if r.uv()? != MAGIC_SPLIT as u64 {
            return Err("snapshot: bad magic".into());
        }
        if r.uv()? != VERSION as u64 {
            return Err("snapshot: version mismatch".into());
        }
        let n = r.uv()? as usize;
        let table = r.pos;
        let headers = n
            .checked_mul(16)
            .and_then(|t| t.checked_add(table + 8))
            .filter(|&h| h <= ast.len())
            .ok_or("snapshot: bad function count")?;
        let u32_at = |at: usize| u32::from_le_bytes(ast[at..at + 4].try_into().unwrap()) as usize;
        let top = headers
            .checked_add(u32_at(headers - 8))
            .filter(|&t| t <= ast.len())
            .ok_or("snapshot: bad header length")?;
        Ok(Rc::new(SplitUnit {
            ast,
            n,
            table,
            headers,
            top,
            bodies_len: u32_at(headers - 4),
            bodies,
            src,
            kept,
            funcs: RefCell::new((0..n).map(|_| std::rc::Weak::new()).collect()),
            hook: RefCell::new(None),
        }))
    }

    /// How many functions the unit has.
    pub(crate) fn fn_count(&self) -> usize {
        self.n
    }

    /// Row `i` of the function table: (owner stream, descendants' end, header offset, body
    /// offset); row `n` holds the totals in the offset columns.
    fn row(&self, i: usize) -> (u64, usize, usize, usize) {
        let u = |at: usize| {
            u32::from_le_bytes(self.ast[at..at + 4].try_into().unwrap()) as usize
        };
        if i == self.n {
            return (0, self.n, u(self.headers - 8), self.bodies_len);
        }
        let at = self.table + 16 * i;
        (u(at) as u64, u(at + 4), u(at + 8), u(at + 12))
    }

    /// The functions of stream `code` among indices `from..to`, in local-index order.
    fn stream(&self, from: usize, to: usize, code: u64) -> R<Vec<usize>> {
        if from > to || to > self.n {
            return Err("snapshot: bad function range".into());
        }
        Ok((from..to).filter(|&j| self.row(j).0 == code).collect())
    }

    /// Install the hook every later-decoded function is passed to (the bytecode attach: see
    /// `bytecode::serialize::attach_unit`), and pass it the functions decoded so far.
    pub(crate) fn set_hook(&self, hook: FunctionHook) {
        let hook: Rc<dyn Fn(usize, &Rc<Function>)> = Rc::from(hook);
        *self.hook.borrow_mut() = Some(hook.clone());
        let live: Vec<(usize, Rc<Function>)> = self
            .funcs
            .borrow()
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.upgrade().map(|f| (i, f)))
            .collect();
        for (i, f) in live {
            hook(i, &f);
        }
    }

    /// Whether function `i` is `f` (decoded and still alive as that node).
    pub(crate) fn is(&self, i: usize, f: *const Function) -> bool {
        self.funcs
            .borrow()
            .get(i)
            .is_some_and(|w| w.strong_count() > 0 && w.as_ptr() == f)
    }

    /// Function `i`, decoding its header if it is not alive.
    pub(crate) fn function(self: &Rc<Self>, i: usize) -> R<Rc<Function>> {
        if let Some(f) = self.funcs.borrow().get(i).and_then(std::rc::Weak::upgrade) {
            return Ok(f);
        }
        if i >= self.n {
            return Err("snapshot: bad function index".into());
        }
        let f = self.decode_header(i)?;
        self.funcs.borrow_mut()[i] = Rc::downgrade(&f);
        let hook = self.hook.borrow().clone();
        if let Some(hook) = hook {
            hook(i, &f);
        }
        Ok(f)
    }

    /// Every function of the unit (tooling and tests; a load decodes on demand).
    pub(crate) fn all_functions(self: &Rc<Self>) -> R<Vec<Rc<Function>>> {
        (0..self.n).map(|i| self.function(i)).collect()
    }

    fn reader<'a>(self: &Rc<Self>, buf: &'a [u8], children: Vec<usize>) -> Reader<'a> {
        Reader {
            buf,
            pos: 0,
            src: self.src.clone(),
            scopes: Vec::new(),
            funcs: None,
            split: Some(SplitRead {
                children,
                unit: self.clone(),
                kept: self.kept.clone(),
            }),
        }
    }

    fn decode_header(self: &Rc<Self>, i: usize) -> R<Rc<Function>> {
        let _mem = crate::memstats::enter(crate::memstats::Cat::AstDecode);
        let (_, end, at, _) = self.row(i);
        let (_, _, next, _) = self.row(i + 1);
        let (start, stop) = (self.headers + at, self.headers + next);
        if end <= i || end > self.n || start > stop || stop > self.top {
            return Err("snapshot: bad function header".into());
        }
        let params_of = self.stream(i + 1, end, 2 * i as u64 + 2)?;
        let mut r = self.reader(&self.ast[start..stop], params_of);
        let name = dec_opt_str(&mut r)?;
        let np = r.uv()? as usize;
        let mut params = Vec::with_capacity(np.min(1 << 16));
        for _ in 0..np {
            params.push(Param {
                pattern: dec_pattern(&mut r)?,
                default: dec_opt_expr(&mut r)?,
                rest: r.bool()?,
            });
        }
        let flags = r.u8()?;
        let source = dec_fnsource(&mut r)?;
        let scan = r.u8()? | crate::ast::SCAN_DONE;
        if r.pos != r.buf.len() {
            return Err("snapshot: trailing data in a function header".into());
        }
        let lazy = LazyBody {
            src: self.src.clone(),
            start: 0,
            end: 0,
            line: 0,
            html_comments: false,
            ctx: LazyCtx {
                strict: flags & 2 != 0,
                in_generator: false,
                in_async: false,
                module: false,
                allow_new_target: false,
                super_prop_ok: false,
                super_call_ok: false,
                in_derived_class: false,
                no_arguments_refs: false,
                in_field_init: false,
            },
            private_scope: None,
            aot: Some(Rc::new(crate::precompiled::AotBody {
                unit: self.clone(),
                idx: i,
            })),
        };
        let f = Rc::new(Function {
            name,
            params,
            body: RefCell::new(None),
            lazy: RefCell::new(Some(Box::new(lazy))),
            lazy_error: OnceCell::new(),
            is_arrow: flags & 1 != 0,
            is_strict: flags & 2 != 0,
            expr_body: flags & 4 != 0,
            is_generator: flags & 8 != 0,
            is_async: flags & 16 != 0,
            is_method: flags & 32 != 0,
            is_fn_expr: flags & 64 != 0,
            source,
            scan: Cell::new(scan),
            hoist: RefCell::new(None),
            body_used: Cell::new(false),
            calls: Cell::new(0),
            code: OnceCell::new(),
            fn_maps: OnceCell::new(),
        });
        // Like a parsed lazy function: the collector releases the body once it goes cold (it
        // decodes again from the blob if the function runs again).
        crate::value::register_lazy_function(&f);
        Ok(f)
    }

    /// The unit's top-level statements.
    pub(crate) fn decode_top(self: &Rc<Self>) -> R<Vec<Stmt>> {
        let _mem = crate::memstats::enter(crate::memstats::Cat::AstDecode);
        let top = self.stream(0, self.n, 0)?;
        let mut r = self.reader(&self.ast[self.top..], top);
        let body = dec_stmts(&mut r)?;
        if r.pos != r.buf.len() {
            return Err("snapshot: trailing data in split unit".into());
        }
        Ok(body)
    }

    /// The deferred body of function `i`.
    pub(crate) fn decode_body(self: &Rc<Self>, i: usize) -> R<Vec<Stmt>> {
        let _mem = crate::memstats::enter(crate::memstats::Cat::AstDecode);
        if i >= self.n {
            return Err("snapshot: bad function index".into());
        }
        let (_, end, _, start) = self.row(i);
        let (_, _, _, stop) = self.row(i + 1);
        if start > stop || stop > self.bodies_len || end <= i || end > self.n {
            return Err("snapshot: bad function body".into());
        }
        let bytes = (self.bodies)(start, stop - start)?;
        let children = self.stream(i + 1, end, 2 * i as u64 + 1)?;
        let mut r = self.reader(&bytes, children);
        let body = dec_stmts(&mut r)?;
        if r.pos != r.buf.len() {
            return Err("snapshot: trailing data in a function body".into());
        }
        Ok(body)
    }
}

// ---- Vec / Option helpers (monomorphized by hand to keep the reader borrow simple) ------------

fn enc_stmts(w: &mut Writer, v: &[Stmt]) {
    w.uv(v.len() as u64);
    for s in v {
        enc_stmt(w, s);
    }
}
fn dec_stmts(r: &mut Reader) -> R<Vec<Stmt>> {
    let n = r.uv()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(dec_stmt(r)?);
    }
    Ok(out)
}

fn enc_exprs(w: &mut Writer, v: &[Expr]) {
    w.uv(v.len() as u64);
    for e in v {
        enc_expr(w, e);
    }
}
fn dec_exprs(r: &mut Reader) -> R<Vec<Expr>> {
    let n = r.uv()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(dec_expr(r)?);
    }
    Ok(out)
}

fn enc_opt_expr(w: &mut Writer, o: &Option<Expr>) {
    match o {
        Some(e) => {
            w.u8(1);
            enc_expr(w, e);
        }
        None => w.u8(0),
    }
}
fn dec_opt_expr(r: &mut Reader) -> R<Option<Expr>> {
    Ok(if r.u8()? == 1 {
        Some(dec_expr(r)?)
    } else {
        None
    })
}

fn enc_opt_str(w: &mut Writer, o: &Option<String>) {
    match o {
        Some(s) => {
            w.u8(1);
            w.str(s);
        }
        None => w.u8(0),
    }
}
fn dec_opt_str(r: &mut Reader) -> R<Option<String>> {
    Ok(if r.u8()? == 1 { Some(r.str()?) } else { None })
}

fn enc_opt_rcstr(w: &mut Writer, o: &Option<Rc<str>>) {
    match o {
        Some(s) => {
            w.u8(1);
            w.str(s);
        }
        None => w.u8(0),
    }
}
fn dec_opt_rcstr(r: &mut Reader) -> R<Option<Rc<str>>> {
    Ok(if r.u8()? == 1 { Some(r.rcstr()?) } else { None })
}

// ---- Stmt -------------------------------------------------------------------------------------

fn enc_stmt(w: &mut Writer, s: &Stmt) {
    match s {
        Stmt::Expr(e) => {
            w.u8(0);
            enc_expr(w, e);
        }
        Stmt::VarDecl { kind, decls } => {
            w.u8(1);
            enc_declkind(w, *kind);
            w.uv(decls.len() as u64);
            for (pat, init) in decls {
                enc_pattern(w, pat);
                enc_opt_expr(w, init);
            }
        }
        Stmt::FuncDecl(f) => {
            w.u8(2);
            enc_function(w, f);
        }
        Stmt::Return(e) => {
            w.u8(3);
            enc_opt_expr(w, e);
        }
        Stmt::If { test, cons, alt } => {
            w.u8(4);
            enc_expr(w, test);
            enc_stmt(w, cons);
            match alt {
                Some(a) => {
                    w.u8(1);
                    enc_stmt(w, a);
                }
                None => w.u8(0),
            }
        }
        Stmt::Block(b) => {
            w.u8(5);
            enc_stmts(w, b);
        }
        Stmt::While { test, body } => {
            w.u8(6);
            enc_expr(w, test);
            enc_stmt(w, body);
        }
        Stmt::DoWhile { body, test } => {
            w.u8(7);
            enc_stmt(w, body);
            enc_expr(w, test);
        }
        Stmt::For {
            init,
            test,
            update,
            body,
        } => {
            w.u8(8);
            match init {
                Some(fi) => {
                    w.u8(1);
                    enc_forinit(w, fi);
                }
                None => w.u8(0),
            }
            enc_opt_expr(w, test);
            enc_opt_expr(w, update);
            enc_stmt(w, body);
        }
        Stmt::ForInOf {
            decl,
            left,
            right,
            of,
            is_await,
            body,
        } => {
            w.u8(9);
            match decl {
                Some(k) => {
                    w.u8(1);
                    enc_declkind(w, *k);
                }
                None => w.u8(0),
            }
            enc_pattern(w, left);
            enc_expr(w, right);
            w.bool(*of);
            w.bool(*is_await);
            enc_stmt(w, body);
        }
        Stmt::Break(l) => {
            w.u8(10);
            enc_opt_str(w, l);
        }
        Stmt::Continue(l) => {
            w.u8(11);
            enc_opt_str(w, l);
        }
        Stmt::Throw(e) => {
            w.u8(12);
            enc_expr(w, e);
        }
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            w.u8(13);
            enc_stmts(w, block);
            match handler {
                Some((param, hbody)) => {
                    w.u8(1);
                    match param {
                        Some(p) => {
                            w.u8(1);
                            enc_pattern(w, p);
                        }
                        None => w.u8(0),
                    }
                    enc_stmts(w, hbody);
                }
                None => w.u8(0),
            }
            match finalizer {
                Some(f) => {
                    w.u8(1);
                    enc_stmts(w, f);
                }
                None => w.u8(0),
            }
        }
        Stmt::Switch { disc, cases } => {
            w.u8(14);
            enc_expr(w, disc);
            w.uv(cases.len() as u64);
            for c in cases {
                enc_opt_expr(w, &c.test);
                enc_stmts(w, &c.body);
            }
        }
        Stmt::Labeled { label, body } => {
            w.u8(15);
            w.str(label);
            enc_stmt(w, body);
        }
        Stmt::With { obj, body } => {
            w.u8(16);
            enc_expr(w, obj);
            enc_stmt(w, body);
        }
        Stmt::ClassDecl(c) => {
            w.u8(17);
            enc_class(w, c);
        }
        Stmt::Empty => w.u8(18),
        Stmt::Debugger => w.u8(19),
        Stmt::Import(d) => {
            w.u8(20);
            w.str(&d.source);
            w.uv(d.specs.len() as u64);
            for s in &d.specs {
                enc_importspec(w, s);
            }
            enc_opt_str(w, &d.attr_type);
        }
        Stmt::ExportNamed { specs, source } => {
            w.u8(21);
            w.uv(specs.len() as u64);
            for s in specs {
                w.str(&s.local);
                w.str(&s.exported);
            }
            enc_opt_rcstr(w, source);
        }
        Stmt::ExportDecl(s) => {
            w.u8(22);
            enc_stmt(w, s);
        }
        Stmt::ExportDefault(s) => {
            w.u8(23);
            enc_stmt(w, s);
        }
        Stmt::ExportAll { source, exported } => {
            w.u8(24);
            w.str(source);
            enc_opt_str(w, exported);
        }
    }
}

fn dec_stmt(r: &mut Reader) -> R<Stmt> {
    Ok(match r.u8()? {
        0 => Stmt::Expr(dec_expr(r)?),
        1 => {
            let kind = dec_declkind(r)?;
            let n = r.uv()? as usize;
            let mut decls = Vec::with_capacity(n);
            for _ in 0..n {
                decls.push((dec_pattern(r)?, dec_opt_expr(r)?));
            }
            Stmt::VarDecl { kind, decls }
        }
        2 => Stmt::FuncDecl(dec_function(r)?),
        3 => Stmt::Return(dec_opt_expr(r)?),
        4 => {
            let test = dec_expr(r)?;
            let cons = Box::new(dec_stmt(r)?);
            let alt = if r.u8()? == 1 {
                Some(Box::new(dec_stmt(r)?))
            } else {
                None
            };
            Stmt::If { test, cons, alt }
        }
        5 => Stmt::Block(dec_stmts(r)?),
        6 => Stmt::While {
            test: dec_expr(r)?,
            body: Box::new(dec_stmt(r)?),
        },
        7 => Stmt::DoWhile {
            body: Box::new(dec_stmt(r)?),
            test: dec_expr(r)?,
        },
        8 => {
            let init = if r.u8()? == 1 {
                Some(Box::new(dec_forinit(r)?))
            } else {
                None
            };
            Stmt::For {
                init,
                test: dec_opt_expr(r)?,
                update: dec_opt_expr(r)?,
                body: Box::new(dec_stmt(r)?),
            }
        }
        9 => {
            let decl = if r.u8()? == 1 {
                Some(dec_declkind(r)?)
            } else {
                None
            };
            Stmt::ForInOf {
                decl,
                left: dec_pattern(r)?,
                right: dec_expr(r)?,
                of: r.bool()?,
                is_await: r.bool()?,
                body: Box::new(dec_stmt(r)?),
            }
        }
        10 => Stmt::Break(dec_opt_str(r)?),
        11 => Stmt::Continue(dec_opt_str(r)?),
        12 => Stmt::Throw(dec_expr(r)?),
        13 => {
            let block = dec_stmts(r)?;
            let handler = if r.u8()? == 1 {
                let param = if r.u8()? == 1 {
                    Some(dec_pattern(r)?)
                } else {
                    None
                };
                Some((param, dec_stmts(r)?))
            } else {
                None
            };
            let finalizer = if r.u8()? == 1 {
                Some(dec_stmts(r)?)
            } else {
                None
            };
            Stmt::Try {
                block,
                handler,
                finalizer,
            }
        }
        14 => {
            let disc = dec_expr(r)?;
            let n = r.uv()? as usize;
            let mut cases = Vec::with_capacity(n);
            for _ in 0..n {
                cases.push(SwitchCase {
                    test: dec_opt_expr(r)?,
                    body: dec_stmts(r)?,
                });
            }
            Stmt::Switch { disc, cases }
        }
        15 => Stmt::Labeled {
            label: r.str()?,
            body: Box::new(dec_stmt(r)?),
        },
        16 => Stmt::With {
            obj: dec_expr(r)?,
            body: Box::new(dec_stmt(r)?),
        },
        17 => Stmt::ClassDecl(Rc::new(dec_class(r)?)),
        18 => Stmt::Empty,
        19 => Stmt::Debugger,
        20 => {
            let source = r.rcstr()?;
            let n = r.uv()? as usize;
            let mut specs = Vec::with_capacity(n);
            for _ in 0..n {
                specs.push(dec_importspec(r)?);
            }
            Stmt::Import(ImportDecl {
                source,
                specs,
                attr_type: dec_opt_str(r)?,
            })
        }
        21 => {
            let n = r.uv()? as usize;
            let mut specs = Vec::with_capacity(n);
            for _ in 0..n {
                specs.push(ExportSpec {
                    local: r.str()?,
                    exported: r.str()?,
                });
            }
            Stmt::ExportNamed {
                specs,
                source: dec_opt_rcstr(r)?,
            }
        }
        22 => Stmt::ExportDecl(Box::new(dec_stmt(r)?)),
        23 => Stmt::ExportDefault(Box::new(dec_stmt(r)?)),
        24 => Stmt::ExportAll {
            source: r.rcstr()?,
            exported: dec_opt_str(r)?,
        },
        t => return Err(format!("snapshot: bad Stmt tag {t}")),
    })
}

// ---- Expr -------------------------------------------------------------------------------------

fn enc_expr(w: &mut Writer, e: &Expr) {
    match e {
        Expr::Paren(x) => {
            w.u8(0);
            enc_expr(w, x);
        }
        Expr::Num(n) => {
            w.u8(1);
            w.f64(*n);
        }
        Expr::BigInt(b) => {
            w.u8(2);
            w.str(&b.to_string_radix(16));
        }
        Expr::Str(s) => {
            w.u8(3);
            w.str(s);
        }
        Expr::ToStr(x) => {
            w.u8(4);
            enc_expr(w, x);
        }
        Expr::Bool(b) => {
            w.u8(5);
            w.bool(*b);
        }
        Expr::Null => w.u8(6),
        Expr::Undefined => w.u8(7),
        Expr::Ident(n) => {
            w.u8(8);
            w.str(n);
        }
        Expr::This => w.u8(9),
        Expr::Regex { body, flags } => {
            w.u8(10);
            w.str(body);
            w.str(flags);
        }
        Expr::Array(elems) => {
            w.u8(11);
            enc_array_elems(w, elems);
        }
        Expr::Object(props) => {
            w.u8(12);
            w.uv(props.len() as u64);
            for p in props {
                enc_propdef(w, p);
            }
        }
        Expr::Func(f) => {
            w.u8(13);
            enc_function(w, f);
        }
        Expr::Class(c) => {
            w.u8(14);
            enc_class(w, c);
        }
        Expr::Yield { delegate, arg } => {
            w.u8(15);
            w.bool(*delegate);
            match arg {
                Some(a) => {
                    w.u8(1);
                    enc_expr(w, a);
                }
                None => w.u8(0),
            }
        }
        Expr::Await(x) => {
            w.u8(16);
            enc_expr(w, x);
        }
        Expr::Super => w.u8(17),
        Expr::Unary { op, arg } => {
            w.u8(18);
            w.str(op);
            enc_expr(w, arg);
        }
        Expr::Update { op, prefix, arg } => {
            w.u8(19);
            w.str(op);
            w.bool(*prefix);
            enc_expr(w, arg);
        }
        Expr::Binary { op, left, right } => {
            w.u8(20);
            w.str(op);
            enc_expr(w, left);
            enc_expr(w, right);
        }
        Expr::Logical { op, left, right } => {
            w.u8(21);
            w.str(op);
            enc_expr(w, left);
            enc_expr(w, right);
        }
        Expr::Assign { op, target, value } => {
            w.u8(22);
            w.str(op);
            enc_expr(w, target);
            enc_expr(w, value);
        }
        Expr::Cond { test, cons, alt } => {
            w.u8(23);
            enc_expr(w, test);
            enc_expr(w, cons);
            enc_expr(w, alt);
        }
        Expr::Call {
            callee,
            args,
            optional,
            pos,
        } => {
            if let (Some(deps), Expr::Ident(name), [ArrayElem::Item(Expr::Str(spec))]) =
                (&mut w.deps, &**callee, args.as_slice())
            {
                if name == "require" && !deps.requires.iter().any(|d| **d == **spec) {
                    deps.requires.push(spec.to_string());
                }
            }
            w.u8(24);
            enc_expr(w, callee);
            enc_array_elems(w, args);
            w.bool(*optional);
            enc_pos(w, *pos);
        }
        Expr::New { callee, args, pos } => {
            w.u8(25);
            enc_expr(w, callee);
            enc_array_elems(w, args);
            enc_pos(w, *pos);
        }
        Expr::Member {
            obj,
            prop,
            optional,
        } => {
            w.u8(26);
            enc_expr(w, obj);
            w.str(prop);
            w.bool(*optional);
        }
        Expr::Index {
            obj,
            index,
            optional,
        } => {
            w.u8(27);
            enc_expr(w, obj);
            enc_expr(w, index);
            w.bool(*optional);
        }
        Expr::Seq(exprs) => {
            w.u8(28);
            enc_exprs(w, exprs);
        }
        Expr::TaggedTemplate {
            tag, quasis, subs, ..
        } => {
            w.u8(29);
            enc_expr(w, tag);
            w.uv(quasis.len() as u64);
            for (cooked, raw) in quasis.iter() {
                enc_opt_str(w, cooked);
                w.str(raw);
            }
            enc_exprs(w, subs);
        }
        Expr::OptionalChain(x) => {
            w.u8(30);
            enc_expr(w, x);
        }
        Expr::PrivateIn { name, obj } => {
            w.u8(31);
            w.str(name);
            enc_expr(w, obj);
        }
        Expr::ImportCall {
            spec,
            phase,
            options,
        } => {
            if let (Some(deps), Expr::Str(s), ImportPhase::Evaluation, None) =
                (&mut w.deps, &**spec, phase, options)
            {
                if !deps.dynamic_imports.iter().any(|d| **d == **s) {
                    deps.dynamic_imports.push(s.to_string());
                }
            }
            w.u8(32);
            enc_expr(w, spec);
            w.u8(match phase {
                ImportPhase::Evaluation => 0,
                ImportPhase::Source => 1,
                ImportPhase::Defer => 2,
            });
            match options {
                Some(o) => {
                    w.u8(1);
                    enc_expr(w, o);
                }
                None => w.u8(0),
            }
        }
        Expr::ImportMeta => w.u8(33),
        Expr::NewTarget => w.u8(34),
    }
}

fn dec_expr(r: &mut Reader) -> R<Expr> {
    Ok(match r.u8()? {
        0 => Expr::Paren(Box::new(dec_expr(r)?)),
        1 => Expr::Num(r.f64()?),
        2 => Expr::BigInt(JsBigInt::parse_radix(&r.str()?, 16).ok_or("snapshot: bad bigint")?),
        3 => Expr::Str(r.rcstr()?),
        4 => Expr::ToStr(Box::new(dec_expr(r)?)),
        5 => Expr::Bool(r.bool()?),
        6 => Expr::Null,
        7 => Expr::Undefined,
        8 => Expr::Ident(r.str()?),
        9 => Expr::This,
        10 => Expr::Regex {
            body: r.rcstr()?,
            flags: r.rcstr()?,
        },
        11 => Expr::Array(dec_array_elems(r)?),
        12 => {
            let n = r.uv()? as usize;
            let mut props = Vec::with_capacity(n);
            for _ in 0..n {
                props.push(dec_propdef(r)?);
            }
            Expr::Object(props)
        }
        13 => Expr::Func(dec_function(r)?),
        14 => Expr::Class(Rc::new(dec_class(r)?)),
        15 => {
            let delegate = r.bool()?;
            let arg = if r.u8()? == 1 {
                Some(Box::new(dec_expr(r)?))
            } else {
                None
            };
            Expr::Yield { delegate, arg }
        }
        16 => Expr::Await(Box::new(dec_expr(r)?)),
        17 => Expr::Super,
        18 => Expr::Unary {
            op: intern_op(&r.str()?)?,
            arg: Box::new(dec_expr(r)?),
        },
        19 => Expr::Update {
            op: intern_op(&r.str()?)?,
            prefix: r.bool()?,
            arg: Box::new(dec_expr(r)?),
        },
        20 => Expr::Binary {
            op: intern_op(&r.str()?)?,
            left: Box::new(dec_expr(r)?),
            right: Box::new(dec_expr(r)?),
        },
        21 => Expr::Logical {
            op: intern_op(&r.str()?)?,
            left: Box::new(dec_expr(r)?),
            right: Box::new(dec_expr(r)?),
        },
        22 => Expr::Assign {
            op: intern_op(&r.str()?)?,
            target: Box::new(dec_expr(r)?),
            value: Box::new(dec_expr(r)?),
        },
        23 => Expr::Cond {
            test: Box::new(dec_expr(r)?),
            cons: Box::new(dec_expr(r)?),
            alt: Box::new(dec_expr(r)?),
        },
        24 => Expr::Call {
            callee: Box::new(dec_expr(r)?),
            args: dec_array_elems(r)?,
            optional: r.bool()?,
            pos: dec_pos(r)?,
        },
        25 => Expr::New {
            callee: Box::new(dec_expr(r)?),
            args: dec_array_elems(r)?,
            pos: dec_pos(r)?,
        },
        26 => Expr::Member {
            obj: Box::new(dec_expr(r)?),
            prop: r.str()?,
            optional: r.bool()?,
        },
        27 => Expr::Index {
            obj: Box::new(dec_expr(r)?),
            index: Box::new(dec_expr(r)?),
            optional: r.bool()?,
        },
        28 => Expr::Seq(dec_exprs(r)?),
        29 => {
            let tag = Box::new(dec_expr(r)?);
            let n = r.uv()? as usize;
            let mut quasis = Vec::with_capacity(n);
            for _ in 0..n {
                quasis.push((dec_opt_str(r)?, r.str()?));
            }
            Expr::TaggedTemplate {
                tag,
                quasis: quasis.into_boxed_slice(),
                site: crate::parser::fresh_template_site(),
                subs: dec_exprs(r)?,
            }
        }
        30 => Expr::OptionalChain(Box::new(dec_expr(r)?)),
        31 => Expr::PrivateIn {
            name: r.str()?,
            obj: Box::new(dec_expr(r)?),
        },
        32 => Expr::ImportCall {
            spec: Box::new(dec_expr(r)?),
            phase: match r.u8()? {
                0 => ImportPhase::Evaluation,
                1 => ImportPhase::Source,
                2 => ImportPhase::Defer,
                t => return Err(format!("snapshot: bad ImportPhase {t}")),
            },
            options: if r.u8()? == 1 {
                Some(Box::new(dec_expr(r)?))
            } else {
                None
            },
        },
        33 => Expr::ImportMeta,
        34 => Expr::NewTarget,
        t => return Err(format!("snapshot: bad Expr tag {t}")),
    })
}

// ---- shared sub-structures --------------------------------------------------------------------

fn enc_array_elems(w: &mut Writer, elems: &[ArrayElem]) {
    w.uv(elems.len() as u64);
    for el in elems {
        match el {
            ArrayElem::Item(e) => {
                w.u8(0);
                enc_expr(w, e);
            }
            ArrayElem::Spread(e) => {
                w.u8(1);
                enc_expr(w, e);
            }
            ArrayElem::Hole => w.u8(2),
        }
    }
}
fn dec_array_elems(r: &mut Reader) -> R<Vec<ArrayElem>> {
    let n = r.uv()? as usize;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(match r.u8()? {
            0 => ArrayElem::Item(dec_expr(r)?),
            1 => ArrayElem::Spread(dec_expr(r)?),
            2 => ArrayElem::Hole,
            t => return Err(format!("snapshot: bad ArrayElem {t}")),
        });
    }
    Ok(out)
}

fn enc_propdef(w: &mut Writer, p: &PropDef) {
    match p {
        PropDef::KeyValue { key, value } => {
            w.u8(0);
            enc_propkey(w, key);
            enc_expr(w, value);
        }
        PropDef::Cover { key, value } => {
            w.u8(1);
            enc_propkey(w, key);
            enc_expr(w, value);
        }
        PropDef::Method { key, func } => {
            w.u8(2);
            enc_propkey(w, key);
            enc_function(w, func);
        }
        PropDef::Getter { key, func } => {
            w.u8(3);
            enc_propkey(w, key);
            enc_function(w, func);
        }
        PropDef::Setter { key, func } => {
            w.u8(4);
            enc_propkey(w, key);
            enc_function(w, func);
        }
        PropDef::Spread(e) => {
            w.u8(5);
            enc_expr(w, e);
        }
        PropDef::Proto(e) => {
            w.u8(6);
            enc_expr(w, e);
        }
    }
}
fn dec_propdef(r: &mut Reader) -> R<PropDef> {
    Ok(match r.u8()? {
        0 => PropDef::KeyValue {
            key: dec_propkey(r)?,
            value: dec_expr(r)?,
        },
        1 => PropDef::Cover {
            key: dec_propkey(r)?,
            value: dec_expr(r)?,
        },
        2 => PropDef::Method {
            key: dec_propkey(r)?,
            func: dec_function(r)?,
        },
        3 => PropDef::Getter {
            key: dec_propkey(r)?,
            func: dec_function(r)?,
        },
        4 => PropDef::Setter {
            key: dec_propkey(r)?,
            func: dec_function(r)?,
        },
        5 => PropDef::Spread(dec_expr(r)?),
        6 => PropDef::Proto(dec_expr(r)?),
        t => return Err(format!("snapshot: bad PropDef {t}")),
    })
}

fn enc_propkey(w: &mut Writer, k: &PropKey) {
    match k {
        PropKey::Ident(n) => {
            w.u8(0);
            w.str(n);
        }
        PropKey::Str(s) => {
            w.u8(1);
            w.str(s);
        }
        PropKey::Num(n) => {
            w.u8(2);
            w.f64(*n);
        }
        PropKey::Computed(e) => {
            w.u8(3);
            enc_expr(w, e);
        }
    }
}
fn dec_propkey(r: &mut Reader) -> R<PropKey> {
    Ok(match r.u8()? {
        0 => PropKey::Ident(r.str()?),
        1 => PropKey::Str(r.rcstr()?),
        2 => PropKey::Num(r.f64()?),
        3 => PropKey::Computed(dec_expr(r)?),
        t => return Err(format!("snapshot: bad PropKey {t}")),
    })
}

fn enc_pattern(w: &mut Writer, p: &Pattern) {
    match p {
        Pattern::Ident(n) => {
            w.u8(0);
            w.str(n);
        }
        Pattern::Array(elems) => {
            w.u8(1);
            w.uv(elems.len() as u64);
            for el in elems {
                match el {
                    ArrayPatElem::Hole => w.u8(0),
                    ArrayPatElem::Elem { pattern, default } => {
                        w.u8(1);
                        enc_pattern(w, pattern);
                        enc_opt_expr(w, default);
                    }
                    ArrayPatElem::Rest(p) => {
                        w.u8(2);
                        enc_pattern(w, p);
                    }
                }
            }
        }
        Pattern::Object(op) => {
            w.u8(2);
            w.uv(op.props.len() as u64);
            for prop in &op.props {
                enc_propkey(w, &prop.key);
                enc_pattern(w, &prop.value);
                enc_opt_expr(w, &prop.default);
            }
            enc_opt_str(w, &op.rest);
        }
        Pattern::Member(e) => {
            w.u8(3);
            enc_expr(w, e);
        }
    }
}
fn dec_pattern(r: &mut Reader) -> R<Pattern> {
    Ok(match r.u8()? {
        0 => Pattern::Ident(r.str()?),
        1 => {
            let n = r.uv()? as usize;
            let mut elems = Vec::with_capacity(n);
            for _ in 0..n {
                elems.push(match r.u8()? {
                    0 => ArrayPatElem::Hole,
                    1 => ArrayPatElem::Elem {
                        pattern: dec_pattern(r)?,
                        default: dec_opt_expr(r)?,
                    },
                    2 => ArrayPatElem::Rest(dec_pattern(r)?),
                    t => return Err(format!("snapshot: bad ArrayPatElem {t}")),
                });
            }
            Pattern::Array(elems)
        }
        2 => {
            let n = r.uv()? as usize;
            let mut props = Vec::with_capacity(n);
            for _ in 0..n {
                props.push(ObjPatProp {
                    key: dec_propkey(r)?,
                    value: dec_pattern(r)?,
                    default: dec_opt_expr(r)?,
                });
            }
            Pattern::Object(ObjectPat {
                props,
                rest: dec_opt_str(r)?,
            })
        }
        3 => Pattern::Member(Box::new(dec_expr(r)?)),
        t => return Err(format!("snapshot: bad Pattern {t}")),
    })
}

fn enc_fnsource(w: &mut Writer, s: &FnSource) {
    // A function parsed out of a template substitution carries a range into the substitution's
    // own text (the parser re-lexes `${...}` separately), not into the unit's source: its range
    // means nothing against the unit source, so its text is written out instead.
    let foreign = match s {
        FnSource::Range { src, .. } => !w.is_unit_src(src),
        FnSource::Kept { .. } => true,
        _ => false,
    };
    if w.strip {
        if let FnSource::Range { start, end, .. } = s {
            if !foreign {
                if let Some(ranges) = &mut w.ranges {
                    ranges.push((*start, *end));
                }
                if let Some((start, end)) = w.keep.as_ref().and_then(|k| k.map(*start, *end)) {
                    w.u8(2);
                    w.uv(start as u64);
                    w.uv(end as u64);
                    return;
                }
            }
        }
        if w.keep.is_some() && (foreign || matches!(s, FnSource::Text(_))) {
            if let Some(t) = s.as_str() {
                w.u8(1);
                w.str(t);
                return;
            }
        }
        return w.u8(0);
    }
    match s {
        FnSource::None => w.u8(0),
        FnSource::Range { .. } if foreign => match s.as_str() {
            Some(t) => {
                w.u8(1);
                w.str(t);
            }
            None => w.u8(0),
        },
        FnSource::Text(t) => {
            w.u8(1);
            w.str(t);
        }
        FnSource::Range { start, end, .. } => {
            w.u8(2);
            w.uv(*start as u64);
            w.uv(*end as u64);
        }
        FnSource::Kept { .. } => unreachable!("a kept range is foreign"),
    }
}
fn dec_fnsource(r: &mut Reader) -> R<FnSource> {
    Ok(match r.u8()? {
        0 => FnSource::None,
        1 => FnSource::Text(r.rcstr()?),
        2 if r.split.is_some() => {
            // A range into kept text that is still compressed: checked when it is sliced.
            let start = r.uv()? as u32;
            let end = r.uv()? as u32;
            let text = r.split.as_ref().and_then(|s| s.kept.clone());
            let text = text.ok_or("snapshot: source range without kept text")?;
            FnSource::Kept { text, start, end }
        }
        2 => {
            let (start, end) = r.range()?;
            FnSource::Range {
                src: r.src.clone(),
                start,
                end,
            }
        }
        t => return Err(format!("snapshot: bad FnSource {t}")),
    })
}

/// A class's private-name scope, by identity: defined inline (parent first, then its names) the
/// first time a lazy body refers to it, referenced by definition index afterwards, so every
/// method of a class shares one `Rc` after decode just as after a parse.
fn enc_scope(w: &mut Writer, s: &Option<Rc<PrivateScope>>) {
    let Some(s) = s else {
        w.u8(0);
        return;
    };
    let key = Rc::as_ptr(s);
    if let Some(&id) = w.scopes.get(&key) {
        w.u8(1);
        w.uv(id as u64);
        return;
    }
    w.u8(2);
    enc_scope(w, &s.parent);
    let names = s.names.get().map(Vec::as_slice).unwrap_or_default();
    w.uv(names.len() as u64);
    for n in names {
        w.str(n);
    }
    let id = w.scopes.len() as u32;
    w.scopes.insert(key, id);
}
fn dec_scope(r: &mut Reader) -> R<Option<Rc<PrivateScope>>> {
    Ok(match r.u8()? {
        0 => None,
        1 => {
            let id = r.uv()? as usize;
            Some(
                r.scopes
                    .get(id)
                    .cloned()
                    .ok_or("snapshot: bad private scope ref")?,
            )
        }
        2 => {
            let parent = dec_scope(r)?;
            let n = r.uv()? as usize;
            let mut names = Vec::with_capacity(n);
            for _ in 0..n {
                names.push(r.str()?);
            }
            let scope = Rc::new(PrivateScope {
                names: OnceCell::from(names),
                parent,
            });
            r.scopes.push(scope.clone());
            Some(scope)
        }
        t => return Err(format!("snapshot: bad private scope tag {t}")),
    })
}

fn enc_lazy(w: &mut Writer, l: &LazyBody) {
    w.uv(l.start as u64);
    w.uv(l.end as u64);
    w.uv(l.line as u64);
    w.bool(l.html_comments);
    let c = &l.ctx;
    let bits = (c.strict as u16)
        | (c.in_generator as u16) << 1
        | (c.in_async as u16) << 2
        | (c.module as u16) << 3
        | (c.allow_new_target as u16) << 4
        | (c.super_prop_ok as u16) << 5
        | (c.super_call_ok as u16) << 6
        | (c.in_derived_class as u16) << 7
        | (c.no_arguments_refs as u16) << 8
        | (c.in_field_init as u16) << 9;
    w.uv(bits as u64);
    enc_scope(w, &l.private_scope);
}
fn dec_lazy(r: &mut Reader) -> R<LazyBody> {
    let (start, end) = r.range()?;
    let line = r.uv()? as u32;
    let html_comments = r.bool()?;
    let bits = r.uv()?;
    let ctx = LazyCtx {
        strict: bits & 1 != 0,
        in_generator: bits & 2 != 0,
        in_async: bits & 4 != 0,
        module: bits & 8 != 0,
        allow_new_target: bits & 16 != 0,
        super_prop_ok: bits & 32 != 0,
        super_call_ok: bits & 64 != 0,
        in_derived_class: bits & 128 != 0,
        no_arguments_refs: bits & 256 != 0,
        in_field_init: bits & 512 != 0,
    };
    Ok(LazyBody {
        src: r.src.clone(),
        start,
        end,
        line,
        html_comments,
        ctx,
        private_scope: dec_scope(r)?,
        aot: None,
    })
}

/// A function the parser skipped the body of is written as its [`LazyBody`] — whether or not
/// the body has since been materialised — and decodes lazy again; an eagerly parsed one (an
/// IIFE, eval code) carries its statements.
fn enc_function(w: &mut Writer, f: &Rc<Function>) {
    if let Some(funcs) = &mut w.funcs {
        funcs.push(f.clone());
    }
    if w.split.is_some() {
        return enc_function_split(w, f);
    }
    enc_opt_str(w, &f.name);
    w.uv(f.params.len() as u64);
    for p in &f.params {
        enc_pattern(w, &p.pattern);
        enc_opt_expr(w, &p.default);
        w.bool(p.rest);
    }
    // Flags packed into one byte.
    let flags = (f.is_arrow as u8)
        | (f.is_strict as u8) << 1
        | (f.expr_body as u8) << 2
        | (f.is_generator as u8) << 3
        | (f.is_async as u8) << 4
        | (f.is_method as u8) << 5
        | (f.is_fn_expr as u8) << 6;
    w.u8(flags);
    enc_fnsource(w, &f.source);
    if w.strip {
        w.u8(0);
        return enc_stmts(w, &f.body());
    }
    let lazy = f.lazy.borrow();
    match lazy.as_ref() {
        Some(l) => {
            w.u8(1);
            enc_lazy(w, l);
        }
        None => {
            w.u8(0);
            enc_stmts(w, &f.body());
        }
    }
}
/// Split mode: the reference here, the header and body into their own buffers.
fn enc_function_split(w: &mut Writer, f: &Rc<Function>) {
    let idx = w.funcs.as_ref().map_or(0, |v| v.len() - 1) as u64;
    let sp = w.split.as_mut().expect("split mode");
    let owner = sp.cur;
    let slot = sp.next_local.entry(owner).or_insert(0);
    let local = *slot;
    *slot += 1;
    sp.headers.push(Vec::new());
    sp.bodies.push(Vec::new());
    sp.owners.push(owner);
    sp.ends.push(0);
    sp.cur = 2 * idx + 2;
    w.uv(local);
    let outer = std::mem::take(&mut w.buf);
    enc_opt_str(w, &f.name);
    w.uv(f.params.len() as u64);
    for p in &f.params {
        enc_pattern(w, &p.pattern);
        enc_opt_expr(w, &p.default);
        w.bool(p.rest);
    }
    w.u8(fn_flags(f));
    enc_fnsource(w, &f.source);
    // The home-object check is decided here, from the source the build parsed: at run time a
    // precompiled method has no source (or only compressed kept text), and deciding it then
    // would either assume the worst (a home scope per method closure) or decompress the kept
    // text of every method the program instantiates.
    let home = crate::ast::SCAN_HOME_CHECKED
        | if f.may_use_home() {
            crate::ast::SCAN_NEEDS_HOME
        } else {
            0
        };
    w.u8(f.scan_flags() | home);
    let header = std::mem::take(&mut w.buf);
    if let Some(sp) = &mut w.split {
        sp.cur = 2 * idx + 1;
    }
    enc_stmts(w, &f.body());
    let body = std::mem::replace(&mut w.buf, outer);
    let end = w.funcs.as_ref().map_or(0, |v| v.len()) as u32;
    let sp = w.split.as_mut().expect("split mode");
    sp.cur = owner;
    sp.headers[idx as usize] = header;
    sp.bodies[idx as usize] = body;
    sp.ends[idx as usize] = end;
}

fn fn_flags(f: &Function) -> u8 {
    (f.is_arrow as u8)
        | (f.is_strict as u8) << 1
        | (f.expr_body as u8) << 2
        | (f.is_generator as u8) << 3
        | (f.is_async as u8) << 4
        | (f.is_method as u8) << 5
        | (f.is_fn_expr as u8) << 6
}

fn dec_function(r: &mut Reader) -> R<Rc<Function>> {
    if r.split.is_some() {
        let local = r.uv()? as usize;
        let sp = r.split.as_ref().expect("split mode");
        let i = *sp
            .children
            .get(local)
            .ok_or("snapshot: bad function reference")?;
        return sp.unit.function(i);
    }
    let slot = r.funcs.as_mut().map(|funcs| {
        funcs.push(None);
        funcs.len() - 1
    });
    let name = dec_opt_str(r)?;
    let n = r.uv()? as usize;
    let mut params = Vec::with_capacity(n);
    for _ in 0..n {
        params.push(Param {
            pattern: dec_pattern(r)?,
            default: dec_opt_expr(r)?,
            rest: r.bool()?,
        });
    }
    let flags = r.u8()?;
    let source = dec_fnsource(r)?;
    let (body, lazy) = match r.u8()? {
        0 => (Some(Rc::new(dec_stmts(r)?)), None),
        1 => (None, Some(Box::new(dec_lazy(r)?))),
        t => return Err(format!("snapshot: bad body tag {t}")),
    };
    let f = Rc::new(Function {
        name,
        params,
        body: RefCell::new(body),
        lazy: RefCell::new(lazy),
        lazy_error: OnceCell::new(),
        is_arrow: flags & 1 != 0,
        is_strict: flags & 2 != 0,
        expr_body: flags & 4 != 0,
        is_generator: flags & 8 != 0,
        is_async: flags & 16 != 0,
        is_method: flags & 32 != 0,
        is_fn_expr: flags & 64 != 0,
        source,
        // Lazy runtime caches — start empty, exactly as the parser leaves them.
        scan: Cell::new(0),
        hoist: RefCell::new(None),
        body_used: Cell::new(false),
        calls: Cell::new(0),
        code: OnceCell::new(),
        fn_maps: OnceCell::new(),
    });
    // Same registration a parsed lazy function gets (parser `rc_fn`): the collector releases
    // the body once it goes cold.
    if f.lazy.borrow().is_some() {
        crate::value::register_lazy_function(&f);
    }
    if let (Some(funcs), Some(slot)) = (&mut r.funcs, slot) {
        funcs[slot] = Some(f.clone());
    }
    Ok(f)
}

fn enc_class(w: &mut Writer, c: &Class) {
    enc_opt_str(w, &c.name);
    match &c.superclass {
        Some(sc) => {
            w.u8(1);
            enc_expr(w, sc);
        }
        None => w.u8(0),
    }
    w.uv(c.members.len() as u64);
    for m in &c.members {
        enc_propkey(w, &m.key);
        w.u8(match m.kind {
            MemberKind::Constructor => 0,
            MemberKind::Method => 1,
            MemberKind::Get => 2,
            MemberKind::Set => 3,
            MemberKind::Field => 4,
            MemberKind::Accessor => 5,
            MemberKind::StaticBlock => 6,
        });
        w.bool(m.is_static);
        match &m.func {
            Some(f) => {
                w.u8(1);
                enc_function(w, f);
            }
            None => w.u8(0),
        }
        enc_opt_expr(w, &m.value);
        enc_exprs(w, &m.decorators);
    }
    enc_exprs(w, &c.decorators);
    enc_fnsource(w, &c.source);
}
fn dec_class(r: &mut Reader) -> R<Class> {
    let name = dec_opt_str(r)?;
    let superclass = if r.u8()? == 1 {
        Some(Box::new(dec_expr(r)?))
    } else {
        None
    };
    let n = r.uv()? as usize;
    let mut members = Vec::with_capacity(n);
    for _ in 0..n {
        let key = dec_propkey(r)?;
        let kind = match r.u8()? {
            0 => MemberKind::Constructor,
            1 => MemberKind::Method,
            2 => MemberKind::Get,
            3 => MemberKind::Set,
            4 => MemberKind::Field,
            5 => MemberKind::Accessor,
            6 => MemberKind::StaticBlock,
            t => return Err(format!("snapshot: bad MemberKind {t}")),
        };
        let is_static = r.bool()?;
        let func = if r.u8()? == 1 {
            Some(dec_function(r)?)
        } else {
            None
        };
        members.push(ClassMember {
            key,
            kind,
            is_static,
            func,
            value: dec_opt_expr(r)?,
            decorators: dec_exprs(r)?,
        });
    }
    Ok(Class {
        name,
        superclass,
        members,
        decorators: dec_exprs(r)?,
        source: dec_fnsource(r)?,
    })
}

fn enc_forinit(w: &mut Writer, fi: &ForInit) {
    match fi {
        ForInit::VarDecl { kind, decls } => {
            w.u8(0);
            enc_declkind(w, *kind);
            w.uv(decls.len() as u64);
            for (pat, init) in decls {
                enc_pattern(w, pat);
                enc_opt_expr(w, init);
            }
        }
        ForInit::Expr(e) => {
            w.u8(1);
            enc_expr(w, e);
        }
    }
}
fn dec_forinit(r: &mut Reader) -> R<ForInit> {
    Ok(match r.u8()? {
        0 => {
            let kind = dec_declkind(r)?;
            let n = r.uv()? as usize;
            let mut decls = Vec::with_capacity(n);
            for _ in 0..n {
                decls.push((dec_pattern(r)?, dec_opt_expr(r)?));
            }
            ForInit::VarDecl { kind, decls }
        }
        1 => ForInit::Expr(dec_expr(r)?),
        t => return Err(format!("snapshot: bad ForInit {t}")),
    })
}

fn enc_importspec(w: &mut Writer, s: &ImportSpec) {
    match s {
        ImportSpec::Default(n) => {
            w.u8(0);
            w.str(n);
        }
        ImportSpec::Namespace(n) => {
            w.u8(1);
            w.str(n);
        }
        ImportSpec::DeferNamespace(n) => {
            w.u8(2);
            w.str(n);
        }
        ImportSpec::Source(n) => {
            w.u8(3);
            w.str(n);
        }
        ImportSpec::Named { imported, local } => {
            w.u8(4);
            w.str(imported);
            w.str(local);
        }
    }
}
fn dec_importspec(r: &mut Reader) -> R<ImportSpec> {
    Ok(match r.u8()? {
        0 => ImportSpec::Default(r.str()?),
        1 => ImportSpec::Namespace(r.str()?),
        2 => ImportSpec::DeferNamespace(r.str()?),
        3 => ImportSpec::Source(r.str()?),
        4 => ImportSpec::Named {
            imported: r.str()?,
            local: r.str()?,
        },
        t => return Err(format!("snapshot: bad ImportSpec {t}")),
    })
}

fn enc_declkind(w: &mut Writer, k: DeclKind) {
    w.u8(match k {
        DeclKind::Var => 0,
        DeclKind::Let => 1,
        DeclKind::Const => 2,
        DeclKind::Using => 3,
        DeclKind::AwaitUsing => 4,
    });
}
fn dec_declkind(r: &mut Reader) -> R<DeclKind> {
    Ok(match r.u8()? {
        0 => DeclKind::Var,
        1 => DeclKind::Let,
        2 => DeclKind::Const,
        3 => DeclKind::Using,
        4 => DeclKind::AwaitUsing,
        t => return Err(format!("snapshot: bad DeclKind {t}")),
    })
}

#[cfg(test)]
mod tests {
    use super::{decode, encode};
    use crate::ast::{Expr, Stmt};
    use crate::{Completion, Engine};

    /// `encode` and `decode` must be exact inverses: re-encoding a decoded tree reproduces the
    /// bytes. This catches every tag/field-order asymmetry (a *dropped* field is caught instead
    /// by the behavioral suites that run the decoded glue).
    fn assert_roundtrips(src: &str) {
        let blob = crate::compile_snapshot(src).unwrap_or_else(|e| panic!("compile {src:?}: {e}"));
        let ast = decode(&blob, src).unwrap_or_else(|e| panic!("decode {src:?}: {e}"));
        assert_eq!(blob, encode(&ast, src), "re-encode differs for: {src}");
    }

    /// Compile `src` to a snapshot and run it from the snapshot; `'passed'` is the expected
    /// completion.
    fn run_snapshot(src: &str) -> Completion {
        let blob = crate::compile_snapshot(src).unwrap_or_else(|e| panic!("compile: {e}"));
        let mut engine = Engine::new();
        engine
            .eval_snapshot(&blob, src, false)
            .unwrap_or_else(|e| panic!("decode: {}", e.message))
    }

    fn check_snapshot(src: &str) {
        let script = format!(
            "function assert(x, m) {{ if (!x) throw new Error(m || 'assertion'); }}\n{src}\n'passed'"
        );
        match run_snapshot(&script) {
            Completion::Value(v) => assert_eq!(v, "passed"),
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
    }

    #[test]
    fn roundtrip_diverse_constructs() {
        for src in [
            "1; 'two'; true; null; undefined; 0xffn; 1.5e10;",
            "let { a, b: [c, ...d] = [], ...rest } = obj; const [x = 1, , z] = arr;",
            "class C extends B { #f = 1; static s = 2; get g(){return this.#f} set g(v){} accessor a; static { init(); } has(o){ return #f in o } ['x'+y]() {} }",
            "async function* gen(a, b = 1, ...c) { yield* a; await b; for await (const x of c) {} }",
            "const f = (x) => x ? a?.b.c?.() : `t${x}${'raw'}`; tag`a${1}b`;",
            "try { throw new E(); } catch { } finally { } with (o) { o.p = 1; } label: for (k in o) break label;",
            "a ??= b; c ||= d; e &&= f; g **= h; -x; !y; typeof z; void 0; delete o.p; a in b; a instanceof B;",
            "switch (x) { case 1: break; default: } do { i++ } while (i < 10); function ctor(){ return new.target; }",
            "const p = import('m'); const q = import('m', { with: { type: 'json' } });",
            "obj = { __proto__: p, shorthand, key: v, [comp]: w, m() {}, get g() {}, *gen() {}, async am() {}, ...spread };",
            "(function iife() { function inner() { return 1; } return inner(); })(); (() => { class K { #p; m() { return this.#p; } } })();",
            "class Outer { #o; m() { class Inner { #i; n() { return this.#i + this.#o; } } return Inner; } }",
        ] {
            assert_roundtrips(src);
        }
    }

    #[test]
    fn roundtrip_real_web_glue() {
        // The actual runtime glue — the thing the build-time snapshot will encode.
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../lumen-web/src/js/");
        for file in [
            "events.js",
            "encoding.js",
            "url.js",
            "streams.js",
            "fetch.js",
            "server.js",
            "crypto.js",
        ] {
            let src = std::fs::read_to_string(format!("{dir}{file}")).unwrap();
            assert_roundtrips(&src);
        }
    }

    #[test]
    fn a_skipped_body_decodes_lazy_and_shares_the_source() {
        let src = "function f(a) { return a + 1; }\nconst g = () => { return 2; };\n(function iife() { return 3; })();";
        let blob = crate::compile_snapshot(src).unwrap();
        let ast = decode(&blob, src).unwrap();
        let mut lazy = Vec::new();
        for s in &ast {
            match s {
                Stmt::FuncDecl(f) => lazy.push(f.clone()),
                Stmt::VarDecl { decls, .. } => {
                    if let Some(Expr::Func(f)) = &decls[0].1 {
                        lazy.push(f.clone());
                    }
                }
                Stmt::Expr(Expr::Call { callee, .. }) => {
                    let f = match &**callee {
                        Expr::Func(f) => f,
                        Expr::Paren(inner) => match &**inner {
                            Expr::Func(f) => f,
                            e => panic!("unexpected callee {e:?}"),
                        },
                        e => panic!("unexpected callee {e:?}"),
                    };
                    assert!(f.parsed_body().is_some(), "an IIFE is encoded eagerly");
                    assert!(f.lazy.borrow().is_none());
                    assert_eq!(f.source.as_str(), Some("function iife() { return 3; }"));
                }
                _ => panic!("unexpected {s:?}"),
            }
        }
        assert_eq!(lazy.len(), 2);
        let mut srcs = Vec::new();
        for f in &lazy {
            assert!(f.parsed_body().is_none(), "a skipped body decodes unparsed");
            let l = f.lazy.borrow();
            let l = l.as_ref().expect("lazy record");
            assert_eq!(
                &l.src[l.start as usize..l.end as usize],
                if f.is_arrow {
                    "{ return 2; }"
                } else {
                    "{ return a + 1; }"
                }
            );
            srcs.push(l.src.clone());
            let crate::ast::FnSource::Range { src: s, .. } = &f.source else {
                panic!("source is a range")
            };
            srcs.push(s.clone());
        }
        assert!(
            srcs.windows(2).all(|w| std::rc::Rc::ptr_eq(&w[0], &w[1])),
            "one shared copy"
        );
        assert_eq!(
            lazy[0].source.as_str(),
            Some("function f(a) { return a + 1; }")
        );
    }

    #[test]
    fn decoded_functions_run_from_the_snapshot() {
        check_snapshot(
            "function f(x) { function g() { return x + 1; } return g(); }
             assert(f(1) === 2, 'nested function declaration');
             const arrow = (a, b = 2) => { return a * b; };
             assert(arrow(3) === 6, 'arrow with a default');
             function* gen() { yield 1; yield* [2, 3]; }
             assert([...gen()].join() === '1,2,3', 'generator');
             const o = { m() { return this.v; }, v: 7, get g() { return this.v + 1; } };
             assert(o.m() === 7 && o.g === 8, 'method and getter');
             let done = false;
             async function a() { const v = await Promise.resolve(4); done = v === 4; }
             a();",
        );
    }

    #[test]
    fn decoded_class_methods_keep_private_names_and_super() {
        check_snapshot(
            "class Base { greet() { return 'base'; } static s() { return 'S'; } }
             class C extends Base {
               #x = 1;
               static #count = 0;
               constructor() { super(); C.#count++; }
               get x() { return this.#x; }
               set x(v) { this.#x = v; }
               greet() { return super.greet() + '+c'; }
               static made() { return C.#count; }
               static ss() { return super.s() + '!'; }
               has(o) { return #x in o; }
               nested() { class Inner { #y = 2; both(c) { return this.#y + c.#x; } } return new Inner().both(this); }
             }
             const c = new C();
             assert(c.x === 1, 'private get');
             c.x = 5;
             assert(c.x === 5, 'private set');
             assert(c.greet() === 'base+c', 'super property');
             assert(C.made() === 1 && C.ss() === 'S!', 'static private and static super');
             assert(c.has(c) && !c.has({}), 'private in');
             assert(c.nested() === 7, 'inner class reaches the enclosing private name');",
        );
    }

    /// Early errors inside a skipped body are load-time errors: the lazy parse that feeds the
    /// snapshot encoder rejects the source, so no unparseable body ever reaches the decoder.
    fn lazy_parse_error(src: &str) -> crate::parser::ParseError {
        crate::parser::parse_script_lazy(src).expect_err("the lazy parse rejects the source")
    }

    #[test]
    fn an_undeclared_private_name_in_a_skipped_body_is_a_load_time_syntax_error() {
        let src = "class C { #x; ok() { return this.#x; } bad() { return this.#nope; } }
                   const c = new C();";
        let err = lazy_parse_error(src);
        assert!(err.message.contains("#nope"), "{}", err.message);
    }

    #[test]
    fn a_syntax_error_in_a_skipped_body_is_a_load_time_error() {
        let src = "function bad() { return 1 + ; }
function fine() { return 'ok'; }";
        assert_eq!(lazy_parse_error(src).line, 1);
    }

    #[test]
    fn to_string_of_a_decoded_function_is_its_source_text() {
        check_snapshot(
            "function  f ( a ) { /* c */ return a; }
             const arrow = async (x) => { return x; };
             class K { m() { return 1; } }
             assert(f.toString() === 'function  f ( a ) { /* c */ return a; }', 'declaration: ' + f.toString());
             assert(arrow.toString() === 'async (x) => { return x; }', 'arrow: ' + arrow.toString());
             assert(K.toString() === 'class K { m() { return 1; } }', 'class: ' + K.toString());
             assert(K.prototype.m.toString() === 'm() { return 1; }', 'method: ' + K.prototype.m.toString());
             assert((function iife() { return 2; }).toString() === 'function iife() { return 2; }', 'eager expression');",
        );
    }

    #[test]
    fn a_decoded_body_flushed_by_the_collector_is_reparsed_on_the_next_call() {
        check_snapshot(
            "function f(a, b) { const c = a * b; return c + 1; }
             const before = f(2, 3);
             $262.gc(); $262.gc();
             assert(f(2, 3) === before, 'same result after the flush');
             class C { #x = 4; m() { return this.#x; } }
             const c = new C();
             assert(c.m() === 4, 'before');
             $262.gc(); $262.gc(); $262.gc();
             assert(c.m() === 4, 'private name still resolves after the flush');",
        );
    }

    #[test]
    fn a_decoded_lazy_function_is_registered_with_the_collector() {
        let src = "function f(a) { const b = a; return b; }";
        let blob = crate::compile_snapshot(src).unwrap();
        let ast = decode(&blob, src).unwrap();
        let Stmt::FuncDecl(f) = &ast[0] else { panic!() };
        assert!(f.parsed_body().is_none());
        assert_eq!(f.body().len(), 2, "parsed on demand from the shared source");
        // The pass right after a read only clears the used flag; a body untouched for a whole
        // interval is released, and comes back on the next read.
        assert_eq!(crate::value::flush_cold_lazy_bodies(), 0);
        assert_eq!(crate::value::flush_cold_lazy_bodies(), 1);
        assert!(f.parsed_body().is_none());
        assert_eq!(f.body().len(), 2);
    }

    #[test]
    fn another_source_is_a_decode_error() {
        let src = "function f() { return 1; }";
        let blob = crate::compile_snapshot(src).unwrap();
        assert!(decode(&blob, "function f() { return 2; }").is_err());
        assert!(decode(&blob, "function f() { return 1; } ").is_err());
        assert!(decode(&blob, src).is_ok());
    }

    #[test]
    fn a_body_that_does_not_parse_is_a_compile_error() {
        assert!(crate::compile_snapshot("function f() { return 1 + ; }").is_err());
    }
}
