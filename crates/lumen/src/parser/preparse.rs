//! Syntax-only validation of a function body the parser is about to skip.
//!
//! A lazily parsed body must still report its early errors at load. The full parser does that by
//! building (and dropping) the whole AST of every body; this pass walks the same grammar over the
//! token vector without building anything. It only ever answers "valid" or "not sure": anything
//! it does not model exactly (classes, `super`, ...) or any error it finds returns `None`, and
//! the caller runs the full checking parse, which also produces the precise error. So the rules here must accept a subset of what the full parser
//! accepts and may never accept more.

use super::*;

/// What the checking parse of a body reports besides errors.
pub(super) struct Outcome {
    /// Top-level lexical names of the body (a parameter may not share one).
    pub lexical: Vec<String>,
    /// Private names referenced in the body.
    pub private: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Id {
    Normal,
    EvalArgs,
    Undefined,
}

/// The shape of the expression just parsed, as far as later rules look at it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum K {
    Ident(Id),
    /// A member access; `true` for a private name (`a.#x`).
    Member(bool),
    Index,
    Call,
    OptChain,
    Super,
    /// An unparenthesized `&&` / `||` expression (`??` may not mix with these).
    AndOr,
    Other,
}

/// What a statement turned out to be, for the single-statement-context rules.
#[derive(Clone, Copy, PartialEq, Eq)]
enum S {
    Plain,
    Lexical,
    Func { plain: bool },
}

enum Pk {
    Ident(Name),
    Keyword,
    Str(bool),
    Other,
}

struct Params {
    simple: bool,
    count: usize,
    first_rest: bool,
    /// Set by [`Pre::method`]: the body is strict and a parameter is a strict-reserved binding.
    strict_reserved: bool,
}

impl Parser {
    /// Validate the body whose `{` is token `open` and whose matching `}` is token `close`.
    pub(super) fn preparse_body(&self, open: usize, close: usize) -> Option<Outcome> {
        if self.ts.is_some() || self.jsx.is_some() || self.in_static_block {
            return None;
        }
        let mut pre = Pre {
            p: self,
            toks: &self.toks,
            in_sub: false,
            pos: open + 1,
            bad: false,
            strict: self.strict,
            module: self.module,
            in_generator: self.in_generator,
            in_async: self.in_async,
            in_params: false,
            no_in: false,
            forbid_arguments: self.no_arguments_refs || self.in_field_init,
            super_prop_ok: self.super_prop_ok,
            super_call_ok: self.super_call_ok,
            iter_depth: 0,
            switch_depth: 0,
            labels: Vec::new(),
            iter_labels: Vec::new(),
            scopes: Vec::new(),
            private: Vec::new(),
            depth: 0,
            stmts: 0,
            chain: 0,
            last_paren: false,
        };
        pre.top(close)
    }
}

struct Pre<'a> {
    p: &'a Parser,
    toks: &'a TokVec,
    /// Inside a template substitution's own token list.
    in_sub: bool,
    pos: usize,
    /// Set by a token the pass does not model (an escaped keyword, a strict-mode octal).
    bad: bool,
    strict: bool,
    module: bool,
    in_generator: bool,
    in_async: bool,
    in_params: bool,
    no_in: bool,
    forbid_arguments: bool,
    super_prop_ok: bool,
    super_call_ok: bool,
    iter_depth: u32,
    switch_depth: u32,
    labels: Vec<Name>,
    iter_labels: Vec<Name>,
    scopes: Vec<DeclScope>,
    private: Vec<Name>,
    depth: u32,
    stmts: u32,
    chain: u32,
    last_paren: bool,
}

/// Nesting beyond this is left to the full parser, which owns the "too deep" errors.
const MAX_DEPTH: u32 = 600;
const MAX_CHAIN: u32 = 96;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BinOp {
    Nullish,
    Logical,
    Other,
}

fn is_assign_punct(p: Punct) -> bool {
    matches!(
        p,
        Punct::Assign
            | Punct::AddAssign
            | Punct::SubAssign
            | Punct::MulAssign
            | Punct::DivAssign
            | Punct::RemAssign
            | Punct::PowAssign
            | Punct::ShlAssign
            | Punct::ShrAssign
            | Punct::UShrAssign
            | Punct::BitAndAssign
            | Punct::BitOrAssign
            | Punct::BitXorAssign
            | Punct::AndAssign
            | Punct::OrAssign
            | Punct::NullishAssign
    )
}

impl<'a> Pre<'a> {
    fn top(&mut self, close: usize) -> Option<Outcome> {
        self.push_scope(true);
        while !self.is_p("}") {
            if matches!(self.kind(), Tok::Eof) {
                return None;
            }
            self.stmt(false)?;
            if self.bad {
                return None;
            }
        }
        if self.pos != close || self.bad {
            return None;
        }
        let scope = self.scopes.pop()?;
        let mut private: Vec<String> = Vec::new();
        for n in &self.private {
            if !private.iter().any(|p| p == n.as_str()) {
                private.push(n.to_string());
            }
        }
        Some(Outcome {
            lexical: scope.lexical.iter().map(|n| n.to_string()).collect(),
            private,
        })
    }

    // ----- token access -----------------------------------------------------------------------

    #[inline]
    fn kind(&self) -> &'a Tok {
        let toks: &'a TokVec = self.toks;
        &toks[self.pos].kind
    }
    #[inline]
    fn tok(&self) -> &'a Token {
        let toks: &'a TokVec = self.toks;
        &toks[self.pos]
    }
    #[inline]
    fn peek(&self, ahead: usize) -> Option<&'a Tok> {
        let toks: &'a TokVec = self.toks;
        toks.get(self.pos + ahead).map(|t| &t.kind)
    }
    fn nl_at(&self, ahead: usize) -> bool {
        self.toks.get(self.pos + ahead).is_some_and(|t| t.nl_before())
    }
    #[inline]
    fn advance(&mut self) {
        let t = self.tok();
        if (t.escaped() && !matches!(t.kind, Tok::Str(_))) || (t.legacy_octal() && self.strict) {
            self.bad = true;
        }
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
    }
    #[inline(always)]
    fn is_p(&self, p: &str) -> bool {
        matches!(self.kind(), Tok::Punct(x) if x.as_str() == p)
    }
    #[inline(always)]
    fn is_kw(&self, k: &str) -> bool {
        matches!(self.kind(), Tok::Keyword(x) if x.as_str() == k)
    }
    #[inline]
    fn is_word(&self, w: &str) -> bool {
        matches!(self.kind(), Tok::Ident(x) if x == w)
    }
    #[inline(always)]
    fn eat_p(&mut self, p: &str) -> bool {
        if self.is_p(p) {
            self.advance();
            true
        } else {
            false
        }
    }
    #[inline(always)]
    fn eat_kw(&mut self, k: &str) -> bool {
        if self.is_kw(k) {
            self.advance();
            true
        } else {
            false
        }
    }
    #[inline(always)]
    fn expect_p(&mut self, p: &str) -> Option<()> {
        self.eat_p(p).then_some(())
    }
    fn at_eof(&self) -> bool {
        matches!(self.kind(), Tok::Eof)
    }
    fn can_end_stmt(&self) -> bool {
        self.is_p(";") || self.is_p("}") || self.at_eof() || self.tok().nl_before()
    }
    fn semicolon(&mut self) -> Option<()> {
        if self.eat_p(";") || self.can_end_stmt() {
            Some(())
        } else {
            None
        }
    }

    fn matching_paren(&self, open: usize) -> Option<usize> {
        if self.in_sub {
            let mut depth = 0i32;
            for (i, t) in self.toks.iter_from(open).enumerate() {
                match &t.kind {
                    Tok::Punct(Punct::LParen | Punct::LBracket | Punct::LBrace) => depth += 1,
                    Tok::Punct(Punct::RParen | Punct::RBracket | Punct::RBrace) => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(open + i);
                        }
                    }
                    Tok::Eof => return None,
                    _ => {}
                }
            }
            None
        } else {
            self.p.matching_paren(open)
        }
    }

    // ----- scopes and limits ------------------------------------------------------------------

    fn push_scope(&mut self, fn_boundary: bool) {
        self.scopes.push(DeclScope {
            fn_boundary,
            ..Default::default()
        });
    }
    fn pop_scope(&mut self) -> Option<DeclScope> {
        scope_pop(&mut self.scopes)
    }

    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        self.depth += 1;
        if self.depth > MAX_DEPTH || (self.depth & 15 == 1 && crate::stack::exhausted()) {
            return None;
        }
        let r = f(self);
        self.depth -= 1;
        r
    }
    fn chained<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        let saved = self.chain;
        let r = f(self);
        self.chain = saved;
        r
    }
    fn chain_link(&mut self) -> Option<()> {
        self.chain += 1;
        if self.chain > 8 {
            if self.chain > MAX_CHAIN {
                return None;
            }
            let room = crate::stack::headroom()?;
            if room / CHAIN_LINK_BYTES <= self.chain as usize + 8 {
                return None;
            }
        }
        Some(())
    }

    // ----- statements -------------------------------------------------------------------------

    fn stmt(&mut self, single: bool) -> Option<S> {
        self.stmts = self.stmts.wrapping_add(1);
        if self.stmts & 7 == 0 && crate::stack::exhausted() {
            return None;
        }
        match self.kind() {
            Tok::Punct(Punct::LBrace) => {
                self.advance();
                self.block_rest()?;
                Some(S::Plain)
            }
            Tok::Punct(Punct::Semi) => {
                self.advance();
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::Var) => {
                self.var_decl(DeclKind::Var)?;
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::Const) => {
                self.var_decl(DeclKind::Const)?;
                Some(S::Lexical)
            }
            Tok::Ident(w)
                if w == "let"
                    && matches!(self.peek(1), Some(Tok::Ident(_) | Tok::Punct(Punct::LBracket | Punct::LBrace)))
                    && (!single || matches!(self.peek(1), Some(Tok::Punct(Punct::LBracket)))) =>
            {
                self.var_decl(DeclKind::Let)?;
                Some(S::Lexical)
            }
            Tok::Ident(w) if w == "using" && matches!(self.peek(1), Some(Tok::Ident(_))) => None,
            Tok::Ident(w)
                if w == "await"
                    && self.in_async
                    && matches!(self.peek(1), Some(Tok::Ident(u)) if u == "using") =>
            {
                None
            }
            Tok::Keyword(Keyword::Function) => {
                let f = self.function(false, false)?;
                let name = f.name?;
                if !single {
                    self.declare_fn(&name, f.is_async, f.is_gen)?;
                }
                Some(S::Func {
                    plain: !f.is_async && !f.is_gen,
                })
            }
            Tok::Ident(w)
                if w == "async"
                    && matches!(self.peek(1), Some(Tok::Keyword(Keyword::Function)))
                    && !self.nl_at(1) =>
            {
                self.advance();
                let f = self.function(true, false)?;
                let name = f.name?;
                if !single {
                    self.declare_fn(&name, f.is_async, f.is_gen)?;
                }
                Some(S::Func { plain: false })
            }
            Tok::Keyword(Keyword::Class | Keyword::Import | Keyword::Export) | Tok::Punct(Punct::At) => None,
            Tok::Keyword(Keyword::If) => {
                self.advance();
                self.expect_p("(")?;
                self.expr()?;
                self.expect_p(")")?;
                self.substatement(true)?;
                if self.eat_kw("else") {
                    self.substatement(true)?;
                }
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::While) => {
                self.advance();
                self.expect_p("(")?;
                self.expr()?;
                self.expect_p(")")?;
                self.loop_body()?;
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::With) => {
                if self.strict {
                    return None;
                }
                self.advance();
                self.expect_p("(")?;
                self.expr()?;
                self.expect_p(")")?;
                self.substatement(false)?;
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::Do) => {
                self.advance();
                self.loop_body()?;
                if !self.eat_kw("while") {
                    return None;
                }
                self.expect_p("(")?;
                self.expr()?;
                self.expect_p(")")?;
                self.eat_p(";");
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::For) => {
                self.push_scope(false);
                self.for_inner()?;
                self.pop_scope()?;
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::Return) => {
                self.advance();
                if !self.can_end_stmt() {
                    self.expr()?;
                }
                self.semicolon()?;
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::Break) => {
                self.advance();
                match self.opt_label() {
                    Some(label) => {
                        if !self.labels.contains(&label) {
                            return None;
                        }
                    }
                    None => {
                        if self.iter_depth == 0 && self.switch_depth == 0 {
                            return None;
                        }
                    }
                }
                self.semicolon()?;
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::Continue) => {
                self.advance();
                match self.opt_label() {
                    Some(label) => {
                        if !self.labels.contains(&label) || !self.iter_labels.contains(&label) {
                            return None;
                        }
                    }
                    None => {
                        if self.iter_depth == 0 {
                            return None;
                        }
                    }
                }
                self.semicolon()?;
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::Throw) => {
                self.advance();
                if self.tok().nl_before() {
                    return None;
                }
                self.expr()?;
                self.semicolon()?;
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::Try) => {
                self.try_stmt()?;
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::Switch) => {
                self.switch_stmt()?;
                Some(S::Plain)
            }
            Tok::Keyword(Keyword::Debugger) => {
                self.advance();
                self.semicolon()?;
                Some(S::Plain)
            }
            Tok::Ident(_) if matches!(self.peek(1), Some(Tok::Punct(Punct::Colon))) => self.labeled(),
            _ => {
                self.expr()?;
                self.semicolon()?;
                Some(S::Plain)
            }
        }
    }

    fn opt_label(&mut self) -> Option<Name> {
        if self.tok().nl_before() {
            return None;
        }
        let Tok::Ident(name) = self.kind() else {
            return None;
        };
        let name = name.clone();
        self.advance();
        Some(name)
    }

    fn check_label(&self, name: &str) -> Option<()> {
        let reserved = (name == "yield" && (self.strict || self.in_generator))
            || (self.strict
                && matches!(
                    name,
                    "let"
                        | "implements"
                        | "interface"
                        | "package"
                        | "private"
                        | "protected"
                        | "public"
                        | "static"
                ))
            || (name == "await" && (self.module || self.in_async))
            || name.starts_with('#');
        (!reserved).then_some(())
    }

    fn labeled(&mut self) -> Option<S> {
        let mut chain: Vec<Name> = Vec::new();
        loop {
            let Tok::Ident(name) = self.kind() else {
                break;
            };
            if !matches!(self.peek(1), Some(Tok::Punct(Punct::Colon))) {
                break;
            }
            self.check_label(name)?;
            if self.labels.contains(name) {
                return None;
            }
            let name = name.clone();
            self.advance();
            self.advance();
            self.labels.push(name.clone());
            chain.push(name);
        }
        let iter_ahead = matches!(self.kind(), Tok::Keyword(Keyword::For | Keyword::While | Keyword::Do));
        if iter_ahead {
            self.iter_labels.extend(chain.iter().cloned());
        }
        let result = if !self.strict && self.is_kw("function") {
            let f = self.function(false, false)?;
            if f.is_async || f.is_gen {
                return None;
            }
            self.declare_fn(&f.name?, false, false)?;
            S::Func { plain: false }
        } else {
            self.substatement(true)?;
            S::Plain
        };
        for _ in &chain {
            self.labels.pop();
            if iter_ahead {
                self.iter_labels.pop();
            }
        }
        Some(result)
    }

    /// The body of an `if`/loop/`with`: no lexical or class declaration, and a plain function
    /// declaration only where Annex B allows it.
    fn substatement(&mut self, annexb_fn: bool) -> Option<()> {
        match self.stmt(true)? {
            S::Plain => Some(()),
            S::Lexical => None,
            S::Func { plain } => (plain && annexb_fn && !self.strict).then_some(()),
        }
    }

    fn loop_body(&mut self) -> Option<()> {
        self.iter_depth += 1;
        let r = self.substatement(false);
        self.iter_depth -= 1;
        r
    }

    /// The rest of a `{ ... }` block whose `{` is consumed; returns the closed scope.
    fn block_rest(&mut self) -> Option<DeclScope> {
        self.push_scope(false);
        while !self.is_p("}") {
            if self.at_eof() {
                return None;
            }
            self.stmt(false)?;
            if self.bad {
                return None;
            }
        }
        self.advance();
        self.pop_scope()
    }

    fn declare_fn(&mut self, name: &Name, is_async: bool, is_gen: bool) -> Option<()> {
        scope_declare_fn(&mut self.scopes, name, is_async, is_gen, self.strict).then_some(())
    }

    fn declare_names(&mut self, kind: DeclKind, names: &[Name]) -> Option<()> {
        if kind != DeclKind::Var && names.iter().any(|n| n.as_str() == "let") {
            return None;
        }
        for n in names {
            let ok = match kind {
                DeclKind::Var => scope_declare_var(&mut self.scopes, n, true),
                _ => scope_declare_lexical(&mut self.scopes, n),
            };
            if !ok {
                return None;
            }
        }
        Some(())
    }

    fn var_decl(&mut self, kind: DeclKind) -> Option<()> {
        self.advance();
        let mut names = Vec::new();
        loop {
            self.binding_pattern(&mut names)?;
            if self.eat_p("=") {
                self.assign()?;
            } else if kind == DeclKind::Const {
                return None;
            }
            if !self.eat_p(",") {
                break;
            }
        }
        self.declare_names(kind, &names)?;
        self.semicolon()
    }

    fn try_stmt(&mut self) -> Option<()> {
        self.advance();
        self.expect_p("{")?;
        self.block_rest()?;
        let mut handled = false;
        if self.eat_kw("catch") {
            handled = true;
            let mut param = None;
            if self.eat_p("(") {
                let mut names = Vec::new();
                self.binding_pattern(&mut names)?;
                self.expect_p(")")?;
                if has_duplicate(&names) {
                    return None;
                }
                param = Some(names);
            }
            self.expect_p("{")?;
            let scope = self.block_rest()?;
            if let Some(names) = param {
                if scope
                    .lexical
                    .iter()
                    .chain(scope.fn_lexical.iter())
                    .any(|l| names.contains(l))
                {
                    return None;
                }
            }
        }
        if self.eat_kw("finally") {
            handled = true;
            self.expect_p("{")?;
            self.block_rest()?;
        }
        handled.then_some(())
    }

    fn switch_stmt(&mut self) -> Option<()> {
        self.push_scope(false);
        self.advance();
        self.expect_p("(")?;
        self.expr()?;
        self.expect_p(")")?;
        self.expect_p("{")?;
        self.switch_depth += 1;
        let mut seen_default = false;
        while !self.is_p("}") && !self.at_eof() {
            if self.eat_kw("case") {
                self.expr()?;
            } else if self.eat_kw("default") {
                if seen_default {
                    return None;
                }
                seen_default = true;
            } else {
                return None;
            }
            self.expect_p(":")?;
            while !self.is_p("}") && !self.is_kw("case") && !self.is_kw("default") && !self.at_eof()
            {
                self.stmt(false)?;
                if self.bad {
                    return None;
                }
            }
        }
        self.switch_depth -= 1;
        self.expect_p("}")?;
        self.pop_scope()?;
        Some(())
    }

    fn for_inner(&mut self) -> Option<()> {
        self.advance();
        if self.is_word("await") {
            return None;
        }
        self.expect_p("(")?;
        let decl = if self.is_kw("var") {
            self.advance();
            Some(DeclKind::Var)
        } else if self.is_kw("const") {
            self.advance();
            Some(DeclKind::Const)
        } else if self.is_word("let")
            && matches!(self.peek(1), Some(Tok::Ident(_) | Tok::Punct(Punct::LBracket | Punct::LBrace)))
        {
            self.advance();
            Some(DeclKind::Let)
        } else if self.is_word("using") && matches!(self.peek(1), Some(Tok::Ident(_))) {
            return None;
        } else if self.is_word("await") {
            return None;
        } else {
            None
        };

        if let Some(kind) = decl {
            let mut names = Vec::new();
            let first_simple = self.binding_pattern(&mut names)?;
            if self.is_kw("in") || self.is_word("of") {
                let of = self.is_word("of");
                self.advance();
                if kind != DeclKind::Var {
                    self.declare_names(kind, &names)?;
                }
                if of {
                    self.assign()?;
                } else {
                    self.expr_allow_in()?;
                }
                self.expect_p(")")?;
                return self.loop_body();
            }
            let first_has_init = self.eat_p("=");
            if first_has_init {
                self.assign_no_in()?;
            }
            let mut missing_init = !first_has_init && (!first_simple || kind == DeclKind::Const);
            if !self.strict
                && kind == DeclKind::Var
                && first_simple
                && first_has_init
                && self.is_kw("in")
            {
                return None;
            }
            while self.eat_p(",") {
                let simple = self.binding_pattern(&mut names)?;
                if self.eat_p("=") {
                    self.assign_no_in()?;
                } else if !simple || kind == DeclKind::Const {
                    missing_init = true;
                }
            }
            self.expect_p(";")?;
            if missing_init {
                return None;
            }
            self.declare_names(kind, &names)?;
            return self.finish_c_for();
        }

        if self.eat_p(";") {
            return self.finish_c_for();
        }
        let bare_async = self.is_word("async");
        let init = self.expr_no_in()?;
        if self.is_kw("in") || self.is_word("of") {
            let of = self.is_word("of");
            self.advance();
            if of {
                self.assign()?;
            } else {
                self.expr_allow_in()?;
            }
            self.expect_p(")")?;
            self.loop_body()?;
            if of && bare_async {
                return None;
            }
            return match init {
                K::Ident(Id::Normal) => Some(()),
                K::Ident(Id::EvalArgs) => (!self.strict).then_some(()),
                K::Member(_) | K::Index => Some(()),
                _ => None,
            };
        }
        self.expect_p(";")?;
        self.finish_c_for()
    }

    fn finish_c_for(&mut self) -> Option<()> {
        if !self.is_p(";") {
            self.expr()?;
        }
        self.expect_p(";")?;
        if !self.is_p(")") {
            self.expr()?;
        }
        self.expect_p(")")?;
        self.loop_body()
    }

    // ----- bindings ---------------------------------------------------------------------------

    fn binding_ident(&mut self, names: &mut Vec<Name>) -> Option<()> {
        let Tok::Ident(name) = self.kind() else {
            return None;
        };
        if name.starts_with('#')
            || (self.strict && is_strict_reserved_binding(name))
            || (self.in_async && name == "await")
            || (self.in_generator && name == "yield")
        {
            return None;
        }
        names.push(name.clone());
        self.advance();
        Some(())
    }

    /// A binding target; `true` when it is a plain identifier.
    fn binding_pattern(&mut self, names: &mut Vec<Name>) -> Option<bool> {
        match self.kind() {
            Tok::Punct(Punct::LBracket) => {
                self.nested(|s| s.array_pattern(names))?;
                Some(false)
            }
            Tok::Punct(Punct::LBrace) => {
                self.nested(|s| s.object_pattern(names))?;
                Some(false)
            }
            _ => {
                self.binding_ident(names)?;
                Some(true)
            }
        }
    }

    fn array_pattern(&mut self, names: &mut Vec<Name>) -> Option<()> {
        self.expect_p("[")?;
        while !self.is_p("]") {
            if self.is_p(",") {
                self.advance();
                continue;
            }
            if self.eat_p("...") {
                self.binding_pattern(names)?;
                break;
            }
            self.binding_pattern(names)?;
            if self.eat_p("=") {
                self.assign()?;
            }
            if !self.eat_p(",") {
                break;
            }
        }
        self.expect_p("]")
    }

    fn object_pattern(&mut self, names: &mut Vec<Name>) -> Option<()> {
        self.expect_p("{")?;
        while !self.is_p("}") {
            if self.eat_p("...") {
                self.binding_ident(names)?;
                break;
            }
            let key = self.prop_key()?;
            if self.eat_p(":") {
                self.binding_pattern(names)?;
                if self.eat_p("=") {
                    self.assign()?;
                }
            } else {
                let Pk::Ident(name) = key else {
                    return None;
                };
                self.check_shorthand(&name)?;
                names.push(name);
                if self.eat_p("=") {
                    self.assign()?;
                }
            }
            if !self.eat_p(",") {
                break;
            }
        }
        self.expect_p("}")
    }

    fn check_shorthand(&self, name: &str) -> Option<()> {
        if name.starts_with('#')
            || (self.strict
                && is_strict_reserved_binding(name)
                && name != "eval"
                && name != "arguments")
            || ((self.in_async || self.module) && name == "await")
            || (self.in_generator && name == "yield")
        {
            None
        } else {
            Some(())
        }
    }

    // ----- expressions ------------------------------------------------------------------------

    fn expr(&mut self) -> Option<K> {
        let first = self.assign()?;
        if self.is_p(",") {
            while self.eat_p(",") {
                self.assign()?;
            }
            Some(K::Other)
        } else {
            Some(first)
        }
    }
    fn expr_allow_in(&mut self) -> Option<K> {
        let saved = std::mem::replace(&mut self.no_in, false);
        let r = self.expr();
        self.no_in = saved;
        r
    }
    fn assign_allow_in(&mut self) -> Option<K> {
        let saved = std::mem::replace(&mut self.no_in, false);
        let r = self.assign();
        self.no_in = saved;
        r
    }
    fn assign_no_in(&mut self) -> Option<K> {
        let saved = std::mem::replace(&mut self.no_in, true);
        let r = self.assign();
        self.no_in = saved;
        r
    }
    fn expr_no_in(&mut self) -> Option<K> {
        let saved = std::mem::replace(&mut self.no_in, true);
        let r = self.expr();
        self.no_in = saved;
        r
    }

    fn assign(&mut self) -> Option<K> {
        if self.in_generator && self.is_word("yield") {
            return self.yield_expr();
        }
        if let Some(arrow) = self.try_arrow()? {
            return Some(arrow);
        }
        if matches!(self.kind(), Tok::Punct(Punct::LBracket | Punct::LBrace)) {
            // A literal followed by `=` is a destructuring assignment target.
            let close = self.matching_paren(self.pos)?;
            if matches!(
                self.toks.get(close + 1).map(|t| &t.kind),
                Some(Tok::Punct(Punct::Assign))
            ) {
                self.assign_pattern()?;
                self.expect_p("=")?;
                self.assign()?;
                return Some(K::Other);
            }
        }
        let left = self.cond()?;
        if let Tok::Punct(op) = self.kind() {
            if is_assign_punct(*op) {
                match left {
                    K::Ident(Id::Normal | Id::Undefined) | K::Member(_) | K::Index => {}
                    K::Ident(Id::EvalArgs) if !self.strict => {}
                    _ => return None,
                }
                self.advance();
                self.assign()?;
                return Some(K::Other);
            }
        }
        Some(left)
    }

    /// An array or object literal read as a destructuring assignment pattern.
    fn assign_pattern(&mut self) -> Option<()> {
        self.nested(|s| {
            if s.is_p("[") {
                s.array_assign_pattern()
            } else {
                s.object_assign_pattern()
            }
        })
    }

    /// A simple assignment target: an identifier or a member access.
    fn simple_target(&mut self) -> Option<()> {
        match self.lhs()? {
            K::Ident(Id::Normal) | K::Member(_) | K::Index => Some(()),
            K::Ident(Id::EvalArgs) if !self.strict => Some(()),
            _ => None,
        }
    }

    fn pattern_target(&mut self) -> Option<()> {
        if matches!(self.kind(), Tok::Punct(Punct::LBracket | Punct::LBrace)) {
            let close = self.matching_paren(self.pos)?;
            if matches!(
                self.toks.get(close + 1).map(|t| &t.kind),
                Some(Tok::Punct(Punct::Assign | Punct::Comma | Punct::RBracket | Punct::RBrace))
            ) {
                return self.assign_pattern();
            }
        }
        self.simple_target()
    }

    fn array_assign_pattern(&mut self) -> Option<()> {
        self.expect_p("[")?;
        let saved = std::mem::replace(&mut self.no_in, false);
        while !self.is_p("]") {
            if self.is_p(",") {
                self.advance();
                continue;
            }
            if self.eat_p("...") {
                self.pattern_target()?;
                if !self.is_p("]") {
                    return None;
                }
                break;
            }
            self.pattern_target()?;
            if self.eat_p("=") {
                self.assign()?;
            }
            if !self.eat_p(",") {
                break;
            }
        }
        self.no_in = saved;
        self.expect_p("]")
    }

    fn object_assign_pattern(&mut self) -> Option<()> {
        self.expect_p("{")?;
        let saved = std::mem::replace(&mut self.no_in, false);
        while !self.is_p("}") {
            if self.eat_p("...") {
                self.simple_target()?;
                if !self.is_p("}") {
                    return None;
                }
                break;
            }
            let key = self.prop_key()?;
            if self.eat_p(":") {
                self.pattern_target()?;
            } else {
                let Pk::Ident(name) = key else {
                    return None;
                };
                self.check_shorthand(&name)?;
                if (self.strict && (name == "eval" || name == "arguments"))
                    || (name == "arguments" && self.forbid_arguments)
                {
                    return None;
                }
            }
            if self.eat_p("=") {
                self.assign()?;
            }
            if !self.eat_p(",") {
                break;
            }
        }
        self.no_in = saved;
        self.expect_p("}")
    }

    fn yield_expr(&mut self) -> Option<K> {
        if self.in_params {
            return None;
        }
        self.advance();
        let delegate = !self.tok().nl_before() && self.eat_p("*");
        let no_arg = (!delegate && self.tok().nl_before())
            || matches!(
                self.kind(),
                Tok::Punct(Punct::Semi | Punct::RParen | Punct::RBracket | Punct::RBrace | Punct::Comma | Punct::Colon) | Tok::Eof
            );
        if !no_arg {
            self.nested(Self::assign)?;
        }
        Some(K::Other)
    }

    fn cond(&mut self) -> Option<K> {
        let test = self.binary(0)?;
        if self.eat_p("?") {
            self.assign_allow_in()?;
            self.expect_p(":")?;
            self.assign()?;
            Some(K::Other)
        } else {
            Some(test)
        }
    }

    fn binary(&mut self, min_prec: u8) -> Option<K> {
        self.chained(|s| s.binary_chain(min_prec))
    }

    #[inline]
    fn binary_op(&self) -> Option<(BinOp, u8, bool)> {
        Some(match self.kind() {
            Tok::Punct(p) => match p {
                Punct::Nullish => (BinOp::Nullish, 1, false),
                Punct::OrOr => (BinOp::Logical, 2, false),
                Punct::AndAnd => (BinOp::Logical, 3, false),
                Punct::Pipe => (BinOp::Other, 4, false),
                Punct::Caret => (BinOp::Other, 5, false),
                Punct::Amp => (BinOp::Other, 6, false),
                Punct::EqEq | Punct::NotEq | Punct::StrictEq | Punct::StrictNe => (BinOp::Other, 7, false),
                Punct::Lt | Punct::Gt | Punct::LtEq | Punct::GtEq => (BinOp::Other, 8, false),
                Punct::Shl | Punct::Shr | Punct::UShr => (BinOp::Other, 9, false),
                Punct::Plus | Punct::Minus => (BinOp::Other, 10, false),
                Punct::Star | Punct::Slash | Punct::Percent => (BinOp::Other, 11, false),
                Punct::Pow => (BinOp::Other, 12, true),
                _ => return None,
            },
            Tok::Keyword(Keyword::Instanceof) => (BinOp::Other, 8, false),
            Tok::Keyword(Keyword::In) if self.no_in => return None,
            Tok::Keyword(Keyword::In) => (BinOp::Other, 8, false),
            _ => return None,
        })
    }

    fn binary_chain(&mut self, min_prec: u8) -> Option<K> {
        let mut left;
        let mut left_paren;
        if let Tok::Ident(n) = self.kind() {
            if n.starts_with('#') {
                // `#x in obj`: the brand check, valid only as the left operand of `in`.
                if min_prec > 8 || self.no_in || !matches!(self.peek(1), Some(Tok::Keyword(Keyword::In))) {
                    return None;
                }
                self.private.push(n.clone());
                self.advance();
                left = K::Other;
                left_paren = false;
                self.last_paren = false;
            } else {
                left = self.unary()?;
                left_paren = self.last_paren;
            }
        } else {
            left = self.unary()?;
            left_paren = self.last_paren;
        }
        loop {
            let Some((op, prec, right_assoc)) = self.binary_op() else {
                break;
            };
            if prec < min_prec {
                break;
            }
            let nullish = op == BinOp::Nullish;
            if nullish && !left_paren && left == K::AndOr {
                return None;
            }
            self.advance();
            self.chain_link()?;
            let next_min = if right_assoc { prec } else { prec + 1 };
            let right = self.binary(next_min)?;
            if nullish && !self.last_paren && right == K::AndOr {
                return None;
            }
            left_paren = false;
            self.last_paren = false;
            left = if op == BinOp::Logical {
                K::AndOr
            } else {
                K::Other
            };
        }
        Some(left)
    }

    fn unary(&mut self) -> Option<K> {
        self.nested(Self::unary_inner)
    }

    fn unary_inner(&mut self) -> Option<K> {
        if self.in_async && self.is_word("await") {
            if self.in_params {
                return None;
            }
            self.advance();
            self.unary()?;
            if self.is_p("**") {
                return None;
            }
            return Some(K::Other);
        }
        let op = match self.kind() {
            Tok::Punct(p @ (Punct::Plus | Punct::Minus | Punct::Bang | Punct::Tilde)) => Some(p.as_str()),
            Tok::Keyword(k @ (Keyword::Typeof | Keyword::Void | Keyword::Delete)) => Some(k.as_str()),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let arg = self.unary()?;
            if op == "delete"
                && ((self.strict && matches!(arg, K::Ident(_))) || arg == K::Member(true)
                    || arg == K::OptChain)
            {
                return None;
            }
            if self.is_p("**") {
                return None;
            }
            return Some(K::Other);
        }
        if self.is_p("++") || self.is_p("--") {
            self.advance();
            let arg = self.unary()?;
            self.update_target(arg)?;
            return Some(K::Other);
        }
        let expr = self.lhs()?;
        if !self.tok().nl_before() && (self.is_p("++") || self.is_p("--")) {
            self.advance();
            self.update_target(expr)?;
            return Some(K::Other);
        }
        Some(expr)
    }

    fn update_target(&self, k: K) -> Option<()> {
        match k {
            K::Ident(Id::Normal) | K::Member(_) | K::Index => Some(()),
            K::Ident(Id::EvalArgs) if !self.strict => Some(()),
            _ => None,
        }
    }

    fn lhs(&mut self) -> Option<K> {
        self.chained(Self::lhs_chain)
    }

    fn lhs_chain(&mut self) -> Option<K> {
        let mut k = self.member_expr()?;
        let mut had_optional = false;
        loop {
            self.chain_link()?;
            match self.kind() {
                Tok::Punct(Punct::LParen) => {
                    self.args()?;
                    k = K::Call;
                }
                Tok::Punct(Punct::Dot) => {
                    self.advance();
                    let private = self.property_name()?;
                    if private && k == K::Super {
                        return None;
                    }
                    k = K::Member(private);
                }
                Tok::Punct(Punct::LBracket) => {
                    self.advance();
                    self.expr_allow_in()?;
                    self.expect_p("]")?;
                    k = K::Index;
                }
                Tok::Template(parts) => {
                    if had_optional {
                        return None;
                    }
                    self.advance();
                    self.template(parts, true)?;
                    k = K::Other;
                }
                Tok::Punct(Punct::OptChain) => {
                    had_optional = true;
                    self.advance();
                    if self.is_p("(") {
                        self.args()?;
                        k = K::Call;
                    } else if self.eat_p("[") {
                        self.expr_allow_in()?;
                        self.expect_p("]")?;
                        k = K::Index;
                    } else {
                        k = K::Member(self.property_name()?);
                    }
                }
                _ => break,
            }
        }
        Some(if had_optional { K::OptChain } else { k })
    }

    fn member_expr(&mut self) -> Option<K> {
        self.chained(Self::member_chain)
    }

    fn member_chain(&mut self) -> Option<K> {
        let mut k = if self.is_kw("new") {
            self.advance();
            if self.is_p(".") || (self.in_async && self.is_word("await")) {
                return None;
            }
            let callee = self.nested(Self::member_expr)?;
            if callee == K::OptChain {
                return None;
            }
            let had_args = self.is_p("(");
            if had_args {
                self.args()?;
            } else if self.is_p("?.") {
                return None;
            }
            K::Other
        } else {
            self.primary()?
        };
        loop {
            self.chain_link()?;
            match self.kind() {
                Tok::Punct(Punct::Dot) => {
                    self.advance();
                    let private = self.property_name()?;
                    if private && k == K::Super {
                        return None;
                    }
                    k = K::Member(private);
                }
                Tok::Template(parts) => {
                    self.advance();
                    self.template(parts, true)?;
                    k = K::Other;
                }
                Tok::Punct(Punct::LBracket) => {
                    self.advance();
                    self.expr_allow_in()?;
                    self.expect_p("]")?;
                    k = K::Index;
                }
                _ => break,
            }
        }
        Some(k)
    }

    /// A property name after `.`: any identifier or keyword; `true` for a private name.
    fn property_name(&mut self) -> Option<bool> {
        match self.kind() {
            Tok::Ident(n) => {
                let private = n.starts_with('#');
                if private {
                    self.private.push(n.clone());
                }
                self.advance();
                Some(private)
            }
            Tok::Keyword(_) => {
                self.advance();
                Some(false)
            }
            _ => None,
        }
    }

    fn args(&mut self) -> Option<()> {
        self.expect_p("(")?;
        let saved = std::mem::replace(&mut self.no_in, false);
        while !self.is_p(")") {
            self.eat_p("...");
            self.assign()?;
            if !self.eat_p(",") {
                break;
            }
        }
        self.no_in = saved;
        self.expect_p(")")
    }

    fn primary(&mut self) -> Option<K> {
        self.last_paren = false;
        match self.kind() {
            Tok::Num(_) | Tok::BigInt(_) | Tok::Str(_) => {
                self.advance();
                Some(K::Other)
            }
            Tok::Template(parts) => {
                self.advance();
                self.template(parts, false)?;
                Some(K::Other)
            }
            Tok::Regex(re) => {
                self.advance();
                crate::regex::Regex::new(&re.body, &re.flags).ok()?;
                Some(K::Other)
            }
            Tok::Keyword(Keyword::True | Keyword::False | Keyword::Null | Keyword::This) => {
                self.advance();
                Some(K::Other)
            }
            Tok::Keyword(Keyword::Super) => {
                self.advance();
                let ok = if self.is_p(".") || self.is_p("[") {
                    self.super_prop_ok
                } else {
                    self.is_p("(") && self.super_call_ok
                };
                ok.then_some(K::Super)
            }
            Tok::Keyword(Keyword::Function) => {
                self.function(false, true)?;
                Some(K::Other)
            }
            Tok::Ident(n)
                if n == "async"
                    && matches!(self.peek(1), Some(Tok::Keyword(Keyword::Function)))
                    && !self.nl_at(1) =>
            {
                self.advance();
                self.function(true, true)?;
                Some(K::Other)
            }
            Tok::Ident(_) => self.ident_ref(),
            Tok::Punct(Punct::LParen) => {
                self.advance();
                let inner = self.expr_allow_in()?;
                self.expect_p(")")?;
                self.last_paren = true;
                Some(inner)
            }
            Tok::Punct(Punct::LBracket) => {
                self.array()?;
                self.last_paren = false;
                Some(K::Other)
            }
            Tok::Punct(Punct::LBrace) => {
                self.object()?;
                self.last_paren = false;
                Some(K::Other)
            }
            _ => None,
        }
    }

    fn ident_ref(&mut self) -> Option<K> {
        let Tok::Ident(name) = self.kind() else {
            return None;
        };
        let name = name.as_str();
        let mut id = Id::Normal;
        match name {
            "arguments" => {
                if self.forbid_arguments {
                    return None;
                }
                id = Id::EvalArgs;
            }
            "eval" => id = Id::EvalArgs,
            "undefined" => id = Id::Undefined,
            "yield" | "await" => {
                if (name == "yield" && self.in_generator)
                    || (name == "await" && (self.in_async || self.module))
                {
                    return None;
                }
                self.advance();
                if self.strict && is_strict_reserved_word(name) {
                    return None;
                }
                // The lexer reads a `/` after these as the start of a regex.
                if matches!(self.kind(), Tok::Regex(_)) {
                    return None;
                }
                return Some(K::Ident(Id::Normal));
            }
            _ => {
                if name.starts_with('#') {
                    return None;
                }
            }
        }
        if self.strict && is_strict_reserved_word(name) {
            return None;
        }
        self.advance();
        Some(K::Ident(id))
    }

    fn array(&mut self) -> Option<()> {
        self.expect_p("[")?;
        let saved = std::mem::replace(&mut self.no_in, false);
        while !self.is_p("]") {
            if self.is_p(",") {
                self.advance();
                continue;
            }
            self.eat_p("...");
            self.assign()?;
            if !self.eat_p(",") {
                break;
            }
        }
        self.no_in = saved;
        self.expect_p("]")
    }

    fn prop_key(&mut self) -> Option<Pk> {
        match self.kind() {
            Tok::Ident(n) => {
                if n.starts_with('#') {
                    return None;
                }
                let n = n.clone();
                self.advance();
                Some(Pk::Ident(n))
            }
            Tok::Keyword(_) => {
                self.advance();
                Some(Pk::Keyword)
            }
            Tok::Str(s) => {
                let proto = &**s == "__proto__";
                self.advance();
                Some(Pk::Str(proto))
            }
            Tok::Num(_) => {
                self.advance();
                Some(Pk::Other)
            }
            Tok::Punct(Punct::LBracket) => {
                self.advance();
                self.assign_allow_in()?;
                self.expect_p("]")?;
                Some(Pk::Other)
            }
            _ => None,
        }
    }

    fn object(&mut self) -> Option<()> {
        self.expect_p("{")?;
        let saved = std::mem::replace(&mut self.no_in, false);
        let mut proto_seen = false;
        while !self.is_p("}") {
            if self.eat_p("...") {
                self.assign()?;
                if !self.eat_p(",") {
                    break;
                }
                continue;
            }
            let accessor = (self.is_word("get") || self.is_word("set"))
                && !matches!(self.peek(1), Some(Tok::Punct(Punct::Colon | Punct::Comma | Punct::RBrace | Punct::LParen)));
            if accessor {
                let is_get = self.is_word("get");
                self.advance();
                self.prop_key()?;
                let params = self.method(false, false)?;
                let arity_ok = if is_get {
                    params.count == 0
                } else {
                    params.count == 1 && !params.first_rest
                };
                if !arity_ok || params.strict_reserved {
                    return None;
                }
                if !self.eat_p(",") {
                    break;
                }
                continue;
            }
            let is_async = self.is_word("async")
                && !matches!(self.peek(1), Some(Tok::Punct(Punct::Colon | Punct::Comma | Punct::RBrace | Punct::LParen | Punct::Assign)));
            if is_async {
                self.advance();
                if self.tok().nl_before() && !self.is_p("*") {
                    return None;
                }
            }
            let is_gen = self.eat_p("*");
            let key = self.prop_key()?;
            if (is_async || is_gen) && !self.is_p("(") {
                return None;
            }
            if self.is_p("(") {
                self.method(is_gen, is_async)?;
            } else if self.eat_p(":") {
                let is_proto = match &key {
                    Pk::Ident(n) => n == "__proto__",
                    Pk::Str(p) => *p,
                    _ => false,
                };
                if is_proto {
                    if proto_seen {
                        return None;
                    }
                    proto_seen = true;
                }
                self.assign()?;
            } else {
                // Shorthand `{ x }`; `{ x = d }` is only meaningful as a pattern.
                let Pk::Ident(name) = key else {
                    return None;
                };
                self.check_shorthand(&name)?;
                if name == "arguments" && self.forbid_arguments {
                    return None;
                }
                if self.is_p("=") {
                    return None;
                }
            }
            if !self.eat_p(",") {
                break;
            }
        }
        self.no_in = saved;
        self.expect_p("}")
    }

    fn template(&mut self, parts: &'a [TplPart], tagged: bool) -> Option<()> {
        if tagged {
            for part in parts {
                if let TplPart::Sub(sub) = part {
                    self.sub_expr(sub)?;
                }
            }
            return Some(());
        }
        self.chained(|s| {
            for part in parts {
                s.chain_link()?;
                match part {
                    TplPart::Str { cooked, .. } => {
                        cooked.as_ref()?;
                    }
                    TplPart::Sub(sub) => s.sub_expr(sub)?,
                }
            }
            Some(())
        })
    }

    fn sub_expr(&mut self, sub: &'a SubToks) -> Option<()> {
        let (toks, pos, in_sub, no_in, last_paren) = (
            self.toks,
            self.pos,
            self.in_sub,
            self.no_in,
            self.last_paren,
        );
        self.toks = &sub.0;
        self.pos = 0;
        self.in_sub = true;
        self.no_in = false;
        let r = self.expr();
        let done = r.is_some() && self.at_eof();
        self.toks = toks;
        self.pos = pos;
        self.in_sub = in_sub;
        self.no_in = no_in;
        self.last_paren = last_paren;
        done.then_some(())
    }

    // ----- functions --------------------------------------------------------------------------

    /// `function` / `async function` (the `async` already consumed), cursor at `function`.
    fn function(&mut self, is_async: bool, is_expr: bool) -> Option<FnInfo> {
        self.advance();
        let is_gen = self.eat_p("*");
        let mut name = None;
        if let Tok::Ident(n) = self.kind() {
            if n.starts_with('#') {
                return None;
            }
            if !is_expr
                && ((n == "await" && (self.in_async || self.module))
                    || (n == "yield" && (self.in_generator || self.strict)))
            {
                return None;
            }
            if is_expr && ((is_gen && n == "yield") || (is_async && n == "await")) {
                return None;
            }
            name = Some(n.clone());
            self.advance();
        }
        let (sg, sa) = (self.in_generator, self.in_async);
        self.in_generator = is_gen;
        self.in_async = is_async;
        let sargs = std::mem::replace(&mut self.forbid_arguments, false);
        let ssuper = std::mem::replace(&mut self.super_prop_ok, false);
        let scall = std::mem::replace(&mut self.super_call_ok, false);
        let mut pnames = Vec::new();
        let params = self.params(&mut pnames)?;
        let body_strict = self.fn_body(&pnames, params.simple)?;
        self.forbid_arguments = sargs;
        self.super_prop_ok = ssuper;
        self.super_call_ok = scall;
        self.in_generator = sg;
        self.in_async = sa;
        let strict = body_strict || self.strict;
        if strict {
            if name.as_ref().is_some_and(|n| is_strict_reserved_binding(n)) {
                return None;
            }
            if pnames.iter().any(|n| is_strict_reserved_binding(n)) {
                return None;
            }
        }
        if (strict || !params.simple) && has_duplicate(&pnames) {
            return None;
        }
        Some(FnInfo {
            name,
            is_async,
            is_gen,
        })
    }

    /// An object-literal method or accessor: parameters and body, cursor at `(`.
    fn method(&mut self, is_gen: bool, is_async: bool) -> Option<Params> {
        let (sg, sa) = (self.in_generator, self.in_async);
        self.in_generator = is_gen;
        self.in_async = is_async;
        let sargs = std::mem::replace(&mut self.forbid_arguments, false);
        let ssuper = std::mem::replace(&mut self.super_prop_ok, true);
        let scall = std::mem::replace(&mut self.super_call_ok, false);
        let mut pnames = Vec::new();
        let mut params = self.params(&mut pnames)?;
        if has_duplicate(&pnames) {
            return None;
        }
        let strict_body = self.fn_body(&pnames, params.simple)?;
        self.forbid_arguments = sargs;
        self.super_prop_ok = ssuper;
        self.super_call_ok = scall;
        self.in_generator = sg;
        self.in_async = sa;
        params.strict_reserved =
            strict_body && !self.strict && pnames.iter().any(|n| is_strict_reserved_binding(n));
        Some(params)
    }

    fn params(&mut self, names: &mut Vec<Name>) -> Option<Params> {
        self.expect_p("(")?;
        let saved = std::mem::replace(&mut self.in_params, true);
        let mut info = Params {
            simple: true,
            count: 0,
            first_rest: false,
            strict_reserved: false,
        };
        while !self.is_p(")") {
            let rest = self.eat_p("...");
            let ident = self.binding_pattern(names)?;
            let mut default = false;
            if !rest && self.eat_p("=") {
                self.assign()?;
                default = true;
            }
            if info.count == 0 {
                info.first_rest = rest;
            }
            info.count += 1;
            info.simple &= ident && !rest && !default;
            if rest || !self.eat_p(",") {
                break;
            }
        }
        self.expect_p(")")?;
        self.in_params = saved;
        Some(info)
    }

    /// A `{ ... }` function body, cursor at `{`. Returns whether the body is strict (its own
    /// directive or inherited).
    fn fn_body(&mut self, pnames: &[Name], params_simple: bool) -> Option<bool> {
        self.expect_p("{")?;
        // A skimmed body has no tokens here; the full checking parse re-lexes it.
        if matches!(self.kind(), Tok::Skim) {
            return None;
        }
        let inner_strict = prologue_has_use_strict_in(self.toks, self.pos);
        if inner_strict && !params_simple {
            return None;
        }
        let saved_strict = self.strict;
        if inner_strict {
            self.strict = true;
        }
        let (siter, sswitch, sparams) = (self.iter_depth, self.switch_depth, self.in_params);
        let slabels = std::mem::take(&mut self.labels);
        let siter_labels = std::mem::take(&mut self.iter_labels);
        self.iter_depth = 0;
        self.switch_depth = 0;
        self.in_params = false;
        self.push_scope(true);
        while !self.is_p("}") {
            if self.at_eof() {
                return None;
            }
            self.stmt(false)?;
            if self.bad {
                return None;
            }
        }
        self.advance();
        let scope = self.pop_scope()?;
        self.iter_depth = siter;
        self.switch_depth = sswitch;
        self.in_params = sparams;
        self.labels = slabels;
        self.iter_labels = siter_labels;
        if scope.lexical.iter().any(|l| pnames.contains(l)) {
            return None;
        }
        let strict = self.strict;
        self.strict = saved_strict;
        Some(strict)
    }

    /// An arrow function if one starts here: `Some(Some(_))` parsed, `Some(None)` not an arrow,
    /// `None` not valid.
    fn try_arrow(&mut self) -> Option<Option<K>> {
        let async_arrow = self.is_word("async")
            && self.toks.get(self.pos + 1).is_some_and(|t| !t.nl_before())
            && matches!(self.peek(1), Some(Tok::Ident(_) | Tok::Punct(Punct::LParen)));
        let base = usize::from(async_arrow);
        if let Some(Tok::Ident(name)) = self.peek(base) {
            if matches!(self.peek(base + 1), Some(Tok::Punct(Punct::Arrow)))
                && !self.toks[self.pos + base + 1].nl_before()
            {
                let name = name.clone();
                for _ in 0..=base {
                    self.advance();
                }
                self.advance();
                if (self.strict && is_strict_reserved_binding(&name))
                    || ((async_arrow || self.in_async) && name == "await")
                    || (self.in_generator && name == "yield")
                    || name.starts_with('#')
                {
                    return None;
                }
                return self.finish_arrow(&[name], true, async_arrow).map(Some);
            }
        }
        if matches!(self.peek(base), Some(Tok::Punct(Punct::LParen))) {
            let Some(close) = self.matching_paren(self.pos + base) else {
                return Some(None);
            };
            if matches!(
                self.toks.get(close + 1).map(|t| &t.kind),
                Some(Tok::Punct(Punct::Arrow))
            ) && !self.toks[close + 1].nl_before()
            {
                if async_arrow {
                    self.advance();
                }
                let sa = self.in_async;
                self.in_async = sa || async_arrow;
                let mut names = Vec::new();
                let params = self.params(&mut names)?;
                self.in_async = sa;
                self.expect_p("=>")?;
                return self
                    .finish_arrow(&names, params.simple, async_arrow)
                    .map(Some);
            }
        }
        Some(None)
    }

    fn finish_arrow(&mut self, names: &[Name], simple: bool, is_async: bool) -> Option<K> {
        if has_duplicate(names) {
            return None;
        }
        let sa = self.in_async;
        self.in_async = is_async;
        if self.is_p("{") {
            let strict_body = self.fn_body(names, simple)?;
            if strict_body && !self.strict && names.iter().any(|n| is_strict_reserved_binding(n)) {
                return None;
            }
        } else {
            self.nested(Self::assign)?;
        }
        self.in_async = sa;
        Some(K::Other)
    }
}

struct FnInfo {
    name: Option<Name>,
    is_async: bool,
    is_gen: bool,
}

fn has_duplicate(names: &[Name]) -> bool {
    names
        .iter()
        .enumerate()
        .any(|(i, n)| names[..i].contains(n))
}
