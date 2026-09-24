//! The parser's TypeScript mode: Node's strip-only `--experimental-strip-types` semantics
//! (amaro / swc's `ts_strip`), in the engine parser itself.
//!
//! In TypeScript mode the parser consumes the erasable TypeScript syntax (annotations, type
//! parameters and arguments, `as`/`satisfies`/`!`, modifiers, overloads, `declare`d and
//! type-only declarations, …) and records each piece as a byte range. The executable AST never
//! sees it. Once the parse succeeds the source is blanked in place — erased characters become
//! whitespace of the same UTF-8 *and* UTF-16 length, exactly as Node does (ASCII → space, 2-byte
//! → U+00A0, 3-byte → U+2002, 4-byte → space + U+FEFF), line terminators kept, `;` put where
//! erasing would join two statements — so every byte offset (function text, lazily parsed
//! bodies, stack positions) is the source's, and a body parsed later is plain JavaScript.
//!
//! Syntax that needs emitted JavaScript (`enum`, instantiated `namespace`, parameter
//! properties, `import x = require()`, `export =`, `<T>expr`, the `module` keyword) is rejected
//! with Node's message; [`error_code`] maps a message to its `ERR_*` code.
//!
//! The types themselves go to a [`SideTable`] keyed by span (never into the AST); see
//! `crate::typescript` for the lazily parsed view the typed tier consumes.

use super::*;
use crate::lexer::{tokenize_opts, LexError, LexOpts};

pub(crate) const MSG_ENUM: &str = "TypeScript enum is not supported in strip-only mode";
pub(crate) const MSG_NAMESPACE: &str =
    "TypeScript namespace declaration is not supported in strip-only mode";
pub(crate) const MSG_PARAM_PROP: &str =
    "TypeScript parameter property is not supported in strip-only mode";
pub(crate) const MSG_IMPORT_EQ: &str =
    "TypeScript import equals declaration is not supported in strip-only mode";
pub(crate) const MSG_EXPORT_ASSIGN: &str =
    "TypeScript export assignment is not supported in strip-only mode";
pub(crate) const MSG_MODULE: &str = "`module` keyword is not supported. Use `namespace` instead.";
pub(crate) const MSG_ANGLE: &str = "The angle-bracket syntax for type assertions, `<T>expr`, is not supported in type strip mode. Instead, use the 'as' syntax: `expr as T`.";

pub const UNSUPPORTED: &str = "ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX";
pub const INVALID: &str = "ERR_INVALID_TYPESCRIPT_SYNTAX";

/// Node's error code for a TypeScript-mode parse failure: the syntax that needs a transform is
/// `ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX`, anything else `ERR_INVALID_TYPESCRIPT_SYNTAX`.
pub fn error_code(message: &str) -> &'static str {
    if [
        MSG_ENUM,
        MSG_NAMESPACE,
        MSG_PARAM_PROP,
        MSG_IMPORT_EQ,
        MSG_EXPORT_ASSIGN,
        MSG_MODULE,
        MSG_ANGLE,
    ]
    .contains(&message)
    {
        UNSUPPORTED
    } else {
        INVALID
    }
}

/// A byte span `[start, end)`.
pub type Span = (u32, u32);

/// One parameter's annotation (see [`SideFn`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamTy {
    /// The binding's first byte.
    pub at: u32,
    pub optional: bool,
    pub rest: bool,
    pub has_default: bool,
    /// The type's text, as a span into [`SideTable::text`].
    pub ty: Option<Span>,
}

/// A function's signature types, keyed by the function's `FnSource::Range.start` (a class
/// constructor's is the class start, where its source begins).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SideFn {
    pub start: u32,
    pub type_params: Option<Span>,
    pub this: Option<Span>,
    pub params: Vec<ParamTy>,
    pub ret: Option<Span>,
}

/// The types (and JSDoc comments) of one parsed source, keyed by span, never stored in the
/// executable AST. TypeScript type text is copied into [`text`](SideTable::text) (the source
/// itself is blanked); JSDoc spans are into the source, whose comments are kept.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SideTable {
    /// Sorted by `start`.
    pub fns: Vec<SideFn>,
    /// `let`/`const`/`var` annotations: the binding's first byte and the type.
    pub vars: Vec<(u32, Span)>,
    /// Class field annotations: the field's first byte (its key) and the type.
    pub fields: Vec<(u32, Span)>,
    /// Every `/** … */` comment of the source (byte spans into it), in order. Which declaration
    /// one documents is decided on demand ([`SideTable::doc_before`]).
    pub docs: Vec<Span>,
    pub text: String,
}

impl SideTable {
    pub fn fn_at(&self, start: u32) -> Option<&SideFn> {
        self.fns
            .binary_search_by_key(&start, |f| f.start)
            .ok()
            .map(|i| &self.fns[i])
    }
    /// The text of a type span.
    pub fn ty(&self, s: Span) -> &str {
        self.text.get(s.0 as usize..s.1 as usize).unwrap_or("")
    }
    /// The JSDoc comment directly before the declaration at byte `at` of `src` (only
    /// whitespace, `export`, `default`, `declare`, `async` and class-member modifiers between).
    pub fn doc_before<'s>(&self, src: &'s str, at: u32) -> Option<&'s str> {
        let i = self.docs.partition_point(|d| d.1 <= at);
        let d = *self.docs.get(i.checked_sub(1)?)?;
        let gap = src.get(d.1 as usize..at as usize)?;
        let ok = gap.split_whitespace().all(|w| {
            matches!(
                w,
                "export"
                    | "default"
                    | "declare"
                    | "async"
                    | "static"
                    | "public"
                    | "private"
                    | "protected"
                    | "readonly"
                    | "override"
                    | "abstract"
                    | "let"
                    | "const"
                    | "var"
            )
        });
        ok.then(|| src.get(d.0 as usize..d.1 as usize)).flatten()
    }
}

thread_local! {
    /// Byte offset of the unsupported syntax the last TypeScript parse on this thread failed on.
    static ERROR_OFFSET: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
    static ERROR_END: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

pub(super) fn set_error_offset(at: Option<u32>) {
    ERROR_OFFSET.set(at);
    ERROR_END.set(at);
}

fn set_error_span(at: u32, end: u32) {
    ERROR_OFFSET.set(Some(at));
    ERROR_END.set(Some(end.max(at)));
}

/// The byte span `[start, end)` of what the last failed TypeScript parse reported: the whole
/// unsupported construct, or the offending token's start (an empty span) for invalid syntax.
pub fn error_span() -> Option<(u32, u32)> {
    let at = ERROR_OFFSET.get()?;
    Some((at, ERROR_END.get().unwrap_or(at)))
}

/// One parsed source's side-table entry.
struct SideEntry {
    /// The source's address (the key) and a weak handle, so a freed source's entry is stale.
    key: usize,
    weak: std::rc::Weak<str>,
    table: Rc<SideTable>,
    /// The TypeScript text before blanking (`typed` builds only; a JavaScript source is its own
    /// original), for the typed tier's lazy whole-file analysis.
    original: Option<Rc<str>>,
    /// That analysis, computed on first request (see `typescript::type_table`).
    cache: std::cell::RefCell<Option<Rc<dyn std::any::Any>>>,
}

thread_local! {
    static SIDE_TABLES: std::cell::RefCell<Vec<SideEntry>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

fn register_side_table(src: &Rc<str>, t: SideTable, original: Option<Rc<str>>) {
    let key = Rc::as_ptr(src) as *const u8 as usize;
    SIDE_TABLES.with(|s| {
        let mut s = s.borrow_mut();
        s.retain(|e| e.weak.strong_count() != 0 && e.key != key);
        s.push(SideEntry {
            key,
            weak: Rc::downgrade(src),
            table: Rc::new(t),
            original,
            cache: std::cell::RefCell::new(None),
        });
    });
}

/// The side table recorded when `src` (a parsed source, as a function's `FnSource::Range`
/// holds it) was parsed, if it had any types or JSDoc comments.
pub fn side_table_for(src: &Rc<str>) -> Option<Rc<SideTable>> {
    let key = Rc::as_ptr(src) as *const u8 as usize;
    SIDE_TABLES.with(|s| {
        s.borrow()
            .iter()
            .find(|e| e.key == key && e.weak.strong_count() != 0)
            .map(|e| e.table.clone())
    })
}

/// The cached per-source analysis for `src` (a source with a side table), computing it with
/// `make(original_text, is_typescript)` on first use. `None` when `src` has no entry or
/// `make` declines.
#[cfg_attr(not(feature = "typed"), allow(dead_code))]
pub(crate) fn side_analysis(
    src: &Rc<str>,
    make: impl FnOnce(&str, bool) -> Option<Rc<dyn std::any::Any>>,
) -> Option<Rc<dyn std::any::Any>> {
    let key = Rc::as_ptr(src) as *const u8 as usize;
    let (original, is_ts) = SIDE_TABLES.with(|s| {
        let s = s.borrow();
        let e = s
            .iter()
            .find(|e| e.key == key && e.weak.strong_count() != 0)?;
        if let Some(c) = e.cache.borrow().as_ref() {
            return Some((Err(c.clone()), false));
        }
        Some(match &e.original {
            Some(o) => (Ok(o.clone()), true),
            None => (Ok(src.clone()), false),
        })
    })?;
    let text = match original {
        Err(cached) => return Some(cached),
        Ok(t) => t,
    };
    // Computed outside the registry borrow: the analysis may parse (and register) sources.
    let made = make(&text, is_ts)?;
    SIDE_TABLES.with(|s| {
        if let Some(e) = s.borrow().iter().find(|e| e.key == key) {
            *e.cache.borrow_mut() = Some(made.clone());
        }
    });
    Some(made)
}

/// Record JSDoc comment spans (chars in the lexed text) for a JavaScript source.
pub(super) fn note_js_docs(src: &SrcMap, docs: &[(u32, u32)]) {
    if docs.is_empty() {
        return;
    }
    let docs = docs
        .iter()
        .filter_map(|&(s, e)| src.byte_range(s, e))
        .collect();
    register_side_table(
        &src.src,
        SideTable {
            docs,
            ..Default::default()
        },
        None,
    );
}

/// The parser's TypeScript state (present only in TypeScript mode).
#[derive(Default)]
pub(super) struct TsState {
    pub erase: Vec<Span>,
    pub semis: Vec<u32>,
    pub parens: Vec<u32>,
    /// (start, end, message): the construct's bytes (end set once it has been parsed).
    pub unsupported: Vec<(u32, u32, &'static str)>,
    /// `>`-led tokens split to close type arguments: (index, the original token).
    undo: Vec<(usize, Token)>,
    /// The last split: its token index (the token now holds the tail, in place, so a split
    /// never shifts the token vector) and the byte end of the `>` consumed from it.
    split_gt: Option<(usize, u32)>,
    /// Token index of the statement just recognized as TypeScript-only.
    type_only_at: usize,
    /// In a conditional's consequent: an arrow with a return type must be followed by `:`.
    no_ret_arrow: bool,
    disallow_cond: bool,
    /// Key of the function whose parameters are about to be parsed.
    fn_start: u32,
    /// `(close paren index, whether a return type followed)` of the last parameter list.
    last_params: (usize, bool),
    pub html: bool,
    pub soft_err: Option<LexError>,
    chars: Option<(u32, Vec<char>)>,
    // Side table (spans into the source until `finish`).
    fns: Vec<SideFn>,
    vars: Vec<(u32, Span)>,
    fields: Vec<(u32, Span)>,
    pub docs: Vec<Span>,
    /// Decorators before `export` (`@d export class C {}`), held for the exported class:
    /// (decorators, the class's start char, the `export` keyword's bytes).
    pending_decorators: Option<(Vec<Expr>, u32, Span)>,
    /// `@d export abstract class`: Node (swc) blanks the `export` keyword there and keeps
    /// `abstract`. Its `stripTypeScriptTypes` text does the same; the engine's own blanked
    /// source keeps the valid form. (`abstract` bytes, `export` bytes).
    swc_abstract_quirk: Vec<(Span, Span)>,
}

impl TsState {
    /// Sets whether an arrow with a return type must be followed by `:`; returns the old value.
    pub(super) fn set_no_ret_arrow(&mut self, v: bool) -> bool {
        std::mem::replace(&mut self.no_ret_arrow, v)
    }

    pub(super) fn new(html: bool) -> Box<TsState> {
        Box::new(TsState {
            type_only_at: usize::MAX,
            html,
            ..Default::default()
        })
    }
}

/// Everything a failed speculative parse must put back.
pub(super) struct Snap {
    pos: usize,
    undo: usize,
    erase: usize,
    semis: usize,
    parens: usize,
    unsupported: usize,
    fns: usize,
    vars: usize,
    fields: usize,
    no_ret_arrow: bool,
    disallow_cond: bool,
    last_params: (usize, bool),
    type_only_at: usize,
    split_gt: Option<(usize, u32)>,
    strict: bool,
    depth: u32,
    in_generator: bool,
    in_async: bool,
    in_params: bool,
    no_in: bool,
    last_for_await: bool,
    fn_depth: u32,
    nonarrow_fn_depth: u32,
    iter_depth: u32,
    switch_depth: u32,
    labels: Vec<String>,
    iter_labels: Vec<String>,
    decl: Vec<(usize, usize, usize)>,
    next_scope_is_fn_boundary: bool,
    allow_new_target: bool,
    top_level: bool,
    super_prop_ok: bool,
    super_call_ok: bool,
    in_derived_class: bool,
    in_case_clause: bool,
    no_arguments_refs: bool,
    proto_dups: usize,
    last_paren: bool,
    single_stmt: bool,
    in_static_block: bool,
    pending_private: usize,
    private_scope: Option<Rc<PrivateScope>>,
    in_field_init: bool,
    lazy: bool,
}

const MEMBER_MODIFIERS: &[&str] = &[
    "public",
    "private",
    "protected",
    "readonly",
    "abstract",
    "override",
    "declare",
    "static",
    "accessor",
];

/// Punctuators that can begin an expression.
fn starts_expr_punct(p: &str) -> bool {
    matches!(
        p,
        "(" | "[" | "{" | "+" | "-" | "~" | "!" | "++" | "--" | "<" | "/" | "/=" | "@" | "..."
    )
}

fn is_binary_punct(p: &str) -> bool {
    matches!(
        p,
        "??" | "||"
            | "&&"
            | "|"
            | "^"
            | "&"
            | "=="
            | "!="
            | "==="
            | "!=="
            | "<"
            | ">"
            | "<="
            | ">="
            | "<<"
            | ">>"
            | ">>>"
            | "+"
            | "-"
            | "*"
            | "/"
            | "%"
            | "**"
    )
}

/// Blanks `src[start..end]` into `out` (see the module docs).
fn blank(out: &mut [u8], src: &str, start: usize, end: usize) {
    let end = end.min(src.len());
    let Some(text) = src.get(start..end) else {
        return;
    };
    for (off, ch) in text.char_indices() {
        let i = start + off;
        let fill: &[u8] = match ch {
            c if (c.is_whitespace() && c != '\u{85}') || c == '\u{feff}' => continue,
            c if c.is_ascii() => b" ",
            c => match c.len_utf8() {
                2 => "\u{a0}".as_bytes(),
                3 => "\u{2002}".as_bytes(),
                _ => " \u{feff}".as_bytes(),
            },
        };
        out[i..i + fill.len()].copy_from_slice(fill);
    }
}

/// Writes the ASCII byte `b` at `at`, padding what remains of a blanked multi-byte character.
fn put(out: &mut [u8], at: usize, b: u8) {
    if at < out.len() {
        out[at] = b;
        let mut k = at + 1;
        while k < out.len() && (out[k] & 0xC0) == 0x80 {
            out[k] = b' ';
            k += 1;
        }
    }
}

/// The stripped text of `src` for a finished TypeScript parse.
pub(super) fn stripped(src: &str, st: &TsState) -> Vec<u8> {
    let mut out = src.as_bytes().to_vec();
    for &(s, e) in &st.erase {
        blank(&mut out, src, s as usize, e as usize);
    }
    // A `;` only matters where erased text meets live code: one inside a larger erased region
    // stays blank.
    let mut ranges = st.erase.clone();
    ranges.sort_unstable();
    for &at in &st.semis {
        let covered = ranges
            .iter()
            .take_while(|r| r.0 < at)
            .any(|&(_, e)| e > at + 1);
        if !covered {
            put(&mut out, at as usize, b';');
        }
    }
    for &at in &st.parens {
        put(&mut out, at as usize, b')');
    }
    out
}

/// A line number (1-based) for a byte offset.
fn line_of(src: &str, at: u32) -> u32 {
    let head = src.get(..at as usize).unwrap_or(src);
    1 + head
        .chars()
        .filter(|&c| matches!(c, '\n' | '\u{2028}' | '\u{2029}') || c == '\r')
        .count() as u32
        - head.matches("\r\n").count() as u32
}

impl Parser {
    // ----- plumbing ----------------------------------------------------------------------------

    fn tss(&mut self) -> &mut TsState {
        self.ts.as_mut().expect("TypeScript mode")
    }
    /// Byte span of token `idx`.
    fn tb(&self, idx: usize) -> Span {
        match self.toks.get(idx) {
            Some(t) => self.src.byte_range(t.start, t.end).unwrap_or((0, 0)),
            None => (0, 0),
        }
    }
    fn cur_b(&self) -> u32 {
        self.tb(self.pos).0
    }
    fn prev_end_b(&self) -> u32 {
        // Right after a split `>`, the previous token is the `>` inside the current token.
        if let Some((idx, end)) = self.ts.as_ref().and_then(|t| t.split_gt) {
            if idx == self.pos {
                return end;
            }
        }
        if self.pos == 0 {
            self.cur_b()
        } else {
            self.tb(self.pos - 1).1
        }
    }
    fn erase(&mut self, start: u32, end: u32) {
        if end > start {
            self.tss().erase.push((start, end));
        }
    }
    fn erase_from(&mut self, start: u32) {
        let end = self.prev_end_b();
        self.erase(start, end);
    }
    /// Advance over the current token, erasing it.
    fn erase_tok(&mut self) {
        let (s, e) = self.tb(self.pos);
        self.erase(s, e);
        self.advance();
    }
    /// A parse error in TypeScript mode: remember where (for Node's code frame).
    pub(super) fn ts_note_error(&self) {
        let at = if self.pos < self.toks.len() {
            self.cur_b()
        } else {
            self.src.src.len() as u32
        };
        set_error_offset(Some(at));
    }

    fn unsupported(&mut self, at: u32, msg: &'static str) {
        self.tss().unsupported.push((at, at, msg));
    }
    /// End the span of the last unsupported construct recorded at `at` here.
    fn unsupported_end(&mut self, at: u32) {
        let end = self.prev_end_b();
        self.unsupported_end_at(at, end);
    }
    fn unsupported_end_at(&mut self, at: u32, end: u32) {
        if let Some(u) = self.tss().unsupported.iter_mut().rev().find(|u| u.0 == at) {
            u.1 = end;
        }
    }
    fn mark_type_only(&mut self, idx: usize) {
        self.tss().type_only_at = idx;
    }
    fn take_stmt_flags(&mut self) {
        self.top_level = false;
        self.single_stmt = false;
        self.in_case_clause = false;
    }
    /// The current token is the word `w` (an identifier or a reserved word).
    fn cw(&self, w: &str) -> bool {
        self.pw(0, w)
    }
    fn pw(&self, k: usize, w: &str) -> bool {
        match self.toks.get(self.pos + k).map(|t| &t.kind) {
            Some(Tok::Ident(x)) => x == w,
            Some(Tok::Keyword(x)) => *x == w,
            _ => false,
        }
    }
    fn pp(&self, k: usize, p: &str) -> bool {
        matches!(self.toks.get(self.pos + k).map(|t| &t.kind), Some(Tok::Punct(x)) if *x == p)
    }
    fn ident_at(&self, k: usize) -> bool {
        matches!(
            self.toks.get(self.pos + k).map(|t| &t.kind),
            Some(Tok::Ident(n)) if !n.starts_with('#')
        )
    }
    fn tok_at(&self, k: usize) -> &Tok {
        self.toks
            .get(self.pos + k)
            .map(|t| &t.kind)
            .unwrap_or(&Tok::Eof)
    }
    fn expect_ident_any(&mut self) -> Result<(), ParseError> {
        match self.cur() {
            Tok::Ident(_) | Tok::Keyword(_) => {
                self.advance();
                Ok(())
            }
            _ => self.err("Identifier expected"),
        }
    }
    fn member_name_ends(&self, k: usize) -> bool {
        matches!(
            self.tok_at(k),
            Tok::Punct("(" | "=" | ";" | ":" | "?" | "!" | "}" | "<") | Tok::Eof
        ) || self.nl_at(k)
    }

    pub(super) fn ts_snapshot(&self) -> Snap {
        let t = self.ts.as_ref().expect("TypeScript mode");
        Snap {
            pos: self.pos,
            undo: t.undo.len(),
            erase: t.erase.len(),
            semis: t.semis.len(),
            parens: t.parens.len(),
            unsupported: t.unsupported.len(),
            fns: t.fns.len(),
            vars: t.vars.len(),
            fields: t.fields.len(),
            no_ret_arrow: t.no_ret_arrow,
            disallow_cond: t.disallow_cond,
            last_params: t.last_params,
            type_only_at: t.type_only_at,
            split_gt: t.split_gt,
            strict: self.strict,
            depth: self.depth,
            in_generator: self.in_generator,
            in_async: self.in_async,
            in_params: self.in_params,
            no_in: self.no_in,
            last_for_await: self.last_for_await,
            fn_depth: self.fn_depth,
            nonarrow_fn_depth: self.nonarrow_fn_depth,
            iter_depth: self.iter_depth,
            switch_depth: self.switch_depth,
            labels: self.labels.clone(),
            iter_labels: self.iter_labels.clone(),
            decl: self
                .decl_scopes
                .iter()
                .map(|s| (s.lexical.len(), s.var.len(), s.fn_lexical.len()))
                .collect(),
            next_scope_is_fn_boundary: self.next_scope_is_fn_boundary,
            allow_new_target: self.allow_new_target,
            top_level: self.top_level,
            super_prop_ok: self.super_prop_ok,
            super_call_ok: self.super_call_ok,
            in_derived_class: self.in_derived_class,
            in_case_clause: self.in_case_clause,
            no_arguments_refs: self.no_arguments_refs,
            proto_dups: self.proto_dups.len(),
            last_paren: self.last_paren,
            single_stmt: self.single_stmt,
            in_static_block: self.in_static_block,
            pending_private: self.pending_private.len(),
            private_scope: self.private_scope.clone(),
            in_field_init: self.in_field_init,
            lazy: self.lazy,
        }
    }

    pub(super) fn ts_restore(&mut self, s: Snap) {
        let Some(t) = self.ts.as_mut() else {
            return;
        };
        while t.undo.len() > s.undo {
            let (idx, tok) = t.undo.pop().expect("undo entry");
            self.toks[idx] = tok;
        }
        t.split_gt = s.split_gt;
        t.erase.truncate(s.erase);
        t.semis.truncate(s.semis);
        t.parens.truncate(s.parens);
        t.unsupported.truncate(s.unsupported);
        t.fns.truncate(s.fns);
        t.vars.truncate(s.vars);
        t.fields.truncate(s.fields);
        t.no_ret_arrow = s.no_ret_arrow;
        t.disallow_cond = s.disallow_cond;
        t.last_params = s.last_params;
        t.type_only_at = s.type_only_at;
        self.pos = s.pos;
        self.strict = s.strict;
        self.depth = s.depth;
        self.in_generator = s.in_generator;
        self.in_async = s.in_async;
        self.in_params = s.in_params;
        self.no_in = s.no_in;
        self.last_for_await = s.last_for_await;
        self.fn_depth = s.fn_depth;
        self.nonarrow_fn_depth = s.nonarrow_fn_depth;
        self.iter_depth = s.iter_depth;
        self.switch_depth = s.switch_depth;
        self.labels = s.labels;
        self.iter_labels = s.iter_labels;
        self.decl_scopes.truncate(s.decl.len());
        for (scope, (l, v, f)) in self.decl_scopes.iter_mut().zip(s.decl) {
            scope.lexical.truncate(l);
            scope.var.truncate(v);
            scope.fn_lexical.truncate(f);
        }
        self.next_scope_is_fn_boundary = s.next_scope_is_fn_boundary;
        self.allow_new_target = s.allow_new_target;
        self.top_level = s.top_level;
        self.super_prop_ok = s.super_prop_ok;
        self.super_call_ok = s.super_call_ok;
        self.in_derived_class = s.in_derived_class;
        self.in_case_clause = s.in_case_clause;
        self.no_arguments_refs = s.no_arguments_refs;
        self.proto_dups.truncate(s.proto_dups);
        self.last_paren = s.last_paren;
        self.single_stmt = s.single_stmt;
        self.in_static_block = s.in_static_block;
        self.pending_private.truncate(s.pending_private);
        self.private_scope = s.private_scope;
        self.in_field_init = s.in_field_init;
        self.lazy = s.lazy;
    }

    /// Re-tokenize from the current token with a leading `/` read as a regex (`regex`) or a
    /// division: the lexer's guess from the previous token can be wrong after TypeScript syntax
    /// (`f(): T {}` then a regex, `x!` then a division), and the parser knows which it needs.
    pub(super) fn ts_relex(&mut self, regex: bool) -> Result<(), ParseError> {
        let (c, line, nl) = {
            let t = &self.toks[self.pos];
            (t.start as usize, t.line, t.nl_before)
        };
        let (base, end) = (self.src.base, self.src.end);
        let cached = matches!(&self.tss().chars, Some((b, _)) if *b == base);
        if !cached {
            let chars: Vec<char> = self.src.src[base as usize..end as usize].chars().collect();
            self.tss().chars = Some((base, chars));
        }
        let html = self.tss().html;
        let lexed = {
            let chars = &self
                .ts
                .as_ref()
                .expect("ts")
                .chars
                .as_ref()
                .expect("chars")
                .1;
            tokenize_opts(
                chars.get(c..).unwrap_or(&[]),
                html,
                line,
                LexOpts {
                    ts: true,
                    docs: false,
                    start_div: !regex,
                },
            )
        }
        .map_err(|e| ParseError {
            message: e.message,
            line: e.line,
            at_eof: e.at_eof,
        })?;
        let mut toks = lexed.tokens;
        for t in &mut toks {
            t.start += c as u32;
            t.end += c as u32;
        }
        if let Some(t) = toks.first_mut() {
            t.nl_before = nl;
        }
        self.toks.truncate(self.pos);
        self.toks.extend(toks);
        self.tss().soft_err = lexed.soft_err;
        Ok(())
    }

    /// Consume a `>` closing type arguments/parameters, splitting a `>>`/`>=`/… token.
    fn ts_eat_gt(&mut self) -> bool {
        let rest = match self.cur() {
            Tok::Punct(">") => {
                self.advance();
                return true;
            }
            Tok::Punct(">>") => ">",
            Tok::Punct(">=") => "=",
            Tok::Punct(">>=") => ">=",
            Tok::Punct(">>>") => ">>",
            Tok::Punct(">>>=") => ">>=",
            _ => return false,
        };
        // Split in place: the token becomes its tail and stays current (inserting a token
        // would shift the whole vector, quadratic over `A<B<C>>>`-heavy declaration files).
        let idx = self.pos;
        let gt_end = self.tb(idx).0 + 1;
        let orig = self.toks[idx].clone();
        let tail = &mut self.toks[idx];
        tail.kind = Tok::Punct(rest);
        tail.start += 1;
        tail.nl_before = false;
        let t = self.tss();
        t.undo.push((idx, orig));
        t.split_gt = Some((idx, gt_end));
        true
    }
    fn ts_expect_gt(&mut self) -> Result<(), ParseError> {
        if self.ts_eat_gt() {
            Ok(())
        } else {
            self.err("'>' expected")
        }
    }

    // ----- types (skipped; their spans are the record) -----------------------------------------

    /// A type; returns its byte span.
    pub(super) fn ts_type(&mut self) -> Result<Span, ParseError> {
        let start = self.cur_b();
        self.ty_cond(true)?;
        Ok((start, self.prev_end_b()))
    }

    fn ty_cond(&mut self, allow_cond: bool) -> Result<(), ParseError> {
        let saved = self.tss().disallow_cond;
        self.tss().disallow_cond = !allow_cond;
        let r = self.ty_cond_inner(allow_cond);
        self.tss().disallow_cond = saved;
        r
    }

    fn ty_cond_inner(&mut self, allow_cond: bool) -> Result<(), ParseError> {
        if self.is_punct("<") {
            self.ts_type_params_list()?;
            return self.fn_type();
        }
        if self.cw("new") || (self.cw("abstract") && self.pw(1, "new")) {
            if self.cw("abstract") {
                self.advance();
            }
            self.advance();
            if self.is_punct("<") {
                self.ts_type_params_list()?;
            }
            return self.fn_type();
        }
        if self.is_punct("(") && self.is_fn_type_start() {
            return self.fn_type();
        }
        self.union_ty()?;
        if allow_cond && self.cw("extends") && !self.nl_before() {
            self.advance();
            self.ty_cond(false)?;
            self.expect_punct("?")?;
            self.ty_cond(true)?;
            self.expect_punct(":")?;
            self.ty_cond(true)?;
        }
        Ok(())
    }

    fn is_fn_type_start(&self) -> bool {
        if matches!(self.tok_at(1), Tok::Punct(")" | "...")) {
            return true;
        }
        let mut k = 1;
        while matches!(self.tok_at(k), Tok::Ident(w) if matches!(w.as_str(), "public" | "private" | "protected" | "readonly"))
            && matches!(self.tok_at(k + 1), Tok::Ident(_) | Tok::Punct("[" | "{"))
        {
            k += 1;
        }
        match self.tok_at(k) {
            Tok::Ident(_) | Tok::Keyword("this") => k += 1,
            Tok::Punct("[" | "{") => match self.matching_paren(self.pos + k) {
                Some(close) => k = close + 1 - self.pos,
                None => return false,
            },
            _ => return false,
        }
        match self.tok_at(k) {
            Tok::Punct(":" | "," | "?" | "=") => true,
            Tok::Punct(")") => self.pp(k + 1, "=>"),
            _ => false,
        }
    }

    /// `( params ) => Ret` of a function type (the parameters are skipped as tokens).
    fn fn_type(&mut self) -> Result<(), ParseError> {
        if !self.is_punct("(") {
            return self.err("'(' expected");
        }
        let close = self
            .matching_paren(self.pos)
            .map_or_else(|| self.err("')' expected"), Ok)?;
        self.pos = close + 1;
        self.expect_punct("=>")?;
        self.ts_return_type_inner()
    }

    fn union_ty(&mut self) -> Result<(), ParseError> {
        self.eat_punct("|");
        self.intersection_ty()?;
        while self.eat_punct("|") {
            self.intersection_ty()?;
        }
        Ok(())
    }

    fn intersection_ty(&mut self) -> Result<(), ParseError> {
        self.eat_punct("&");
        self.operator_ty()?;
        while self.eat_punct("&") {
            self.operator_ty()?;
        }
        Ok(())
    }

    fn operator_ty(&mut self) -> Result<(), ParseError> {
        if self.cw("keyof")
            && !matches!(
                self.tok_at(1),
                Tok::Punct(")" | "]" | "," | ">" | ";" | "=" | "|" | "&" | "}" | "?" | ":" | ".")
                    | Tok::Eof
            )
        {
            self.advance();
            return self.operator_ty();
        }
        if self.cw("unique") && self.pw(1, "symbol") {
            self.advance();
            self.advance();
            return Ok(());
        }
        if self.cw("readonly")
            && matches!(
                self.tok_at(1),
                Tok::Ident(_) | Tok::Keyword(_) | Tok::Punct("(" | "[" | "{")
            )
        {
            self.advance();
            return self.operator_ty();
        }
        if self.cw("infer") && self.ident_at(1) {
            self.advance();
            self.advance();
            if self.cw("extends") {
                let outer_disallow = self.tss().disallow_cond;
                let snap = self.ts_snapshot();
                let r = (|| {
                    self.advance();
                    self.ty_cond(false)?;
                    if !outer_disallow && self.is_punct("?") {
                        return self.err("conditional");
                    }
                    Ok(())
                })();
                if r.is_err() {
                    self.ts_restore(snap);
                }
            }
            return Ok(());
        }
        self.postfix_ty()
    }

    fn postfix_ty(&mut self) -> Result<(), ParseError> {
        self.primary_ty()?;
        while self.is_punct("[") && !self.nl_before() {
            self.advance();
            if !self.eat_punct("]") {
                self.ty_cond(true)?;
                self.expect_punct("]")?;
            }
        }
        Ok(())
    }

    fn primary_ty(&mut self) -> Result<(), ParseError> {
        match self.cur().clone() {
            Tok::Str(_) | Tok::Num(_) | Tok::BigInt(_) | Tok::Template(_) => {
                self.advance();
                Ok(())
            }
            Tok::Punct("-") => {
                self.advance();
                match self.cur() {
                    Tok::Num(_) | Tok::BigInt(_) => {
                        self.advance();
                        Ok(())
                    }
                    _ => self.err("Number expected"),
                }
            }
            Tok::Punct("{") => self.object_type(),
            Tok::Punct("[") => self.tuple_type(),
            Tok::Punct("(") => {
                self.advance();
                self.ty_cond(true)?;
                self.expect_punct(")")
            }
            Tok::Keyword("typeof") => {
                self.advance();
                if self.cw("import") {
                    return self.import_type();
                }
                self.expect_ident_any()?;
                while self.eat_punct(".") {
                    self.expect_ident_any()?;
                }
                if self.is_punct("<") && !self.nl_before() {
                    self.ts_type_args()?;
                }
                Ok(())
            }
            Tok::Keyword("import") => self.import_type(),
            Tok::Keyword("void" | "null" | "this" | "true" | "false") => {
                self.advance();
                Ok(())
            }
            Tok::Ident(n) if !n.starts_with('#') => {
                self.advance();
                while self.is_punct(".") {
                    self.advance();
                    self.expect_ident_any()?;
                }
                if self.is_punct("<") && !self.nl_before() {
                    self.ts_type_args()?;
                }
                Ok(())
            }
            _ => self.err("Type expected"),
        }
    }

    fn import_type(&mut self) -> Result<(), ParseError> {
        self.advance(); // import
        self.expect_punct("(")?;
        let close = self
            .matching_paren(self.pos - 1)
            .map_or_else(|| self.err("')' expected"), Ok)?;
        self.pos = close + 1;
        while self.eat_punct(".") {
            self.expect_ident_any()?;
        }
        if self.is_punct("<") {
            self.ts_type_args()?;
        }
        Ok(())
    }

    fn tuple_type(&mut self) -> Result<(), ParseError> {
        self.expect_punct("[")?;
        while !self.eat_punct("]") {
            self.eat_punct("...");
            if matches!(self.cur(), Tok::Ident(_) | Tok::Keyword(_))
                && (self.pp(1, ":") || (self.pp(1, "?") && self.pp(2, ":")))
            {
                self.advance();
                self.eat_punct("?");
                self.advance();
                self.ty_cond(true)?;
            } else {
                self.ty_cond(true)?;
                self.eat_punct("?");
            }
            if !self.eat_punct(",") {
                self.expect_punct("]")?;
                break;
            }
        }
        Ok(())
    }

    fn type_member_sep(&mut self) -> Result<(), ParseError> {
        if self.eat_punct(";") || self.eat_punct(",") || self.is_punct("}") || self.nl_before() {
            Ok(())
        } else {
            self.err("';' expected")
        }
    }

    /// A signature's `(params)` (as tokens) and optional `: Ret`.
    fn sig_rest(&mut self) -> Result<(), ParseError> {
        if self.is_punct("<") {
            self.ts_type_params_list()?;
        }
        if !self.is_punct("(") {
            return self.err("'(' expected");
        }
        let close = self
            .matching_paren(self.pos)
            .map_or_else(|| self.err("')' expected"), Ok)?;
        self.pos = close + 1;
        if self.eat_punct(":") {
            self.ts_return_type_inner()?;
        }
        Ok(())
    }

    fn object_type(&mut self) -> Result<(), ParseError> {
        let open = self.pos;
        self.expect_punct("{")?;
        // Mapped type: `{ [K in T]: U }`.
        let mut k = 0;
        while matches!(self.tok_at(k), Tok::Punct("+" | "-")) || self.pw(k, "readonly") {
            k += 1;
        }
        if self.pp(k, "[") && self.ident_at(k + 1) && self.pw(k + 2, "in") {
            let close = self
                .matching_brace(open)
                .map_or_else(|| self.err("'}' expected"), Ok)?;
            self.pos = close + 1;
            return Ok(());
        }
        while !self.eat_punct("}") {
            if self.at_eof() {
                return self.err("'}' expected");
            }
            if self.cw("readonly") && !self.member_name_ends(1) {
                self.advance();
            }
            if self.is_punct("[") && self.ident_at(1) && self.pp(2, ":") {
                self.advance();
                self.advance();
                self.advance();
                self.ty_cond(true)?;
                self.expect_punct("]")?;
                if self.eat_punct(":") {
                    self.ty_cond(true)?;
                }
                self.type_member_sep()?;
                continue;
            }
            if self.is_punct("(") || self.is_punct("<") {
                self.sig_rest()?;
                self.type_member_sep()?;
                continue;
            }
            if self.cw("new") && (self.pp(1, "(") || self.pp(1, "<")) {
                self.advance();
                self.sig_rest()?;
                self.type_member_sep()?;
                continue;
            }
            if (self.cw("get") || self.cw("set")) && !self.member_name_ends(1) {
                self.advance();
            }
            self.ts_prop_key()?;
            self.eat_punct("?");
            if self.is_punct("(") || self.is_punct("<") {
                self.sig_rest()?;
            } else if self.eat_punct(":") {
                self.ty_cond(true)?;
            }
            self.type_member_sep()?;
        }
        Ok(())
    }

    /// A property key in a type (a computed one skipped as tokens).
    fn ts_prop_key(&mut self) -> Result<(), ParseError> {
        match self.cur() {
            Tok::Ident(_) | Tok::Keyword(_) | Tok::Str(_) | Tok::Num(_) | Tok::BigInt(_) => {
                self.advance();
                Ok(())
            }
            Tok::Punct("[") => {
                let close = self
                    .matching_paren(self.pos)
                    .map_or_else(|| self.err("']' expected"), Ok)?;
                self.pos = close + 1;
                Ok(())
            }
            _ => self.err("Property name expected"),
        }
    }

    /// `<T, U>` type arguments, erased.
    pub(super) fn ts_type_args(&mut self) -> Result<(), ParseError> {
        let start = self.cur_b();
        self.expect_punct("<")?;
        if !self.ts_eat_gt() {
            loop {
                self.ty_cond(true)?;
                if !self.eat_punct(",") {
                    self.ts_expect_gt()?;
                    break;
                }
                if self.ts_eat_gt() {
                    break;
                }
            }
        }
        self.erase_from(start);
        Ok(())
    }

    fn ts_type_params_list(&mut self) -> Result<(), ParseError> {
        self.expect_punct("<")?;
        while !self.ts_eat_gt() {
            while (self.cw("const") || self.cw("in") || self.cw("out")) && self.ident_at(1) {
                self.advance();
            }
            match self.cur() {
                Tok::Ident(_) => {
                    self.advance();
                }
                _ => return self.err("Type parameter declaration expected"),
            }
            if self.cw("extends") {
                self.advance();
                self.ty_cond(true)?;
            }
            if self.eat_punct("=") {
                self.ty_cond(true)?;
            }
            if !self.eat_punct(",") {
                self.ts_expect_gt()?;
                break;
            }
        }
        Ok(())
    }

    /// `<T extends U = V, …>` type parameters, erased; returns the span.
    pub(super) fn ts_type_params(&mut self) -> Result<Span, ParseError> {
        let start = self.cur_b();
        self.ts_type_params_list()?;
        self.erase_from(start);
        Ok((start, self.prev_end_b()))
    }

    fn ts_return_type_inner(&mut self) -> Result<(), ParseError> {
        let param_like =
            |p: &Self, k: usize| p.ident_at(k) || matches!(p.tok_at(k), Tok::Keyword("this"));
        if self.cw("asserts") && param_like(self, 1) && !self.nl_at(1) {
            self.advance();
            self.advance();
            if self.cw("is") {
                self.advance();
                self.ty_cond(true)?;
            }
            return Ok(());
        }
        if param_like(self, 0) && self.pw(1, "is") && !self.nl_at(1) {
            self.advance();
            self.advance();
            return self.ty_cond(true);
        }
        self.ty_cond(true)
    }

    // ----- statements --------------------------------------------------------------------------

    /// `parse_stmt` in TypeScript mode: TypeScript-only statements are erased whole, with Node's
    /// `;` where the code around them would otherwise join.
    pub(super) fn ts_parse_stmt(&mut self) -> Result<Stmt, ParseError> {
        if self.is_punct("@") && self.top_level && self.module {
            if let Some(s) = self.ts_decorated_export()? {
                return Ok(s);
            }
        }
        let idx = self.pos;
        let start = self.cur_b();
        let single = self.single_stmt;
        let s = match self.ts_stmt(idx)? {
            Some(s) => s,
            None => self.parse_stmt_js()?,
        };
        if self.tss().type_only_at != idx {
            return Ok(s);
        }
        self.tss().type_only_at = usize::MAX;
        self.erase_from(start);
        if single {
            self.tss().semis.push(start);
        } else if idx > 0 && self.ts_asi_hazard() {
            let (prev_start, _) = self.tb(idx - 1);
            if !matches!(self.toks[idx - 1].kind, Tok::Punct(";")) {
                self.tss().semis.push(start);
            } else if self
                .tss()
                .erase
                .iter()
                .any(|&(s, e)| s <= prev_start && prev_start < e)
            {
                // The `;` ending an erased statement before this one: Node keeps that one.
                self.tss().semis.push(prev_start);
            }
        }
        Ok(Stmt::Empty)
    }

    fn ts_asi_hazard(&self) -> bool {
        matches!(
            self.cur(),
            Tok::Punct("(" | "[" | "+" | "-" | "/" | "/=") | Tok::Regex(_) | Tok::Template(_)
        )
    }

    /// Which TypeScript declaration starts at token `pos + k`, if any.
    fn ts_decl_at(&self, k: usize) -> Option<&'static str> {
        let same_line = !self.nl_at(k + 1);
        match self.tok_at(k) {
            Tok::Ident(w) => match w.as_str() {
                "type" if same_line && self.ident_at(k + 1) => Some("type"),
                "interface" if same_line && self.ident_at(k + 1) => Some("interface"),
                "declare"
                    if same_line
                        && matches!(self.tok_at(k + 1), Tok::Ident(_) | Tok::Keyword(_)) =>
                {
                    Some("declare")
                }
                "abstract" if same_line && matches!(self.tok_at(k + 1), Tok::Keyword("class")) => {
                    Some("abstract")
                }
                "namespace" | "module"
                    if same_line
                        && (self.ident_at(k + 1) || matches!(self.tok_at(k + 1), Tok::Str(_))) =>
                {
                    Some("namespace")
                }
                _ => None,
            },
            Tok::Keyword("enum") if self.ident_at(k + 1) => Some("enum"),
            Tok::Keyword("const") if self.pw(k + 1, "enum") && self.ident_at(k + 2) => {
                Some("const enum")
            }
            _ => None,
        }
    }

    /// A TypeScript-only statement at the cursor (the parser's other hooks handle TypeScript
    /// inside JavaScript statements). `None`: parse it as JavaScript.
    fn ts_stmt(&mut self, idx: usize) -> Result<Option<Stmt>, ParseError> {
        if let Some(kind) = self.ts_decl_at(0) {
            if kind == "abstract" {
                self.erase_tok();
                return Ok(None);
            }
            self.take_stmt_flags();
            self.ts_decl(kind, idx)?;
            return Ok(Some(Stmt::Empty));
        }
        match self.cur().clone() {
            Tok::Keyword("import") if !matches!(self.tok_at(1), Tok::Punct("(" | ".")) => {
                self.ts_import(idx)
            }
            Tok::Keyword("export") => self.ts_export(idx),
            Tok::Keyword("function") => self.ts_overload(idx),
            Tok::Ident(w)
                if w == "async"
                    && matches!(self.tok_at(1), Tok::Keyword("function"))
                    && !self.nl_at(1) =>
            {
                self.ts_overload(idx)
            }
            _ => Ok(None),
        }
    }

    /// A declaration found by [`ts_decl_at`] at the cursor (not `abstract`).
    fn ts_decl(&mut self, kind: &str, idx: usize) -> Result<(), ParseError> {
        let start = self.cur_b();
        match kind {
            "type" => {
                self.ts_type_alias()?;
                self.mark_type_only(idx);
            }
            "interface" => {
                self.ts_interface()?;
                self.mark_type_only(idx);
            }
            "declare" => {
                self.ts_declare()?;
                self.mark_type_only(idx);
            }
            "namespace" => {
                if !self.ts_namespace(false)? {
                    self.mark_type_only(idx);
                }
            }
            "enum" => self.ts_enum(Some(start))?,
            "const enum" => {
                self.advance();
                self.ts_enum(Some(start))?;
            }
            _ => {}
        }
        Ok(())
    }

    fn ts_type_alias(&mut self) -> Result<(), ParseError> {
        self.advance(); // type
        self.expect_ident_any()?;
        if self.is_punct("<") {
            self.ts_type_params_list()?;
        }
        self.expect_punct("=")?;
        self.ty_cond(true)?;
        self.consume_semicolon()
    }

    fn ts_interface(&mut self) -> Result<(), ParseError> {
        self.advance(); // interface
        self.expect_ident_any()?;
        if self.is_punct("<") {
            self.ts_type_params_list()?;
        }
        if self.cw("extends") {
            self.advance();
            loop {
                self.ts_type_reference()?;
                if !self.eat_punct(",") {
                    break;
                }
            }
        }
        self.object_type()
    }

    fn ts_type_reference(&mut self) -> Result<(), ParseError> {
        self.expect_ident_any()?;
        while self.eat_punct(".") {
            self.expect_ident_any()?;
        }
        if self.is_punct("<") {
            self.ts_type_args()?;
        }
        Ok(())
    }

    /// `enum E { … }` (the cursor at `enum`): unsupported unless ambient (`at` is `None`).
    fn ts_enum(&mut self, at: Option<u32>) -> Result<(), ParseError> {
        if let Some(at) = at {
            self.unsupported(at, MSG_ENUM);
        }
        self.advance(); // enum
        self.expect_ident_any()?;
        self.skip_braces()?;
        if let Some(at) = at {
            self.unsupported_end(at);
        }
        Ok(())
    }

    fn skip_braces(&mut self) -> Result<(), ParseError> {
        if !self.is_punct("{") {
            return self.err("'{' expected");
        }
        let close = self
            .matching_brace(self.pos)
            .map_or_else(|| self.err("'}' expected"), Ok)?;
        self.pos = close + 1;
        Ok(())
    }

    /// `namespace N { … }` / `module …` at the cursor; returns whether it is instantiated
    /// (holds values, so it would need emitted code).
    fn ts_namespace(&mut self, declare: bool) -> Result<bool, ParseError> {
        let start = self.cur_b();
        let global = self.cw("global");
        let module_kw = self.cw("module");
        if !global {
            self.advance();
        }
        let string_name = matches!(self.cur(), Tok::Str(_));
        if string_name || global {
            self.advance();
        } else {
            self.expect_ident_any()?;
            while self.eat_punct(".") {
                self.expect_ident_any()?;
            }
        }
        let mut concrete = false;
        if self.is_punct("{") {
            if declare {
                self.skip_braces()?;
            } else {
                let open = self.pos;
                self.advance();
                while !self.is_punct("}") && !self.at_eof() {
                    let k = usize::from(matches!(self.cur(), Tok::Keyword("export")));
                    match self.ts_decl_at(k) {
                        Some(d @ ("type" | "interface")) => {
                            self.pos += k;
                            if d == "type" {
                                self.ts_type_alias()?;
                            } else {
                                self.ts_interface()?;
                            }
                        }
                        Some("namespace") => {
                            self.pos += k;
                            concrete |= self.ts_namespace(false)?;
                        }
                        _ => {
                            concrete = true;
                            break;
                        }
                    }
                }
                if concrete {
                    let close = self
                        .matching_brace(open)
                        .map_or_else(|| self.err("'}' expected"), Ok)?;
                    self.pos = close + 1;
                } else {
                    self.expect_punct("}")?;
                }
            }
        } else {
            self.consume_semicolon()?;
        }
        if module_kw && !string_name {
            // Node's span: `module` and the first name.
            self.unsupported(start, MSG_MODULE);
            let src = &self.src.src;
            let mut end = start as usize + "module".len();
            end += src[end..].len() - src[end..].trim_start().len();
            end += src[end..]
                .find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .unwrap_or(src.len() - end);
            self.unsupported_end_at(start, end as u32);
        } else if !declare && !global && concrete {
            self.unsupported(start, MSG_NAMESPACE);
            self.unsupported_end(start);
        }
        Ok(concrete)
    }

    /// `declare …` (the cursor at `declare`): skipped whole.
    fn ts_declare(&mut self) -> Result<(), ParseError> {
        self.advance(); // declare
        match self.cur().clone() {
            Tok::Keyword("var" | "const") | Tok::Ident(_)
                if self.cw("var") || self.cw("const") || self.cw("let") || self.cw("using") =>
            {
                if self.cw("const") && self.pw(1, "enum") {
                    self.advance();
                    return self.ts_enum(None);
                }
                self.advance();
                loop {
                    if self.is_punct("{") || self.is_punct("[") {
                        let close = self
                            .matching_paren(self.pos)
                            .map_or_else(|| self.err("pattern expected"), Ok)?;
                        self.pos = close + 1;
                    } else {
                        self.expect_ident_any()?;
                    }
                    self.eat_punct("!");
                    if self.eat_punct(":") {
                        self.ty_cond(true)?;
                    }
                    if self.eat_punct("=") {
                        self.parse_assign()?;
                    }
                    if !self.eat_punct(",") {
                        break;
                    }
                }
                self.consume_semicolon()
            }
            Tok::Keyword("function") | Tok::Ident(_) if self.cw("function") || self.cw("async") => {
                self.skip_fn_signature()?;
                if self.is_punct("{") {
                    self.skip_braces()?;
                }
                self.consume_semicolon()
            }
            Tok::Keyword("class") | Tok::Ident(_) if self.cw("class") || self.cw("abstract") => {
                if self.cw("abstract") {
                    self.advance();
                }
                self.advance(); // class
                if self.ident_at(0) && !self.cw("extends") && !self.cw("implements") {
                    self.advance();
                }
                if self.is_punct("<") {
                    self.ts_type_params_list()?;
                }
                if self.cw("extends") {
                    self.advance();
                    self.ts_type_reference()?;
                }
                if self.cw("implements") {
                    self.advance();
                    loop {
                        self.ts_type_reference()?;
                        if !self.eat_punct(",") {
                            break;
                        }
                    }
                }
                self.skip_braces()
            }
            Tok::Keyword("enum") => self.ts_enum(None),
            Tok::Ident(w) if matches!(w.as_str(), "namespace" | "module" | "global") => {
                self.ts_namespace(true).map(|_| ())
            }
            Tok::Ident(w) if w == "type" => self.ts_type_alias(),
            Tok::Ident(w) if w == "interface" => self.ts_interface(),
            _ => self.err("Declaration expected after 'declare'"),
        }
    }

    /// `[async] function [*] [name] [<T>] (…) [: R]` as tokens.
    fn skip_fn_signature(&mut self) -> Result<(), ParseError> {
        if self.cw("async") {
            self.advance();
        }
        if !self.eat_kw("function") {
            return self.err("'function' expected");
        }
        self.eat_punct("*");
        if matches!(self.cur(), Tok::Ident(_)) {
            self.advance();
        }
        self.sig_rest()
    }

    /// A function declaration without a body (an overload signature): erased whole.
    fn ts_overload(&mut self, idx: usize) -> Result<Option<Stmt>, ParseError> {
        let snap = self.ts_snapshot();
        let sig = self.skip_fn_signature().is_ok() && !self.is_punct("{");
        self.ts_restore(snap);
        if !sig {
            return Ok(None);
        }
        self.take_stmt_flags();
        self.skip_fn_signature()?;
        self.consume_semicolon()?;
        self.mark_type_only(idx);
        Ok(Some(Stmt::Empty))
    }

    fn ts_import(&mut self, idx: usize) -> Result<Option<Stmt>, ParseError> {
        let start = self.cur_b();
        if self.pw(1, "type")
            && ((self.ident_at(2) && !self.pw(2, "from")) || self.pp(2, "{") || self.pp(2, "*"))
        {
            self.take_stmt_flags();
            self.advance();
            self.advance();
            if self.ident_at(0) && self.pp(1, "=") {
                // `import type x = require("y")`.
                self.advance();
                self.advance();
                self.parse_expr()?;
                self.consume_semicolon()?;
            } else {
                self.skip_import_clause()?;
                self.skip_from_clause()?;
                self.consume_semicolon()?;
            }
            self.mark_type_only(idx);
            return Ok(Some(Stmt::Empty));
        }
        if self.ident_at(1) && self.pp(2, "=") {
            self.take_stmt_flags();
            self.unsupported(start, MSG_IMPORT_EQ);
            self.advance();
            self.advance();
            self.advance();
            self.parse_expr()?;
            self.consume_semicolon()?;
            self.unsupported_end(start);
            return Ok(Some(Stmt::Empty));
        }
        let open = if self.pp(1, "{") {
            Some(self.pos + 1)
        } else if self.ident_at(1) && self.pp(2, ",") && self.pp(3, "{") {
            Some(self.pos + 3)
        } else {
            None
        };
        if let Some(open) = open {
            self.ts_strip_type_specifiers(open);
        }
        Ok(None)
    }

    fn skip_import_clause(&mut self) -> Result<(), ParseError> {
        loop {
            if self.is_punct("{") {
                self.skip_braces()?;
            } else if self.eat_punct("*") {
                self.advance(); // as
                self.expect_ident_any()?;
            } else if matches!(self.cur(), Tok::Ident(_)) {
                self.advance();
            } else {
                return self.err("import clause expected");
            }
            if !self.eat_punct(",") {
                return Ok(());
            }
        }
    }

    /// `[from "m"] [with { … }]`.
    fn skip_from_clause(&mut self) -> Result<(), ParseError> {
        if self.cw("from") {
            self.advance();
            match self.cur() {
                Tok::Str(_) => {
                    self.advance();
                }
                _ => return self.err("Module specifier expected"),
            }
        }
        if (self.cw("with") || self.cw("assert")) && !self.nl_before() && self.pp(1, "{") {
            self.advance();
            self.skip_braces()?;
        }
        Ok(())
    }

    /// Erase the `type`-only specifiers of the `{ … }` at token `open` (with their commas) and
    /// drop their tokens, so the JavaScript import/export parse sees the rest.
    fn ts_strip_type_specifiers(&mut self, open: usize) {
        let Some(mut close) = self.matching_brace(open) else {
            return;
        };
        let name_like = |p: &Self, i: usize| {
            matches!(
                p.toks.get(i).map(|t| &t.kind),
                Some(Tok::Ident(_) | Tok::Str(_) | Tok::Keyword(_))
            )
        };
        let is = |p: &Self, i: usize, w: &str| matches!(p.toks.get(i).map(|t| &t.kind), Some(Tok::Ident(x)) if x == w);
        let punct = |p: &Self, i: usize, q: &str| matches!(p.toks.get(i).map(|t| &t.kind), Some(Tok::Punct(x)) if *x == q);
        let mut i = open + 1;
        while i < close {
            let (type_only, len) =
                if is(self, i, "type") && !punct(self, i + 1, ",") && !punct(self, i + 1, "}") {
                    if is(self, i + 1, "as") {
                        if is(self, i + 2, "as") {
                            if name_like(self, i + 3) {
                                (true, 4)
                            } else {
                                (false, 3)
                            }
                        } else if name_like(self, i + 2) && !punct(self, i + 2, ",") {
                            (false, 3)
                        } else {
                            (true, 2)
                        }
                    } else if is(self, i + 2, "as") {
                        (true, 4)
                    } else {
                        (true, 2)
                    }
                } else if is(self, i + 1, "as") {
                    (false, 3)
                } else {
                    (false, 1)
                };
            let mut end = (i + len).min(close);
            let comma = punct(self, end, ",");
            if !type_only {
                i = end + usize::from(comma);
                continue;
            }
            if comma {
                end += 1;
            }
            let (s, _) = self.tb(i);
            let (_, e) = self.tb(end - 1);
            self.erase(s, e);
            self.toks.drain(i..end);
            close -= end - i;
        }
    }

    fn ts_export(&mut self, idx: usize) -> Result<Option<Stmt>, ParseError> {
        let start = self.cur_b();
        if self.pp(1, "=") {
            self.take_stmt_flags();
            self.unsupported(start, MSG_EXPORT_ASSIGN);
            self.advance();
            self.advance();
            self.parse_expr()?;
            self.consume_semicolon()?;
            self.unsupported_end(start);
            return Ok(Some(Stmt::Empty));
        }
        if self.pw(1, "as") && self.pw(2, "namespace") {
            // Node leaves `export as namespace X` (a UMD declaration) untouched.
            self.take_stmt_flags();
            self.advance();
            self.advance();
            self.advance();
            self.expect_ident_any()?;
            self.consume_semicolon()?;
            return Ok(Some(Stmt::Empty));
        }
        if matches!(self.tok_at(1), Tok::Keyword("import")) && self.ident_at(2) && self.pp(3, "=") {
            self.take_stmt_flags();
            let at = self.tb(self.pos + 1).0;
            self.unsupported(at, MSG_IMPORT_EQ);
            for _ in 0..4 {
                self.advance();
            }
            self.parse_expr()?;
            self.consume_semicolon()?;
            self.unsupported_end(at);
            return Ok(Some(Stmt::Empty));
        }
        if self.pw(1, "type") && (self.pp(2, "{") || self.pp(2, "*")) {
            self.take_stmt_flags();
            self.advance();
            self.advance();
            if self.eat_punct("*") {
                if self.cw("as") {
                    self.advance();
                    self.advance();
                }
            } else {
                self.skip_braces()?;
            }
            self.skip_from_clause()?;
            self.consume_semicolon()?;
            self.mark_type_only(idx);
            return Ok(Some(Stmt::Empty));
        }
        if self.pp(1, "{") {
            self.ts_strip_type_specifiers(self.pos + 1);
            return Ok(None);
        }
        if matches!(self.tok_at(1), Tok::Keyword("default")) {
            if self.pw(2, "interface") && self.ident_at(3) {
                self.take_stmt_flags();
                self.advance();
                self.advance();
                self.ts_interface()?;
                self.mark_type_only(idx);
                return Ok(Some(Stmt::Empty));
            }
            if self.pw(2, "abstract") && matches!(self.tok_at(3), Tok::Keyword("class")) {
                let (s, e) = self.tb(self.pos + 2);
                self.erase(s, e);
                self.ts_note_abstract_quirk((s, e));
                self.toks.remove(self.pos + 2);
                return Ok(None);
            }
            if self.pw(2, "function") || (self.pw(2, "async") && self.pw(3, "function")) {
                return self.ts_export_overload(idx, 2);
            }
            return Ok(None);
        }
        match self.ts_decl_at(1) {
            Some("abstract") => {
                let (s, e) = self.tb(self.pos + 1);
                self.erase(s, e);
                self.ts_note_abstract_quirk((s, e));
                self.toks.remove(self.pos + 1);
                Ok(None)
            }
            Some(kind) => {
                self.take_stmt_flags();
                self.advance();
                let inner = self.pos;
                self.ts_decl(kind, inner)?;
                if self.tss().type_only_at == inner {
                    self.mark_type_only(idx);
                }
                Ok(Some(Stmt::Empty))
            }
            None if self.pw(1, "function") || (self.pw(1, "async") && self.pw(2, "function")) => {
                self.ts_export_overload(idx, 1)
            }
            None => Ok(None),
        }
    }

    fn ts_export_overload(&mut self, idx: usize, k: usize) -> Result<Option<Stmt>, ParseError> {
        let snap = self.ts_snapshot();
        self.pos += k;
        let sig = self.skip_fn_signature().is_ok() && !self.is_punct("{");
        self.ts_restore(snap);
        if !sig {
            return Ok(None);
        }
        self.take_stmt_flags();
        self.pos += k;
        self.skip_fn_signature()?;
        self.consume_semicolon()?;
        self.mark_type_only(idx);
        Ok(Some(Stmt::Empty))
    }

    /// After a declared binding (`let x`, a for-head or catch binding): `!` and `: T`, erased.
    pub(super) fn ts_binding_suffix(&mut self, pat_idx: usize) -> Result<(), ParseError> {
        if self.is_punct("!") {
            self.erase_tok();
        }
        if self.is_punct(":") {
            let s = self.cur_b();
            self.advance();
            let ty = self.ts_type()?;
            self.erase_from(s);
            let at = self.tb(pat_idx).0;
            self.tss().vars.push((at, ty));
        }
        Ok(())
    }

    /// Set the key the next parameter list's types are recorded under (a function's source
    /// start, in the lexed slice's chars).
    pub(super) fn ts_fn_start(&mut self, start: u32) {
        let b = self.src.byte_range(start, start).map_or(0, |r| r.0);
        self.tss().fn_start = b;
    }

    /// `@dec export [default] class …` (decorators before `export`, which TypeScript allows):
    /// the decorators go to the exported class. `None` (nothing consumed) when no `export`
    /// follows the decorators.
    fn ts_decorated_export(&mut self) -> Result<Option<Stmt>, ParseError> {
        let snap = self.ts_snapshot();
        let class_start = self.cur_start();
        let decorators = self.parse_decorators()?;
        if !self.is_kw("export") {
            self.ts_restore(snap);
            return Ok(None);
        }
        let export = self.tb(self.pos);
        self.tss().pending_decorators = Some((decorators, class_start, export));
        let s = self.ts_parse_stmt()?;
        if self.tss().pending_decorators.take().is_some() && !matches!(s, Stmt::Empty) {
            // (`export declare class` is erased and Node keeps its decorators as text.)
            return self.err("Decorators are not valid here");
        }
        Ok(Some(s))
    }

    /// Decorators pending for this class (see `ts_decorated_export`): (decorators, start char).
    pub(super) fn ts_take_decorators(&mut self) -> Option<(Vec<Expr>, u32)> {
        let t = self.ts.as_mut()?;
        t.pending_decorators.take().map(|(d, at, _)| (d, at))
    }

    fn ts_note_abstract_quirk(&mut self, abs: Span) {
        let t = self.tss();
        if let Some((_, _, export)) = t.pending_decorators {
            t.swc_abstract_quirk.push((abs, export));
        }
    }

    /// A class constructor's `FnSource` is the class: move its types' key there.
    pub(super) fn ts_remap_fn(&mut self, from: u32, to: u32) {
        if let Some(f) = self.tss().fns.iter_mut().rev().find(|f| f.start == from) {
            f.start = to;
        }
    }

    /// `parse_params_inner` in TypeScript mode: `this` parameters, parameter properties
    /// (rejected), `?`, annotations, and the return type after `)`.
    pub(super) fn ts_params_inner(&mut self) -> Result<Vec<Param>, ParseError> {
        let mut sig = SideFn {
            start: self.tss().fn_start,
            ..Default::default()
        };
        let mut params = Vec::new();
        while !self.is_punct(")") {
            if self.is_punct("@") {
                self.parse_decorators()?;
            }
            let pstart = self.cur_b();
            let mut property = false;
            while matches!(self.cur(), Tok::Ident(w) if matches!(w.as_str(), "public" | "private" | "protected" | "readonly" | "override"))
                && (self.ident_at(1) || self.pp(1, "{") || self.pp(1, "["))
            {
                self.advance();
                property = true;
            }
            let prop_at = self.cur_b();
            if property {
                self.unsupported(prop_at, MSG_PARAM_PROP);
            }
            if matches!(self.cur(), Tok::Keyword("this")) && self.pp(1, ":") {
                self.advance();
                self.advance();
                sig.this = Some(self.ts_type()?);
                let end = self.prev_end_b();
                if self.eat_punct(",") {
                    self.erase_from(pstart);
                    continue;
                }
                self.erase(pstart, end);
                break;
            }
            let rest = self.eat_punct("...");
            let at = self.cur_b();
            let pattern = self.parse_binding_pattern()?;
            let optional = self.is_punct("?");
            if optional {
                self.erase_tok();
            }
            let ty = if self.is_punct(":") {
                let s = self.cur_b();
                self.advance();
                let ty = self.ts_type()?;
                self.erase_from(s);
                Some(ty)
            } else {
                None
            };
            let default = if !rest && self.eat_punct("=") {
                Some(self.parse_assign()?)
            } else {
                None
            };
            if property {
                self.unsupported_end(prop_at);
            }
            sig.params.push(ParamTy {
                at,
                optional,
                rest,
                has_default: default.is_some(),
                ty,
            });
            params.push(Param {
                pattern,
                default,
                rest,
            });
            if rest || !self.eat_punct(",") {
                break;
            }
        }
        self.expect_punct(")")?;
        let close = self.pos - 1;
        let mut has_ret = false;
        if self.is_punct(":") {
            let s = self.cur_b();
            self.advance();
            let ts = self.cur_b();
            self.ts_return_type_inner()?;
            sig.ret = Some((ts, self.prev_end_b()));
            self.erase_from(s);
            has_ret = true;
        }
        let t = self.tss();
        t.last_params = (close, has_ret);
        t.fns.push(sig);
        Ok(params)
    }

    // ----- classes -----------------------------------------------------------------------------

    /// The index after the last token before `idx` that is not erased.
    pub(super) fn ts_skip_erased_back(&self, mut idx: usize) -> usize {
        let Some(t) = self.ts.as_ref() else {
            return idx;
        };
        // Only type arguments and `!` end right before a call's `(`.
        if idx == 0 || !matches!(self.toks[idx - 1].kind, Tok::Punct(">" | "!")) {
            return idx;
        }
        while idx > 0 {
            let at = self.tb(idx - 1).0;
            // The covering erase (type arguments, `!`) was pushed just now: look at recent ones.
            if !t
                .erase
                .iter()
                .rev()
                .take(16)
                .any(|&(s, e)| s <= at && at < e)
            {
                break;
            }
            idx -= 1;
        }
        idx
    }

    /// `abstract` before `class` (after decorators): erased.
    pub(super) fn ts_erase_abstract(&mut self) {
        if matches!(self.tok_at(1), Tok::Keyword("class")) {
            self.erase_tok();
        }
    }

    /// After a class name: type parameters. After `extends X`: its type arguments, then
    /// `implements …`.
    pub(super) fn ts_class_heritage(&mut self, after_extends: bool) -> Result<(), ParseError> {
        if self.is_punct("<") {
            if after_extends {
                self.ts_type_args()?;
            } else {
                self.ts_type_params()?;
            }
        }
        if self.cw("implements") {
            let s = self.cur_b();
            self.advance();
            loop {
                self.ts_type_reference()?;
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.erase_from(s);
        }
        Ok(())
    }

    /// A class member in TypeScript mode: modifiers, index signatures, and the members Node
    /// erases whole (`declare` fields, abstract members, overload signatures).
    pub(super) fn ts_class_member(&mut self) -> Result<Vec<ClassMember>, ParseError> {
        let anchor_idx = self.pos;
        let anchor = self.cur_b();
        let decorated = self.is_punct("@");
        let decorators = self.parse_decorators()?;
        let lead_erased = !decorated
            && matches!(self.cur(), Tok::Ident(w) if matches!(w.as_str(), "public" | "private" | "protected" | "readonly" | "abstract" | "override" | "declare"))
            && !self.member_name_ends(1);
        let (mut is_static, mut is_abstract, mut declare) = (false, false, false);
        while let Tok::Ident(w) = self.cur().clone() {
            if !MEMBER_MODIFIERS.contains(&w.as_str()) || self.member_name_ends(1) {
                break;
            }
            if w == "static" && self.pp(1, "{") {
                is_static = true;
                self.advance();
                break;
            }
            match w.as_str() {
                "accessor" => break,
                "static" => {
                    is_static = true;
                    self.advance();
                    continue;
                }
                "abstract" => is_abstract = true,
                "declare" => declare = true,
                _ => {}
            }
            self.erase_tok();
        }
        // Index signature.
        let index_sig = self.is_punct("[") && self.ident_at(1) && self.pp(2, ":");
        let erase_whole = if index_sig {
            self.advance();
            self.advance();
            self.advance();
            self.ty_cond(true)?;
            self.expect_punct("]")?;
            if self.eat_punct(":") {
                self.ty_cond(true)?;
            }
            self.consume_semicolon()?;
            true
        } else if declare || is_abstract || self.ts_member_is_signature() {
            self.skip_member()?;
            true
        } else {
            false
        };
        if erase_whole {
            self.erase_from(anchor);
            let semis = &mut self.tss().semis;
            if let Some(i) = semis.iter().rposition(|&s| s == anchor) {
                semis.remove(i);
            }
            let next_hazard =
                self.is_punct("[") || self.is_punct("*") || self.cw("in") || self.cw("instanceof");
            if next_hazard {
                let erased = |p: &Self, at: u32| {
                    p.ts.as_ref()
                        .is_some_and(|t| t.erase.iter().any(|&(s, e)| s <= at && at < e))
                };
                let mut k = anchor_idx;
                while k > 0 && erased(self, self.tb(k - 1).0) {
                    k -= 1;
                }
                if k > 0 && !matches!(self.toks[k - 1].kind, Tok::Punct(";" | "{" | "}")) {
                    self.tss().semis.push(anchor);
                }
            }
            return Ok(Vec::new());
        }
        if lead_erased && !is_static && !self.cw("accessor") {
            // What the key looks like, past get/set/async prefixes.
            let mut k = 0;
            while (self.pw(k, "get") || self.pw(k, "set") || self.pw(k, "async"))
                && !self.member_name_ends(k + 1)
            {
                k += 1;
            }
            if self.pp(k, "*") || self.pp(k, "[") || self.pw(k, "in") || self.pw(k, "instanceof") {
                self.tss().semis.push(anchor);
            }
        }
        self.parse_class_member_rest(decorators, is_static)
    }

    /// Whether the member at the cursor is a method signature without a body.
    fn ts_member_is_signature(&mut self) -> bool {
        let snap = self.ts_snapshot();
        let r = (|| {
            self.skip_member_head()?;
            if !(self.is_punct("(") || self.is_punct("<")) {
                return Ok(false);
            }
            self.sig_rest()?;
            Ok::<bool, ParseError>(!self.is_punct("{"))
        })();
        self.ts_restore(snap);
        r.unwrap_or(false)
    }

    /// `get`/`set`/`async`/`*` prefixes, the key, `?`.
    fn skip_member_head(&mut self) -> Result<(), ParseError> {
        while (self.cw("get") || self.cw("set") || self.cw("async") || self.cw("accessor"))
            && !self.member_name_ends(1)
            && !self.pp(1, "*")
        {
            self.advance();
        }
        if self.cw("async") && self.pp(1, "*") {
            self.advance();
        }
        self.eat_punct("*");
        if self.is_punct("[") {
            self.advance();
            self.parse_assign_allow_in()?;
            self.expect_punct("]")?;
        } else {
            self.parse_prop_key()?;
        }
        self.eat_punct("?");
        Ok(())
    }

    /// A member erased whole: its head, then a signature (with a body, if any) or a field.
    fn skip_member(&mut self) -> Result<(), ParseError> {
        self.skip_member_head()?;
        if self.is_punct("(") || self.is_punct("<") {
            self.sig_rest()?;
            if self.is_punct("{") {
                self.skip_braces()?;
                return Ok(());
            }
        } else {
            self.eat_punct("!");
            if self.eat_punct(":") {
                self.ty_cond(true)?;
            }
            if self.eat_punct("=") {
                self.parse_assign_allow_in()?;
            }
        }
        self.consume_semicolon()
    }

    /// After a class member's key: `?`, and for a field `!` and `: T`.
    pub(super) fn ts_member_after_key(&mut self, key_idx: usize) -> Result<(), ParseError> {
        if self.is_punct("?") {
            self.erase_tok();
        }
        if self.is_punct("!") {
            self.erase_tok();
        }
        if self.is_punct(":") {
            let s = self.cur_b();
            self.advance();
            let ty = self.ts_type()?;
            self.erase_from(s);
            let at = self.tb(key_idx).0;
            self.tss().fields.push((at, ty));
        }
        Ok(())
    }

    // ----- expressions -------------------------------------------------------------------------

    /// Arrows only TypeScript has: `(…): R =>`, `<T>(…) =>`, `async <T>(…) =>`.
    pub(super) fn ts_try_arrow(
        &mut self,
        arrow_start: u32,
        call_prefix: bool,
    ) -> Result<Option<Expr>, ParseError> {
        let no_ret = std::mem::take(&mut self.tss().no_ret_arrow);
        self.ts_fn_start(arrow_start);
        let is_async = self.is_ident_word("async")
            && !self.cur_escaped()
            && !self.nl_at(1)
            && (self.pp(1, "(") || self.pp(1, "<"));
        let open = self.pos + usize::from(is_async);
        let candidate = match self.toks.get(open).map(|t| &t.kind) {
            Some(Tok::Punct("(")) => self.matching_paren(open).is_some_and(|close| {
                matches!(
                    self.toks.get(close + 1).map(|t| &t.kind),
                    Some(Tok::Punct(":"))
                )
            }),
            Some(Tok::Punct("<")) => true,
            _ => false,
        };
        if !candidate {
            return Ok(None);
        }
        let snap = self.ts_snapshot();
        let head = (|| {
            if is_async {
                self.advance();
            }
            if self.is_punct("<") {
                self.ts_type_params()?;
            }
            if !self.is_punct("(") {
                return self.err("'(' expected");
            }
            let sa = self.in_async;
            self.in_async = sa || is_async;
            let params = self.parse_params();
            self.in_async = sa;
            let params = params?;
            if !self.is_punct("=>") {
                return self.err("'=>' expected");
            }
            let (close, has_ret) = self.tss().last_params;
            if has_ret {
                // A line break between `)` and `=>` would be invalid JavaScript once the return
                // type is gone: Node moves the `)` to the type's last character.
                let broken = (close + 1..=self.pos).any(|i| self.toks[i].nl_before);
                if broken {
                    let (cs, ce) = self.tb(close);
                    let last_end = self.prev_end_b() as usize;
                    let last = self.src.src[..last_end]
                        .char_indices()
                        .next_back()
                        .map_or(0, |c| c.0);
                    self.erase(cs, ce);
                    self.tss().parens.push(last as u32);
                } else if self.nl_before() {
                    return self.err("'=>' expected");
                }
            } else if self.nl_before() {
                return self.err("'=>' expected");
            }
            self.advance();
            Ok((params, has_ret))
        })();
        let (params, has_ret) = match head {
            Ok(h) => h,
            Err(_) => {
                self.ts_restore(snap);
                return Ok(None);
            }
        };
        if no_ret && has_ret {
            // `c ? (x): T => y : z`: an arrow only when a `:` follows its body.
            let body = self.finish_arrow(params, is_async, arrow_start, call_prefix);
            return match body {
                Ok(e) if self.is_punct(":") => Ok(Some(e)),
                _ => {
                    self.ts_restore(snap);
                    Ok(None)
                }
            };
        }
        self.finish_arrow(params, is_async, arrow_start, call_prefix)
            .map(Some)
    }

    /// `<T>expr`: rejected, as Node does.
    pub(super) fn ts_angle_assertion(&mut self) -> Result<Expr, ParseError> {
        let at = self.cur_b();
        self.unsupported(at, MSG_ANGLE);
        self.advance();
        self.ty_cond(true)?;
        self.ts_expect_gt()?;
        let e = self.parse_unary()?;
        self.unsupported_end(at);
        Ok(e)
    }

    /// `as T` / `as const` / `satisfies T` in operator position (binds like a relational
    /// operator). Returns whether one was consumed.
    pub(super) fn ts_as(&mut self, min_prec: u8) -> Result<bool, ParseError> {
        if self.nl_before() || min_prec > 8 || !(self.cw("as") || self.cw("satisfies")) {
            return Ok(false);
        }
        if self.cur_escaped() {
            return Ok(false);
        }
        let is_as = self.cw("as");
        let start = self.cur_b();
        self.advance();
        if is_as && self.cw("const") {
            self.advance();
        } else {
            self.ty_cond(true)?;
        }
        self.erase_from(start);
        // `a as T` ends the expression: a `(`/`[`/template on the next line would join it.
        if self.nl_before() && matches!(self.cur(), Tok::Punct("(" | "[") | Tok::Template(_)) {
            self.tss().semis.push(start);
        }
        Ok(true)
    }

    /// TypeScript after an expression in a call chain: `x!` and `f<T>` (type arguments of a
    /// call, a tagged template or an instantiation expression). Returns whether one was
    /// consumed.
    pub(super) fn ts_lhs_suffix(&mut self) -> Result<bool, ParseError> {
        if self.is_punct("!") && !self.nl_before() {
            self.erase_tok();
            return Ok(true);
        }
        if !self.is_punct("<") {
            return Ok(false);
        }
        let snap = self.ts_snapshot();
        let ok = self.ts_type_args().is_ok()
            && (self.is_punct("(")
                || matches!(self.cur(), Tok::Template(_))
                || self.can_follow_type_args());
        if !ok {
            self.ts_restore(snap);
        }
        Ok(ok)
    }

    fn can_follow_type_args(&self) -> bool {
        match self.cur() {
            Tok::Punct("<" | ">" | "+" | "-") => false,
            _ if self.nl_before() => true,
            Tok::Punct(p) => is_binary_punct(p) || !starts_expr_punct(p),
            Tok::Keyword(k) => matches!(*k, "in" | "instanceof"),
            Tok::Ident(w) => matches!(w.as_str(), "as" | "satisfies"),
            Tok::Eof => true,
            _ => false,
        }
    }

    /// `new C<T>(…)`: type arguments before the arguments.
    pub(super) fn ts_new_type_args(&mut self) {
        if self.is_punct("<") {
            let snap = self.ts_snapshot();
            if self.ts_type_args().is_err() {
                self.ts_restore(snap);
            }
        }
    }

    // ----- finishing ---------------------------------------------------------------------------

    /// After a successful parse: report unsupported syntax, blank the source in place, and
    /// register the side table. Returns the stripped text when `want_text`.
    pub(super) fn ts_finish(&mut self, want_text: bool) -> Result<Option<String>, ParseError> {
        let st = self.ts.take().expect("TypeScript mode");
        let rc = self.src.src.clone();
        if let Some(&(at, end, msg)) = st.unsupported.iter().min_by_key(|u| u.0) {
            set_error_span(at, end);
            return Err(ParseError {
                message: msg.to_string(),
                line: line_of(&rc, at),
                at_eof: false,
            });
        }
        // Side table: copy the type text out before the source is blanked.
        let original: Option<Rc<str>> = cfg!(feature = "typed").then(|| Rc::from(&*rc));
        let mut table = SideTable::default();
        let mut copy = |s: Span| -> Span {
            let from = table.text.len() as u32;
            table
                .text
                .push_str(rc.get(s.0 as usize..s.1 as usize).unwrap_or(""));
            (from, table.text.len() as u32)
        };
        let mut fns = st.fns.clone();
        for f in &mut fns {
            f.this = f.this.map(&mut copy);
            f.ret = f.ret.map(&mut copy);
            f.type_params = f.type_params.map(&mut copy);
            for p in &mut f.params {
                p.ty = p.ty.map(&mut copy);
            }
        }
        let vars: Vec<(u32, Span)> = st.vars.iter().map(|&(a, s)| (a, copy(s))).collect();
        let fields: Vec<(u32, Span)> = st.fields.iter().map(|&(a, s)| (a, copy(s))).collect();
        fns.sort_by_key(|f| f.start);
        fns.dedup_by_key(|f| f.start);
        table.fns = fns;
        table.vars = vars;
        table.fields = fields;
        table.docs = st.docs.clone();
        let out = stripped(&rc, &st);
        let text = if want_text && !st.swc_abstract_quirk.is_empty() {
            let mut st = st;
            for (abs, exp) in std::mem::take(&mut st.swc_abstract_quirk) {
                st.erase.retain(|&r| r != abs);
                st.erase.push(exp);
            }
            Some(stripped(&rc, &st))
        } else {
            None
        };
        // The stripped text replaces the source in place: every function and lazy body of this
        // parse holds this `Rc`, and none has read it yet.
        debug_assert_eq!(out.len(), rc.len());
        debug_assert!(std::str::from_utf8(&out).is_ok());
        if std::str::from_utf8(&out).is_ok() && out.len() == rc.len() {
            // SAFETY: `out` is valid UTF-8 of the same length, and no `&str` into the
            // allocation is live (every holder is an `Rc` clone that has not been read). This
            // is `Rc::get_mut_unchecked`'s contract.
            unsafe {
                let p = Rc::as_ptr(&rc) as *const u8 as *mut u8;
                std::ptr::copy_nonoverlapping(out.as_ptr(), p, out.len());
            }
        }
        if table != SideTable::default() {
            register_side_table(&rc, table, original);
        }
        let out = text.unwrap_or(out);
        Ok(want_text.then(|| String::from_utf8(out).unwrap_or_default()))
    }
}
