//! Recursive-descent parser for TypeScript (and plain JavaScript) producing [`super::ast`].
//!
//! It accepts the erasable TypeScript syntax the strip mode supports plus the declarations
//! that need emit (`enum`, `namespace`, parameter properties) so the checker can explain why
//! a file cannot run. It is not a validating parser: it accepts some programs the engine will
//! reject, and it never needs to be exact about early errors. What it must be exact about is
//! byte offsets, because those key the type facts (docs/typed-tier.md §3.1).

use super::ast::*;
use super::lexer::{self, Comment, Tok, Token};
use super::types::{
    FnType, IndexSignature, ObjectType, Param, Predicate, Property, Type, TypeParam,
};
use super::{Diagnostic, LineIndex};

type PResult<T> = Result<T, Diagnostic>;

/// Parses a whole file.
pub fn parse_module(src: &str, lang: Lang) -> Result<Module, Diagnostic> {
    parse_module_strip(src, lang).map(|(m, _)| m)
}

/// What type erasure has to do to a parsed file (see [`super::strip`]): byte ranges to blank,
/// offsets where a `;` must replace the first blanked byte (ASI hazards), and the constructs
/// strip-only mode cannot run (Node's `ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX`).
#[derive(Debug, Default)]
pub struct StripPlan {
    pub erase: Vec<(u32, u32)>,
    pub semis: Vec<u32>,
    pub unsupported: Vec<(u32, u32, &'static str)>,
    /// Offsets overwritten with `)`: an arrow's return type that spans lines moves the
    /// parameter list's `)` to the type's last character, so `=>` stays on the `)` line
    /// (Node does the same).
    pub parens: Vec<u32>,
}

/// [`parse_module`], also returning the erasure plan.
pub fn parse_module_strip(src: &str, lang: Lang) -> Result<(Module, StripPlan), Diagnostic> {
    let lexed = lexer::lex(src)?;
    let mut p = Parser::new(src, lexed.tokens, lexed.comments, lang, false);
    let mut stmts = Vec::new();
    while !p.at_eof() {
        stmts.push(p.list_stmt()?);
    }
    let plan = std::mem::take(&mut p.plan);
    Ok((
        Module {
            lang,
            stmts,
            funcs: p.funcs,
            classes: p.classes,
            comments: p.comments,
        },
        plan,
    ))
}

/// Parses a standalone type expression (a TypeScript type, or a JSDoc/Closure type when
/// `jsdoc` is set).
pub fn parse_type_text(src: &str, jsdoc: bool) -> Result<Type, Diagnostic> {
    let lexed = lexer::lex(src)?;
    let mut p = Parser::new(src, lexed.tokens, lexed.comments, Lang::Ts, jsdoc);
    let ty = p.ty()?;
    if !p.at_eof() {
        return Err(p.err("Unexpected token after type expression"));
    }
    Ok(ty)
}

/// A JSDoc tag type: `...T` (rest) and `T=` (optional) are only valid at the top.
pub fn parse_jsdoc_type(src: &str) -> Result<(Type, bool, bool), Diagnostic> {
    let lexed = lexer::lex(src)?;
    let mut p = Parser::new(src, lexed.tokens, lexed.comments, Lang::Ts, true);
    let rest = p.eat_punct("...");
    let ty = p.ty()?;
    let optional = p.eat_punct("=");
    if !p.at_eof() {
        return Err(p.err("Unexpected token after type expression"));
    }
    Ok((ty, optional, rest))
}

struct Parser<'s> {
    src: &'s str,
    toks: Vec<Token>,
    pos: usize,
    lang: Lang,
    jsdoc: bool,
    funcs: Vec<Func>,
    classes: Vec<ClassNode>,
    comments: Vec<Comment>,
    fn_stack: Vec<FnId>,
    no_in: bool,
    in_generator: bool,
    in_async: bool,
    /// Set for the `whenTrue` branch of a conditional: an arrow function with a return type
    /// there must be followed by `:` (tsc's `allowReturnTypeInArrowFunction = false`).
    no_ret_arrow: bool,
    /// Inside a `declare` namespace/module/global body: everything is a declaration.
    ambient: bool,
    /// Parsing the `extends` operand of a conditional type.
    disallow_cond: bool,
    plan: StripPlan,
}

const MEMBER_MODIFIERS: &[&str] = &[
    "public",
    "private",
    "protected",
    "readonly",
    "static",
    "abstract",
    "override",
    "declare",
    "accessor",
];

const RESERVED: &[&str] = &[
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "export",
    "extends",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "null",
    "true",
    "false",
    "enum",
];

fn assign_op(p: &str) -> bool {
    matches!(
        p,
        "=" | "+="
            | "-="
            | "*="
            | "/="
            | "%="
            | "**="
            | "<<="
            | "&="
            | "|="
            | "^="
            | "&&="
            | "||="
            | "??="
    )
}

/// Punctuators that can begin an expression.
fn starts_expr_punct(p: &str) -> bool {
    matches!(
        p,
        "(" | "[" | "{" | "+" | "-" | "~" | "!" | "++" | "--" | "<" | "/" | "/=" | "@" | "..."
    )
}

fn binary_prec(op: &str) -> Option<u8> {
    Some(match op {
        "??" => 1,
        "||" => 2,
        "&&" => 3,
        "|" => 4,
        "^" => 5,
        "&" => 6,
        "==" | "!=" | "===" | "!==" => 7,
        "<" | ">" | "<=" | ">=" | "instanceof" | "in" => 8,
        "<<" | ">>" | ">>>" => 9,
        "+" | "-" => 10,
        "*" | "/" | "%" => 11,
        "**" => 12,
        _ => return None,
    })
}

impl<'s> Parser<'s> {
    fn new(
        src: &'s str,
        toks: Vec<Token>,
        comments: Vec<Comment>,
        lang: Lang,
        jsdoc: bool,
    ) -> Self {
        Parser {
            src,
            toks,
            pos: 0,
            lang,
            jsdoc,
            funcs: Vec::new(),
            classes: Vec::new(),
            comments,
            fn_stack: Vec::new(),
            no_in: false,
            in_generator: false,
            in_async: false,
            no_ret_arrow: false,
            ambient: false,
            disallow_cond: false,
            plan: StripPlan::default(),
        }
    }

    // ----- erasure plan --------------------------------------------------------------------

    /// Records `[start, end)` as TypeScript-only text (a no-op in JavaScript files).
    fn erase(&mut self, start: u32, end: u32) {
        if self.ts() && end > start {
            self.plan.erase.push((start, end));
        }
    }
    /// Erases from `start` to the end of the last consumed token.
    fn erase_from(&mut self, start: u32) {
        let end = self.prev_end();
        self.erase(start, end);
    }
    fn unsupported(&mut self, start: u32, end: u32, msg: &'static str) {
        if self.ts() && !self.ambient {
            self.plan.unsupported.push((start, end, msg));
        }
    }
    /// `: T` if present (TypeScript only), erased.
    fn annotation(&mut self) -> PResult<Option<Type>> {
        if !self.ts() || !self.is_punct(":") {
            return Ok(None);
        }
        let start = self.start();
        self.advance();
        let t = self.ty()?;
        self.erase_from(start);
        Ok(Some(t))
    }
    /// Eats `p` if present (TypeScript only), erasing it.
    fn eat_erased(&mut self, p: &str) -> bool {
        if !self.ts() || !self.is_punct(p) {
            return false;
        }
        let t = self.advance();
        self.erase(t.start, t.end);
        true
    }
    /// Re-tokenizes from the current token with `/` read as a regex (`true`) or a division.
    fn relex(&mut self, regex: bool) -> PResult<()> {
        let mut lexed = lexer::Lexed {
            tokens: std::mem::take(&mut self.toks),
            comments: std::mem::take(&mut self.comments),
        };
        let r = lexer::relex(self.src, &mut lexed, self.pos, regex);
        self.toks = lexed.tokens;
        self.comments = lexed.comments;
        r
    }

    /// Whether a statement is TypeScript-only (erased entirely by the strip).
    fn type_only(&self, s: &Stmt) -> bool {
        match s {
            Stmt::TypeAlias { .. } | Stmt::Interface { .. } => true,
            Stmt::Var { declare, .. } | Stmt::Enum { declare, .. } => *declare,
            Stmt::Func(id) => self.funcs[*id].body.is_none(),
            Stmt::Class(id) => self.classes[*id].declare,
            Stmt::Namespace { declare, body, .. } => {
                *declare || !body.iter().any(|s| self.ns_concrete(s))
            }
            Stmt::Import { type_only, .. } | Stmt::ExportNamed { type_only, .. } => *type_only,
            Stmt::Export { stmt, .. } => self.type_only(stmt),
            _ => false,
        }
    }

    /// Whether a statement makes a (non-`declare`) namespace instantiated. Node's rule, which is
    /// stricter than tsc's: only type aliases, interfaces and namespaces made of those (even `declare`d ones) keep it erasable.
    fn ns_concrete(&self, s: &Stmt) -> bool {
        match s {
            Stmt::TypeAlias { .. } | Stmt::Interface { .. } => false,
            Stmt::Namespace { body, .. } => body.iter().any(|s| self.ns_concrete(s)),
            Stmt::Export { stmt, .. } => self.ns_concrete(stmt),
            _ => true,
        }
    }

    /// Tokens that continue the previous line's expression in JavaScript when a statement
    /// between them is erased (Node inserts a `;` there).
    fn asi_hazard(&self) -> bool {
        matches!(
            self.tok(),
            Tok::Punct("(" | "[" | "+" | "-" | "/" | "/=")
                | Tok::Regex
                | Tok::Template { head: true, .. }
        )
    }

    /// A statement in a statement list: an erased one gets a `;` when the text around it would
    /// otherwise join into one expression.
    fn list_stmt(&mut self) -> PResult<Stmt> {
        let idx = self.pos;
        let start = self.start();
        let s = self.stmt()?;
        if self.ts() && idx > 0 && self.type_only(&s) && self.asi_hazard() {
            let prev = self.toks[idx - 1].start;
            if !matches!(self.toks[idx - 1].tok, Tok::Punct(";")) {
                self.plan.semis.push(start);
            } else if self.plan.erase.iter().any(|&(s, e)| s <= prev && prev < e) {
                // The `;` ending an erased statement before this one: Node keeps that one.
                self.plan.semis.push(prev);
            }
        }
        Ok(s)
    }

    /// The body of `if`/`else`/loops/labels: an erased declaration there becomes `;`.
    fn sub_stmt(&mut self) -> PResult<Stmt> {
        let start = self.start();
        let s = self.stmt()?;
        if self.ts() && self.type_only(&s) {
            self.plan.semis.push(start);
        }
        Ok(s)
    }

    // ----- token helpers -------------------------------------------------------------------

    fn ts(&self) -> bool {
        self.lang == Lang::Ts
    }
    fn cur(&self) -> &Token {
        &self.toks[self.pos]
    }
    fn tok(&self) -> &Tok {
        &self.toks[self.pos].tok
    }
    fn peek(&self, k: usize) -> &Tok {
        let i = (self.pos + k).min(self.toks.len() - 1);
        &self.toks[i].tok
    }
    fn peek_tok(&self, k: usize) -> &Token {
        let i = (self.pos + k).min(self.toks.len() - 1);
        &self.toks[i]
    }
    fn at_eof(&self) -> bool {
        matches!(self.tok(), Tok::Eof)
    }
    fn start(&self) -> u32 {
        self.cur().start
    }
    fn prev_end(&self) -> u32 {
        if self.pos == 0 {
            0
        } else {
            self.toks[self.pos - 1].end
        }
    }
    fn loc_from(&self, start: u32) -> Loc {
        Loc {
            start,
            end: self.prev_end().max(start),
        }
    }
    fn advance(&mut self) -> Token {
        let t = self.toks[self.pos].clone();
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        t
    }
    fn err(&self, msg: impl Into<String>) -> Diagnostic {
        let t = self.cur();
        LineIndex::new(self.src).diagnostic(1005, msg, t.start, t.end)
    }
    fn is_punct(&self, p: &str) -> bool {
        matches!(self.tok(), Tok::Punct(q) if *q == p)
    }
    fn peek_is_punct(&self, k: usize, p: &str) -> bool {
        matches!(self.peek(k), Tok::Punct(q) if *q == p)
    }
    fn eat_punct(&mut self, p: &str) -> bool {
        if self.is_punct(p) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn expect_punct(&mut self, p: &str) -> PResult<()> {
        if self.eat_punct(p) {
            Ok(())
        } else {
            Err(self.err(format!("'{p}' expected")))
        }
    }
    fn is_word(&self, w: &str) -> bool {
        matches!(self.tok(), Tok::Ident(n) if n == w)
    }
    fn peek_is_word(&self, k: usize, w: &str) -> bool {
        matches!(self.peek(k), Tok::Ident(n) if n == w)
    }
    fn eat_word(&mut self, w: &str) -> bool {
        if self.is_word(w) {
            self.advance();
            true
        } else {
            false
        }
    }
    fn expect_word(&mut self, w: &str) -> PResult<()> {
        if self.eat_word(w) {
            Ok(())
        } else {
            Err(self.err(format!("'{w}' expected")))
        }
    }
    fn nl_before_cur(&self) -> bool {
        self.cur().nl_before
    }
    fn peek_nl(&self, k: usize) -> bool {
        self.peek_tok(k).nl_before
    }
    fn ident(&mut self) -> PResult<(String, Loc)> {
        match self.tok().clone() {
            Tok::Ident(n) => {
                let t = self.advance();
                Ok((
                    n,
                    Loc {
                        start: t.start,
                        end: t.end,
                    },
                ))
            }
            _ => Err(self.err("Identifier expected")),
        }
    }
    fn binding_ident(&mut self) -> PResult<(String, Loc)> {
        if let Tok::Ident(n) = self.tok() {
            if RESERVED.contains(&n.as_str()) {
                return Err(self.err(format!("'{n}' is not a valid binding name")));
            }
        }
        self.ident()
    }
    fn is_ident_tok(&self, k: usize) -> bool {
        matches!(self.peek(k), Tok::Ident(n) if !RESERVED.contains(&n.as_str()))
    }
    fn semi(&mut self) -> PResult<()> {
        if self.eat_punct(";") || self.is_punct("}") || self.at_eof() || self.nl_before_cur() {
            Ok(())
        } else {
            Err(self.err("';' expected"))
        }
    }
    /// Index of the token closing the bracket at `open` (`(`/`[`/`{`), or `None`.
    fn matching(&self, open: usize) -> Option<usize> {
        let mut depth = 0i32;
        for (i, t) in self.toks.iter().enumerate().skip(open) {
            match &t.tok {
                Tok::Punct("(" | "[" | "{") => depth += 1,
                Tok::Punct(")" | "]" | "}") => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                Tok::Template { head, tail, .. } => {
                    if *head && !*tail {
                        depth += 1;
                    } else if !*head && *tail {
                        depth -= 1;
                    }
                }
                Tok::Eof => return None,
                _ => {}
            }
        }
        None
    }

    /// Runs `f`; on failure restores the position and drops any functions/classes it made.
    fn try_parse<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> Option<T> {
        let (pos, nf, nc) = (self.pos, self.funcs.len(), self.classes.len());
        let (no_in, gen, asy) = (self.no_in, self.in_generator, self.in_async);
        let depth = self.fn_stack.len();
        let (ne, ns, nu, np) = (
            self.plan.erase.len(),
            self.plan.semis.len(),
            self.plan.unsupported.len(),
            self.plan.parens.len(),
        );
        let nra = self.no_ret_arrow;
        match f(self) {
            Ok(v) => Some(v),
            Err(_) => {
                self.plan.erase.truncate(ne);
                self.plan.semis.truncate(ns);
                self.plan.unsupported.truncate(nu);
                self.plan.parens.truncate(np);
                self.no_ret_arrow = nra;
                self.pos = pos;
                self.funcs.truncate(nf);
                self.classes.truncate(nc);
                self.fn_stack.truncate(depth);
                self.no_in = no_in;
                self.in_generator = gen;
                self.in_async = asy;
                None
            }
        }
    }

    fn reserve_fn(&mut self, kind: FnKind, start: u32) -> FnId {
        let id = self.funcs.len();
        self.funcs.push(Func {
            id,
            kind,
            name: None,
            start,
            end: start,
            doc_anchor: start,
            is_async: false,
            is_generator: false,
            type_params: Vec::new(),
            this_type: None,
            params: Vec::new(),
            ret: None,
            predicate: None,
            body: None,
            class: None,
            is_static: false,
            parent: self.fn_stack.last().copied(),
            declare: false,
            loc: Loc { start, end: start },
        });
        id
    }

    /// The JSDoc comment (`/** */`) that ends right before the token at `start` with only
    /// trivia in between, following tsc's "nearest preceding comment" rule.
    fn jsdoc_before(&self, start: u32) -> Option<Comment> {
        let idx = self.comments.partition_point(|c| c.end <= start);
        let c = *self.comments.get(idx.checked_sub(1)?)?;
        let gap = self.src.get(c.end as usize..start as usize)?;
        (c.is_jsdoc(self.src) && gap.chars().all(char::is_whitespace)).then_some(c)
    }

    // ----- statements ----------------------------------------------------------------------

    fn block_body(&mut self) -> PResult<Vec<Stmt>> {
        self.expect_punct("{")?;
        let mut out = Vec::new();
        while !self.is_punct("}") {
            if self.at_eof() {
                return Err(self.err("'}' expected"));
            }
            out.push(self.list_stmt()?);
        }
        self.advance();
        Ok(out)
    }

    /// A statement; TypeScript-only declarations are erased whole.
    fn stmt(&mut self) -> PResult<Stmt> {
        let start = self.start();
        let s = self.stmt_inner()?;
        if self.ts() && self.type_only(&s) {
            self.erase_from(start);
        }
        Ok(s)
    }

    fn stmt_inner(&mut self) -> PResult<Stmt> {
        let start = self.start();
        match self.tok().clone() {
            Tok::Punct("{") => {
                let body = self.block_body()?;
                Ok(Stmt::Block(body, self.loc_from(start)))
            }
            Tok::Punct(";") => {
                self.advance();
                Ok(Stmt::Empty(self.loc_from(start)))
            }
            Tok::Punct("@") => {
                self.decorators()?;
                self.stmt()
            }
            Tok::Ident(w) => self.word_stmt(&w, start),
            _ => self.expr_stmt(start),
        }
    }

    fn word_stmt(&mut self, w: &str, start: u32) -> PResult<Stmt> {
        let next_same_line = !self.peek_nl(1);
        match w {
            "var" | "const" => {
                if w == "const" && self.peek_is_word(1, "enum") {
                    self.advance();
                    return self.enum_decl(start, false, true);
                }
                self.var_stmt(start, false)
            }
            "let"
                if self.is_ident_tok(1)
                    || self.peek_is_punct(1, "[")
                    || self.peek_is_punct(1, "{") =>
            {
                self.var_stmt(start, false)
            }
            "using" if next_same_line && self.is_ident_tok(1) && !self.peek_is_word(1, "in") => {
                self.var_stmt(start, false)
            }
            "await"
                if next_same_line
                    && self.peek_is_word(1, "using")
                    && !self.peek_nl(2)
                    && self.is_ident_tok(2) =>
            {
                self.var_stmt(start, false)
            }
            "function" => {
                let id = self.function(start, false, FnKind::Decl)?;
                Ok(Stmt::Func(id))
            }
            "async" if next_same_line && self.peek_is_word(1, "function") => {
                let id = self.function(start, true, FnKind::Decl)?;
                Ok(Stmt::Func(id))
            }
            "class" => {
                let id = self.class(start, false, false, false)?;
                Ok(Stmt::Class(id))
            }
            "if" => {
                self.advance();
                self.expect_punct("(")?;
                let test = self.expr()?;
                self.expect_punct(")")?;
                let cons = Box::new(self.sub_stmt()?);
                let alt = if self.eat_word("else") {
                    Some(Box::new(self.sub_stmt()?))
                } else {
                    None
                };
                Ok(Stmt::If {
                    test,
                    cons,
                    alt,
                    loc: self.loc_from(start),
                })
            }
            "for" => self.for_stmt(start),
            "while" => {
                self.advance();
                self.expect_punct("(")?;
                let test = self.expr()?;
                self.expect_punct(")")?;
                let body = Box::new(self.sub_stmt()?);
                Ok(Stmt::While {
                    test,
                    body,
                    loc: self.loc_from(start),
                })
            }
            "do" => {
                self.advance();
                let body = Box::new(self.sub_stmt()?);
                self.expect_word("while")?;
                self.expect_punct("(")?;
                let test = self.expr()?;
                self.expect_punct(")")?;
                self.eat_punct(";");
                Ok(Stmt::DoWhile {
                    body,
                    test,
                    loc: self.loc_from(start),
                })
            }
            "return" => {
                self.advance();
                let arg = if self.is_punct(";")
                    || self.is_punct("}")
                    || self.at_eof()
                    || self.nl_before_cur()
                {
                    None
                } else {
                    Some(self.expr()?)
                };
                self.semi()?;
                Ok(Stmt::Return(arg, self.loc_from(start)))
            }
            "break" | "continue" => {
                self.advance();
                let label = if !self.nl_before_cur() && self.is_ident_tok(0) {
                    Some(self.ident()?.0)
                } else {
                    None
                };
                self.semi()?;
                let loc = self.loc_from(start);
                Ok(if w == "break" {
                    Stmt::Break(label, loc)
                } else {
                    Stmt::Continue(label, loc)
                })
            }
            "throw" => {
                self.advance();
                let e = self.expr()?;
                self.semi()?;
                Ok(Stmt::Throw(e, self.loc_from(start)))
            }
            "try" => self.try_stmt(start),
            "switch" => self.switch_stmt(start),
            "with" => {
                self.advance();
                self.expect_punct("(")?;
                let obj = self.expr()?;
                self.expect_punct(")")?;
                let body = Box::new(self.sub_stmt()?);
                Ok(Stmt::With {
                    obj,
                    body,
                    loc: self.loc_from(start),
                })
            }
            "debugger" => {
                self.advance();
                self.semi()?;
                Ok(Stmt::Debugger(self.loc_from(start)))
            }
            "import" if !self.peek_is_punct(1, "(") && !self.peek_is_punct(1, ".") => {
                self.import_decl(start)
            }
            "export" => self.export_decl(start),
            "type" if self.ts() && next_same_line && self.is_ident_tok(1) => self.type_alias(start),
            "interface" if self.ts() && next_same_line && self.is_ident_tok(1) => {
                self.interface_decl(start)
            }
            "enum" if self.is_ident_tok(1) => self.advance_word_enum(start),
            "namespace" | "module"
                if self.ts()
                    && next_same_line
                    && (self.is_ident_tok(1) || matches!(self.peek(1), Tok::Str(_))) =>
            {
                self.namespace(start, false)
            }
            "abstract" if next_same_line && self.peek_is_word(1, "class") => {
                self.advance();
                self.erase_from(start);
                let id = self.class(start, false, true, false)?;
                Ok(Stmt::Class(id))
            }
            // `global { }` inside an ambient module.
            "global" if self.ts() && self.ambient && self.peek_is_punct(1, "{") => {
                self.namespace(start, true)
            }
            "declare" if self.ts() && next_same_line && matches!(self.peek(1), Tok::Ident(_)) => {
                self.declare(start)
            }
            _ if self.is_ident_tok(0) && self.peek_is_punct(1, ":") => {
                let (label, _) = self.ident()?;
                self.advance();
                let body = Box::new(self.sub_stmt()?);
                Ok(Stmt::Labeled {
                    label,
                    body,
                    loc: self.loc_from(start),
                })
            }
            _ => self.expr_stmt(start),
        }
    }

    fn advance_word_enum(&mut self, start: u32) -> PResult<Stmt> {
        self.enum_decl(start, false, false)
    }

    fn expr_stmt(&mut self, start: u32) -> PResult<Stmt> {
        let e = self.expr()?;
        self.semi()?;
        // `a.b = function () {}`: the statement's JSDoc belongs to the function (tsc).
        if let ExprKind::Assign { value, .. } = &e.kind {
            self.anchor_expr(value, start);
        }
        Ok(Stmt::Expr(e))
    }

    fn anchor_expr(&mut self, e: &Expr, anchor: u32) {
        match &e.kind {
            ExprKind::Func(id) => self.funcs[*id].doc_anchor = anchor,
            ExprKind::Class(id) => self.classes[*id].doc_anchor = anchor,
            _ => {}
        }
    }

    fn var_kind(&mut self) -> VarKind {
        let k = match self.tok() {
            Tok::Ident(w) if w == "var" => VarKind::Var,
            Tok::Ident(w) if w == "let" => VarKind::Let,
            Tok::Ident(w) if w == "using" => VarKind::Using,
            Tok::Ident(w) if w == "await" => {
                self.advance(); // `await using`
                VarKind::Using
            }
            _ => VarKind::Const,
        };
        self.advance();
        k
    }

    fn var_stmt(&mut self, start: u32, declare: bool) -> PResult<Stmt> {
        let kind = self.var_kind();
        let decls = self.declarators()?;
        self.semi()?;
        for d in &decls {
            if let Some(init) = &d.init {
                self.anchor_expr(init, start);
            }
        }
        Ok(Stmt::Var {
            kind,
            decls,
            declare,
            loc: self.loc_from(start),
        })
    }

    fn declarators(&mut self) -> PResult<Vec<VarDecl>> {
        let mut decls = Vec::new();
        loop {
            let pat = self.pattern()?;
            let definite = self.eat_erased("!");
            let ty = self.annotation()?;
            let init = if self.eat_punct("=") {
                Some(self.assign()?)
            } else {
                None
            };
            decls.push(VarDecl {
                pat,
                ty,
                definite,
                init,
            });
            if !self.eat_punct(",") {
                return Ok(decls);
            }
        }
    }

    fn for_stmt(&mut self, start: u32) -> PResult<Stmt> {
        self.advance();
        let is_await = self.eat_word("await");
        self.expect_punct("(")?;
        let mut init = None;
        let decl_kind = match self.tok() {
            Tok::Ident(w) if w == "var" || w == "const" => true,
            Tok::Ident(w) if w == "let" => {
                self.is_ident_tok(1) || self.peek_is_punct(1, "[") || self.peek_is_punct(1, "{")
            }
            Tok::Ident(w) if w == "using" => self.is_ident_tok(1) && !self.peek_is_word(1, "of"),
            Tok::Ident(w) if w == "await" => self.peek_is_word(1, "using") && self.is_ident_tok(2),
            _ => false,
        };
        if decl_kind {
            let init_start = self.start();
            let kind = self.var_kind();
            let pat = self.pattern()?;
            if self.is_word("of") || self.is_word("in") {
                let of = self.is_word("of");
                self.advance();
                let right = if of { self.assign()? } else { self.expr()? };
                self.expect_punct(")")?;
                let body = Box::new(self.sub_stmt()?);
                return Ok(Stmt::ForIn {
                    head: ForHead::Var(kind, pat),
                    right,
                    body,
                    of,
                    is_await,
                    loc: self.loc_from(start),
                });
            }
            // Re-parse the declarators with `in` disallowed.
            let definite = self.eat_erased("!");
            let ty = self.annotation()?;
            let saved = self.no_in;
            self.no_in = true;
            let first_init = if self.eat_punct("=") {
                Some(self.assign()?)
            } else {
                None
            };
            let mut decls = vec![VarDecl {
                pat,
                ty,
                definite,
                init: first_init,
            }];
            if self.eat_punct(",") {
                decls.extend(self.declarators()?);
            }
            self.no_in = saved;
            init = Some(Box::new(Stmt::Var {
                kind,
                decls,
                declare: false,
                loc: self.loc_from(init_start),
            }));
        } else if !self.is_punct(";") {
            let saved = self.no_in;
            self.no_in = true;
            let e = self.expr()?;
            self.no_in = saved;
            if self.is_word("of") || self.is_word("in") {
                let of = self.is_word("of");
                self.advance();
                let right = if of { self.assign()? } else { self.expr()? };
                self.expect_punct(")")?;
                let body = Box::new(self.sub_stmt()?);
                return Ok(Stmt::ForIn {
                    head: ForHead::Expr(e),
                    right,
                    body,
                    of,
                    is_await,
                    loc: self.loc_from(start),
                });
            }
            init = Some(Box::new(Stmt::Expr(e)));
        }
        self.expect_punct(";")?;
        let test = if self.is_punct(";") {
            None
        } else {
            Some(self.expr()?)
        };
        self.expect_punct(";")?;
        let update = if self.is_punct(")") {
            None
        } else {
            Some(self.expr()?)
        };
        self.expect_punct(")")?;
        let body = Box::new(self.sub_stmt()?);
        Ok(Stmt::For {
            init,
            test,
            update,
            body,
            loc: self.loc_from(start),
        })
    }

    fn try_stmt(&mut self, start: u32) -> PResult<Stmt> {
        self.advance();
        let block = self.block_body()?;
        let mut param = None;
        let mut handler = None;
        if self.eat_word("catch") {
            if self.eat_punct("(") {
                let pat = self.pattern()?;
                let ty = self.annotation()?;
                self.expect_punct(")")?;
                param = Some((pat, ty));
            }
            handler = Some(self.block_body()?);
        }
        let finalizer = if self.eat_word("finally") {
            Some(self.block_body()?)
        } else {
            None
        };
        if handler.is_none() && finalizer.is_none() {
            return Err(self.err("'catch' or 'finally' expected"));
        }
        Ok(Stmt::Try {
            block,
            param,
            handler,
            finalizer,
            loc: self.loc_from(start),
        })
    }

    fn switch_stmt(&mut self, start: u32) -> PResult<Stmt> {
        self.advance();
        self.expect_punct("(")?;
        let disc = self.expr()?;
        self.expect_punct(")")?;
        self.expect_punct("{")?;
        let mut cases = Vec::new();
        while !self.eat_punct("}") {
            let test = if self.eat_word("default") {
                None
            } else {
                self.expect_word("case")?;
                Some(self.expr()?)
            };
            self.expect_punct(":")?;
            let mut body = Vec::new();
            while !self.is_word("case") && !self.is_word("default") && !self.is_punct("}") {
                if self.at_eof() {
                    return Err(self.err("'}' expected"));
                }
                body.push(self.list_stmt()?);
            }
            cases.push((test, body));
        }
        Ok(Stmt::Switch {
            disc,
            cases,
            loc: self.loc_from(start),
        })
    }

    fn module_specifier(&mut self) -> PResult<String> {
        match self.tok().clone() {
            Tok::Str(s) => {
                self.advance();
                Ok(s)
            }
            _ => Err(self.err("Module specifier expected")),
        }
    }

    fn import_attributes(&mut self) -> PResult<()> {
        if (self.is_word("with") || self.is_word("assert")) && !self.nl_before_cur() {
            self.advance();
            let open = self.pos;
            let close = self
                .matching(open)
                .ok_or_else(|| self.err("'}' expected"))?;
            self.pos = close + 1;
        }
        Ok(())
    }

    fn import_decl(&mut self, start: u32) -> PResult<Stmt> {
        self.advance();
        let mut type_only = false;
        if self.ts()
            && self.is_word("type")
            && (self.is_ident_tok(1) && !self.peek_is_word(1, "from")
                || self.peek_is_punct(1, "{")
                || self.peek_is_punct(1, "*"))
        {
            self.advance();
            type_only = true;
        }
        if let Tok::Str(from) = self.tok().clone() {
            self.advance();
            self.import_attributes()?;
            self.semi()?;
            return Ok(Stmt::Import {
                names: Vec::new(),
                type_only,
                from,
                loc: self.loc_from(start),
            });
        }
        // `import x = require("y")` / `import x = A.B`.
        if self.is_ident_tok(0) && self.peek_is_punct(1, "=") {
            let (name, _) = self.ident()?;
            self.advance();
            self.expr()?;
            self.semi()?;
            let loc = self.loc_from(start);
            if type_only {
                // `import type x = require("y")` is erased like any type-only import.
                return Ok(Stmt::Import {
                    names: Vec::new(),
                    type_only: true,
                    from: String::new(),
                    loc,
                });
            }
            self.unsupported(
                loc.start,
                loc.end,
                "TypeScript import equals declaration is not supported in strip-only mode",
            );
            return Ok(Stmt::ImportEquals { name, loc });
        }
        let mut names = Vec::new();
        if self.is_ident_tok(0) {
            let (local, _) = self.ident()?;
            names.push(ImportName { local, type_only });
            self.eat_punct(",");
        }
        if self.eat_punct("*") {
            self.expect_word("as")?;
            let (local, _) = self.ident()?;
            names.push(ImportName { local, type_only });
        } else if self.eat_punct("{") {
            while !self.eat_punct("}") {
                let (_, local, item_type) = self.specifier()?;
                names.push(ImportName {
                    local,
                    type_only: type_only || item_type,
                });
                if !self.eat_punct(",") {
                    self.expect_punct("}")?;
                    break;
                }
            }
        }
        self.expect_word("from")?;
        let from = self.module_specifier()?;
        self.import_attributes()?;
        self.semi()?;
        Ok(Stmt::Import {
            names,
            type_only,
            from,
            loc: self.loc_from(start),
        })
    }

    /// An import/export specifier `[type] name [as alias]`, parsed the way tsc resolves the
    /// `type`/`as` ambiguities. Returns `(name, alias-or-name, type_only)`; a type-only
    /// specifier is erased together with its trailing comma.
    fn specifier(&mut self) -> PResult<(String, String, bool)> {
        let start = self.start();
        let name_like = |p: &Self, k: usize| matches!(p.peek(k), Tok::Ident(_) | Tok::Str(_));
        let mut type_only = false;
        let (name, alias);
        if self.ts()
            && self.is_word("type")
            && !self.peek_is_punct(1, ",")
            && !self.peek_is_punct(1, "}")
        {
            if self.peek_is_word(1, "as") {
                if self.peek_is_word(2, "as") {
                    if name_like(self, 3) {
                        // `type as as x`: the type-only `as`, renamed.
                        type_only = true;
                        self.advance();
                        self.advance();
                        self.advance();
                        name = "as".to_string();
                        alias = self.spec_name()?;
                    } else {
                        // `type as as`: `type` renamed `as`.
                        self.advance();
                        self.advance();
                        self.advance();
                        name = "type".to_string();
                        alias = "as".to_string();
                    }
                } else if name_like(self, 2) {
                    // `type as x`: `type` renamed.
                    self.advance();
                    self.advance();
                    name = "type".to_string();
                    alias = self.spec_name()?;
                } else {
                    // `type as`: the type-only `as`.
                    type_only = true;
                    self.advance();
                    self.advance();
                    name = "as".to_string();
                    alias = name.clone();
                }
            } else {
                type_only = true;
                self.advance();
                name = self.spec_name()?;
                alias = if self.eat_word("as") {
                    self.spec_name()?
                } else {
                    name.clone()
                };
            }
        } else {
            name = self.spec_name()?;
            alias = if self.eat_word("as") {
                self.spec_name()?
            } else {
                name.clone()
            };
        }
        if type_only {
            let end = if self.is_punct(",") {
                self.cur().end
            } else {
                self.prev_end()
            };
            self.erase(start, end);
        }
        Ok((name, alias, type_only))
    }

    fn spec_name(&mut self) -> PResult<String> {
        match self.tok().clone() {
            Tok::Str(s) => {
                self.advance();
                Ok(s)
            }
            _ => Ok(self.ident()?.0),
        }
    }

    fn export_decl(&mut self, start: u32) -> PResult<Stmt> {
        self.advance();
        if self.eat_word("default") {
            let s = self.start();
            let stmt = if self.is_word("function")
                || (self.is_word("async") && self.peek_is_word(1, "function") && !self.peek_nl(1))
            {
                let is_async = self.is_word("async");
                Some(Stmt::Func(self.function(start, is_async, FnKind::Decl)?))
            } else if self.is_word("class") {
                Some(Stmt::Class(self.class(start, false, false, false)?))
            } else if self.is_word("abstract") && self.peek_is_word(1, "class") {
                self.advance();
                self.erase_from(s);
                Some(Stmt::Class(self.class(start, false, true, false)?))
            } else if self.ts() && self.is_word("interface") && self.is_ident_tok(1) {
                Some(self.interface_decl(s)?)
            } else {
                None
            };
            if let Some(stmt) = stmt {
                return Ok(Stmt::Export {
                    stmt: Box::new(stmt),
                    default: true,
                    loc: self.loc_from(start),
                });
            }
            let expr = self.assign()?;
            self.semi()?;
            self.anchor_expr(&expr, start);
            return Ok(Stmt::ExportDefaultExpr {
                expr,
                loc: self.loc_from(start),
            });
        }
        if self.ts() && self.eat_punct("=") {
            let expr = self.expr()?;
            self.semi()?;
            let loc = self.loc_from(start);
            self.unsupported(
                loc.start,
                loc.end,
                "TypeScript export assignment is not supported in strip-only mode",
            );
            return Ok(Stmt::ExportAssign { expr, loc });
        }
        if self.ts() && self.is_word("as") && self.peek_is_word(1, "namespace") {
            self.advance();
            self.advance();
            self.ident()?;
            self.semi()?;
            // Node leaves `export as namespace X` (a .d.ts-only UMD declaration) untouched.
            return Ok(Stmt::ExportNamed {
                names: Vec::new(),
                type_only: false,
                loc: self.loc_from(start),
            });
        }
        let type_only = self.ts()
            && self.is_word("type")
            && (self.peek_is_punct(1, "{") || self.peek_is_punct(1, "*"));
        if type_only {
            self.advance();
        }
        if self.eat_punct("*") {
            if self.eat_word("as") {
                match self.tok() {
                    Tok::Str(_) => {
                        self.advance();
                    }
                    _ => {
                        self.ident()?;
                    }
                }
            }
            self.expect_word("from")?;
            self.module_specifier()?;
            self.import_attributes()?;
            self.semi()?;
            return Ok(Stmt::ExportNamed {
                names: Vec::new(),
                type_only,
                loc: self.loc_from(start),
            });
        }
        if self.eat_punct("{") {
            let mut names = Vec::new();
            while !self.eat_punct("}") {
                let (_, exported, type_only) = self.specifier()?;
                if !type_only {
                    names.push(exported);
                }
                if !self.eat_punct(",") {
                    self.expect_punct("}")?;
                    break;
                }
            }
            if self.eat_word("from") {
                self.module_specifier()?;
                self.import_attributes()?;
            }
            self.semi()?;
            return Ok(Stmt::ExportNamed {
                names,
                type_only,
                loc: self.loc_from(start),
            });
        }
        // `export <declaration>`: the declaration's own start is after `export`, but JSDoc
        // attaches to the whole statement.
        let inner_start = self.start();
        let stmt = if self.is_word("import") {
            self.import_decl(inner_start)?
        } else {
            self.stmt()?
        };
        match &stmt {
            Stmt::Func(id) => self.funcs[*id].doc_anchor = start,
            Stmt::Class(id) => self.classes[*id].doc_anchor = start,
            Stmt::Var { decls, .. } => {
                for d in decls {
                    if let Some(init) = &d.init {
                        self.anchor_expr(init, start);
                    }
                }
            }
            _ => {}
        }
        Ok(Stmt::Export {
            stmt: Box::new(stmt),
            default: false,
            loc: self.loc_from(start),
        })
    }

    fn type_alias(&mut self, start: u32) -> PResult<Stmt> {
        self.advance();
        let (name, _) = self.ident()?;
        let params = self.type_params_opt()?;
        self.expect_punct("=")?;
        let ty = self.ty()?;
        self.semi()?;
        Ok(Stmt::TypeAlias {
            name,
            params,
            ty,
            loc: self.loc_from(start),
        })
    }

    fn interface_decl(&mut self, start: u32) -> PResult<Stmt> {
        self.advance();
        let (name, _) = self.ident()?;
        let params = self.type_params_opt()?;
        let mut extends = Vec::new();
        if self.eat_word("extends") {
            loop {
                extends.push(self.type_reference()?);
                if !self.eat_punct(",") {
                    break;
                }
            }
        }
        let body = match self.object_type()? {
            Type::Object(o) => o,
            _ => ObjectType::default(),
        };
        Ok(Stmt::Interface {
            name,
            params,
            extends,
            body,
            loc: self.loc_from(start),
        })
    }

    fn enum_decl(&mut self, start: u32, declare: bool, is_const: bool) -> PResult<Stmt> {
        if !declare {
            let end = self.cur().end;
            self.unsupported(
                start,
                end,
                "TypeScript enum is not supported in strip-only mode",
            );
        }
        self.expect_word("enum")?;
        let (name, _) = self.ident()?;
        self.expect_punct("{")?;
        let mut members = Vec::new();
        while !self.eat_punct("}") {
            let m = match self.tok().clone() {
                Tok::Str(s) => {
                    self.advance();
                    s
                }
                _ => self.ident()?.0,
            };
            let init = if self.eat_punct("=") {
                Some(self.assign()?)
            } else {
                None
            };
            members.push((m, init));
            if !self.eat_punct(",") {
                self.expect_punct("}")?;
                break;
            }
        }
        Ok(Stmt::Enum {
            name,
            members,
            is_const,
            declare,
            loc: self.loc_from(start),
        })
    }

    fn namespace(&mut self, start: u32, declare: bool) -> PResult<Stmt> {
        let global = self.is_word("global");
        let module_kw = self.is_word("module");
        if !global {
            self.advance(); // namespace / module
        }
        let string_name = matches!(self.tok(), Tok::Str(_));
        let name = match self.tok().clone() {
            Tok::Str(s) => {
                self.advance();
                s
            }
            _ => {
                let mut n = self.ident()?.0;
                while self.eat_punct(".") {
                    n.push('.');
                    n.push_str(&self.ident()?.0);
                }
                n
            }
        };
        let saved = self.ambient;
        self.ambient = saved || declare;
        let body = if self.is_punct("{") {
            self.block_body()
        } else {
            self.semi().map(|_| Vec::new())
        };
        self.ambient = saved;
        let body = body?;
        if module_kw && !string_name && self.ts() {
            // Rejected even in ambient context.
            let end = self.prev_end();
            self.plan.unsupported.push((
                start,
                end,
                "`module` keyword is not supported. Use `namespace` instead.",
            ));
        } else if !declare && !global {
            let end = self.prev_end();
            if body.iter().any(|s| self.ns_concrete(s)) {
                self.unsupported(
                    start,
                    end,
                    "TypeScript namespace declaration is not supported in strip-only mode",
                );
            }
        }
        Ok(Stmt::Namespace {
            name,
            declare,
            body,
            loc: self.loc_from(start),
        })
    }

    fn declare(&mut self, start: u32) -> PResult<Stmt> {
        self.advance(); // declare
        let stmt = match self.tok().clone() {
            Tok::Ident(w) => match w.as_str() {
                "var" | "let" | "const" => {
                    if w == "const" && self.peek_is_word(1, "enum") {
                        self.advance();
                        return self.enum_decl(start, true, true);
                    }
                    return self.var_stmt(start, true);
                }
                "function" | "async" => {
                    let is_async = w == "async";
                    let id = self.function(start, is_async, FnKind::Decl)?;
                    self.funcs[id].declare = true;
                    Stmt::Func(id)
                }
                "class" => Stmt::Class(self.class(start, false, false, true)?),
                "abstract" => {
                    self.advance();
                    Stmt::Class(self.class(start, false, true, true)?)
                }
                "enum" => return self.enum_decl(start, true, false),
                "namespace" | "module" | "global" => return self.namespace(start, true),
                "type" => return self.type_alias(start),
                "interface" => return self.interface_decl(start),
                _ => return Err(self.err("Declaration expected after 'declare'")),
            },
            _ => return Err(self.err("Declaration expected after 'declare'")),
        };
        Ok(stmt)
    }

    // ----- functions -----------------------------------------------------------------------

    /// `function` declarations and expressions; the cursor is at `async` or `function`.
    fn function(&mut self, anchor: u32, is_async: bool, kind: FnKind) -> PResult<FnId> {
        let start = self.start();
        if is_async {
            self.advance();
        }
        self.expect_word("function")?;
        let is_generator = self.eat_punct("*");
        let id = self.reserve_fn(kind, start);
        let name = if self.is_ident_tok(0) || self.is_word("yield") || self.is_word("await") {
            Some(self.ident()?.0)
        } else {
            None
        };
        self.funcs[id].name = name;
        self.funcs[id].doc_anchor = anchor;
        self.function_rest(id, is_async, is_generator)?;
        Ok(id)
    }

    /// Type parameters, parameters, return type and body.
    fn function_rest(&mut self, id: FnId, is_async: bool, is_generator: bool) -> PResult<()> {
        let type_params = self.type_params_opt()?;
        self.fn_stack.push(id);
        let (sg, sa) = (self.in_generator, self.in_async);
        self.in_generator = is_generator;
        self.in_async = is_async;
        let result = (|| {
            let (this_type, params) = self.params()?;
            let (ret, predicate) = self.return_annotation()?;
            let body = if self.is_punct("{") {
                let saved = self.no_in;
                self.no_in = false;
                let b = self.block_body();
                self.no_in = saved;
                Some(Body::Block(b?))
            } else if self.ts() {
                // Overload signature, `declare function`, or abstract method.
                self.semi()?;
                None
            } else {
                return Err(self.err("'{' expected"));
            };
            Ok((this_type, params, ret, predicate, body))
        })();
        self.fn_stack.pop();
        self.in_generator = sg;
        self.in_async = sa;
        let (this_type, params, ret, predicate, body) = result?;
        let end = self.prev_end();
        let f = &mut self.funcs[id];
        f.is_async = is_async;
        f.is_generator = is_generator;
        f.type_params = type_params;
        f.this_type = this_type;
        f.params = params;
        f.ret = ret;
        f.predicate = predicate;
        f.body = body;
        f.end = end;
        f.loc.end = end;
        Ok(())
    }

    /// `( params )`, returning a TS `this` parameter separately.
    fn params(&mut self) -> PResult<(Option<Type>, Vec<ParamNode>)> {
        self.expect_punct("(")?;
        let mut this_type = None;
        let mut params = Vec::new();
        while !self.eat_punct(")") {
            if self.is_punct("@") {
                self.decorators()?;
            }
            let start = self.start();
            let mut property = false;
            while self.ts()
                && matches!(self.tok(), Tok::Ident(w) if matches!(w.as_str(), "public" | "private" | "protected" | "readonly" | "override"))
                && (self.is_ident_tok(1)
                    || self.peek_is_punct(1, "{")
                    || self.peek_is_punct(1, "["))
            {
                self.advance();
                property = true;
            }
            if property {
                let end = self.prev_end();
                self.unsupported(
                    start,
                    end,
                    "TypeScript parameter property is not supported in strip-only mode",
                );
            }
            if self.ts() && self.is_word("this") && (self.peek_is_punct(1, ":")) {
                // `this: T,` is erased with its comma.
                self.advance();
                self.advance();
                this_type = Some(self.ty()?);
                let end = self.prev_end();
                if self.eat_punct(",") {
                    self.erase_from(start);
                    continue;
                }
                self.erase(start, end);
            } else {
                let rest = self.eat_punct("...");
                let pat = self.pattern()?;
                let optional = self.eat_erased("?");
                let ty = self.annotation()?;
                let default = if self.eat_punct("=") {
                    Some(self.assign()?)
                } else {
                    None
                };
                params.push(ParamNode {
                    pat,
                    ty,
                    optional,
                    rest,
                    default,
                    property,
                    loc: self.loc_from(start),
                });
            }
            if !self.eat_punct(",") {
                self.expect_punct(")")?;
                break;
            }
        }
        Ok((this_type, params))
    }

    /// `: ReturnType` after a parameter list, if present, erased.
    fn return_annotation(&mut self) -> PResult<(Option<Type>, Option<Predicate>)> {
        if !self.ts() || !self.is_punct(":") {
            return Ok((None, None));
        }
        let start = self.start();
        self.advance();
        let r = self.return_type()?;
        self.erase_from(start);
        Ok(r)
    }

    /// A return type annotation, with type predicates.
    fn return_type(&mut self) -> PResult<(Option<Type>, Option<Predicate>)> {
        let param_like = |p: &Self, k: usize| p.is_ident_tok(k) || p.peek_is_word(k, "this");
        if self.is_word("asserts") && param_like(self, 1) && !self.peek_nl(1) {
            self.advance();
            let param = self.ident()?.0;
            let ty = if self.eat_word("is") {
                Some(self.ty()?)
            } else {
                None
            };
            return Ok((
                Some(Type::Void),
                Some(Predicate {
                    param,
                    ty,
                    asserts: true,
                }),
            ));
        }
        if param_like(self, 0) && self.peek_is_word(1, "is") && !self.peek_nl(1) {
            let param = self.ident()?.0;
            self.advance();
            let ty = self.ty()?;
            return Ok((
                Some(Type::Boolean),
                Some(Predicate {
                    param,
                    ty: Some(ty),
                    asserts: false,
                }),
            ));
        }
        Ok((Some(self.ty()?), None))
    }

    /// Binding patterns: identifiers, object and array destructuring.
    fn pattern(&mut self) -> PResult<Pattern> {
        let start = self.start();
        if self.is_punct("{") || self.is_punct("[") {
            let mut names = Vec::new();
            let mut defaults = Vec::new();
            self.destructure(&mut names, &mut defaults)?;
            return Ok(Pattern::Destructure {
                names,
                defaults,
                loc: self.loc_from(start),
            });
        }
        let (name, loc) = self.binding_ident()?;
        Ok(Pattern::Ident { name, loc })
    }

    fn destructure(
        &mut self,
        names: &mut Vec<(String, Loc)>,
        defaults: &mut Vec<Expr>,
    ) -> PResult<()> {
        let object = self.is_punct("{");
        self.advance();
        let close = if object { "}" } else { "]" };
        while !self.eat_punct(close) {
            if !object && self.is_punct(",") {
                self.advance();
                continue;
            }
            let rest = self.eat_punct("...");
            if object && !rest {
                // key [: pattern] [= default]
                let key_is_ident = self.is_ident_tok(0) && !self.peek_is_punct(1, ":");
                if key_is_ident {
                    let (n, l) = self.ident()?;
                    names.push((n, l));
                } else {
                    self.prop_key()?;
                    self.expect_punct(":")?;
                    self.binding_elem(names, defaults)?;
                }
            } else {
                self.binding_elem(names, defaults)?;
            }
            if self.eat_punct("=") {
                defaults.push(self.assign()?);
            }
            if !self.eat_punct(",") {
                self.expect_punct(close)?;
                break;
            }
        }
        Ok(())
    }

    fn binding_elem(
        &mut self,
        names: &mut Vec<(String, Loc)>,
        defaults: &mut Vec<Expr>,
    ) -> PResult<()> {
        if self.is_punct("{") || self.is_punct("[") {
            self.destructure(names, defaults)
        } else {
            let (n, l) = self.binding_ident()?;
            names.push((n, l));
            Ok(())
        }
    }

    // ----- classes -------------------------------------------------------------------------

    fn decorators(&mut self) -> PResult<()> {
        while self.eat_punct("@") {
            let e = self.primary()?;
            self.call_member(e, false)?;
        }
        Ok(())
    }

    fn class(
        &mut self,
        anchor: u32,
        is_expr: bool,
        is_abstract: bool,
        declare: bool,
    ) -> PResult<ClassId> {
        let start = self.start();
        self.expect_word("class")?;
        let id = self.classes.len();
        self.classes.push(ClassNode {
            id,
            name: None,
            start,
            end: start,
            doc_anchor: anchor,
            type_params: Vec::new(),
            extends: None,
            extends_args: Vec::new(),
            implements: Vec::new(),
            members: Vec::new(),
            is_abstract,
            declare,
            is_expr,
            parent_fn: self.fn_stack.last().copied(),
        });
        if self.is_ident_tok(0) && !self.is_word("implements") && !(self.is_word("extends")) {
            self.classes[id].name = Some(self.ident()?.0);
        }
        self.classes[id].type_params = self.type_params_opt()?;
        if self.eat_word("extends") {
            // ClassHeritage is a LeftHandSideExpression: calls are allowed (mixins).
            let base = self.primary()?;
            let base = self.call_member(base, false)?;
            self.classes[id].extends = Some(base);
            if self.ts() && self.is_punct("<") {
                self.classes[id].extends_args = self.type_args()?;
            }
        }
        if self.ts() && self.is_word("implements") {
            let start = self.start();
            self.advance();
            loop {
                let t = self.type_reference()?;
                self.classes[id].implements.push(t);
                if !self.eat_punct(",") {
                    break;
                }
            }
            self.erase_from(start);
        }
        self.expect_punct("{")?;
        let mut members = Vec::new();
        while !self.eat_punct("}") {
            if self.at_eof() {
                return Err(self.err("'}' expected"));
            }
            if let Some(m) = self.class_member(id)? {
                members.push(m);
            }
        }
        let end = self.prev_end();
        let c = &mut self.classes[id];
        c.members = members;
        c.end = end;
        Ok(id)
    }

    /// Whether the token at offset `k` ends a member name (so a modifier word there is really
    /// the member's name).
    fn member_name_ends(&self, k: usize) -> bool {
        matches!(
            self.peek(k),
            Tok::Punct("(" | "=" | ";" | ":" | "?" | "!" | "}" | "<") | Tok::Eof
        ) || self.peek_nl(k)
    }

    fn prop_key(&mut self) -> PResult<(PropKey, bool)> {
        Ok(match self.tok().clone() {
            Tok::Ident(n) | Tok::Private(n) => {
                self.advance();
                (PropKey::Ident(n), false)
            }
            Tok::Str(s) => {
                self.advance();
                (PropKey::Str(s), false)
            }
            Tok::Num(n) => {
                self.advance();
                (PropKey::Num(n), false)
            }
            Tok::BigInt(b) => {
                self.advance();
                (PropKey::Str(b), false)
            }
            Tok::Punct("[") => {
                self.advance();
                let saved = self.no_in;
                self.no_in = false;
                let e = self.assign();
                self.no_in = saved;
                let e = e?;
                self.expect_punct("]")?;
                (PropKey::Computed(Box::new(e)), true)
            }
            _ => return Err(self.err("Property name expected")),
        })
    }

    fn class_member(&mut self, class: ClassId) -> PResult<Option<Member>> {
        if self.eat_punct(";") {
            return Ok(None);
        }
        let anchor = self.start();
        let anchor_idx = self.pos;
        let m = self.class_member_inner(class, anchor)?;
        if let Some((m, erase_whole)) = m {
            if erase_whole {
                self.erase_from(anchor);
                self.plan.semis.retain(|&s| s != anchor);
                // `a = 1 \n declare b: T \n [k] = 2`: without a `;` the field initializer would
                // run into the next member. (Node emits nothing here and breaks such classes;
                // this is a deliberate difference.)
                let next_hazard = self.is_punct("[")
                    || self.is_punct("*")
                    || matches!(self.tok(), Tok::Ident(w) if w == "in" || w == "instanceof");
                if self.ts() && next_hazard {
                    let erased =
                        |p: &Self, at: u32| p.plan.erase.iter().any(|&(s, e)| s <= at && at < e);
                    let mut k = anchor_idx;
                    while k > 0 && erased(self, self.toks[k - 1].start) {
                        k -= 1;
                    }
                    if k > 0 && !matches!(self.toks[k - 1].tok, Tok::Punct(";" | "{" | "}")) {
                        self.plan.semis.push(anchor);
                    }
                }
            }
            return Ok(Some(m));
        }
        Ok(None)
    }

    /// A class member, and whether it is TypeScript-only (index signatures, `declare` fields,
    /// abstract members, overload signatures), which the strip erases whole.
    fn class_member_inner(
        &mut self,
        class: ClassId,
        anchor: u32,
    ) -> PResult<Option<(Member, bool)>> {
        let decorated = self.is_punct("@");
        if decorated {
            self.decorators()?;
        }
        let mut is_static = false;
        let mut is_abstract = false;
        let mut readonly = false;
        let mut declare = false;
        let mut accessor = false;
        // The member starts with an erased modifier (`private [k]`, `public *g()`): Node puts
        // a `;` there so the previous field cannot swallow what follows.
        let lead_erased = !decorated
            && matches!(self.tok(), Tok::Ident(w) if matches!(w.as_str(), "public" | "private" | "protected" | "readonly" | "abstract" | "override" | "declare"))
            && !self.member_name_ends(1);
        while let Tok::Ident(w) = self.tok().clone() {
            if !MEMBER_MODIFIERS.contains(&w.as_str()) || self.member_name_ends(1) {
                break;
            }
            if w == "static" && self.peek_is_punct(1, "{") {
                break;
            }
            match w.as_str() {
                "static" => is_static = true,
                "abstract" => is_abstract = true,
                "readonly" => readonly = true,
                "declare" => declare = true,
                "accessor" => accessor = true,
                _ => {}
            }
            let t = self.advance();
            if !matches!(w.as_str(), "static" | "accessor") {
                self.erase(t.start, t.end);
            }
        }
        if self.is_word("static") && self.peek_is_punct(1, "{") {
            // A static block has no `FnSource` in the engine, so it is not a keyed function;
            // functions nested in it keep the enclosing function as their parent.
            let start = self.start();
            self.advance();
            let (sa, sg) = (self.in_async, self.in_generator);
            self.in_async = false;
            self.in_generator = false;
            let body = self.block_body();
            self.in_async = sa;
            self.in_generator = sg;
            return Ok(Some((
                Member::StaticBlock(body?, self.loc_from(start)),
                false,
            )));
        }
        // Index signature.
        if self.ts() && self.is_punct("[") && self.is_ident_tok(1) && self.peek_is_punct(2, ":") {
            let start = self.start();
            self.advance();
            self.advance();
            self.advance();
            self.ty()?;
            self.expect_punct("]")?;
            if self.eat_punct(":") {
                self.ty()?;
            }
            self.semi()?;
            return Ok(Some((Member::Index(self.loc_from(start)), true)));
        }
        let member_start = self.start();
        let mut kind = FnKind::Method;
        if (self.is_word("get") || self.is_word("set"))
            && !self.member_name_ends(1)
            && !self.peek_is_punct(1, "*")
        {
            kind = if self.is_word("get") {
                FnKind::Getter
            } else {
                FnKind::Setter
            };
            self.advance();
        }
        let is_async = self.is_word("async") && !self.member_name_ends(1) && !self.peek_nl(1);
        if is_async {
            self.advance();
        }
        let is_generator = self.eat_punct("*");
        let (key, computed) = self.prop_key()?;
        if lead_erased
            && !is_static
            && !accessor
            && (computed
                || is_generator
                || matches!(&key, PropKey::Ident(n) if n == "in" || n == "instanceof"))
        {
            self.plan.semis.push(anchor);
        }
        let name = key.static_name().unwrap_or_else(|| "[computed]".into());
        let optional = self.eat_erased("?");
        if self.is_punct("(") || (self.ts() && self.is_punct("<")) {
            if kind == FnKind::Method && !is_static && name == "constructor" && !computed {
                kind = FnKind::Constructor;
            }
            let id = self.reserve_fn(kind, member_start);
            let f = &mut self.funcs[id];
            f.name = Some(name.clone());
            f.doc_anchor = anchor;
            f.class = Some(class);
            f.is_static = is_static;
            f.loc.start = anchor;
            self.function_rest(id, is_async, is_generator)?;
            let _ = optional;
            let bodiless = self.funcs[id].body.is_none();
            return Ok(Some((
                Member::Method {
                    name,
                    computed,
                    func: id,
                    is_static,
                    is_abstract,
                },
                bodiless || is_abstract,
            )));
        }
        let definite = self.eat_erased("!");
        let ty = self.annotation()?;
        let init = if self.eat_punct("=") {
            // A field initializer is its own function-like scope; `this` is the instance.
            let saved = self.no_in;
            self.no_in = false;
            let e = self.assign();
            self.no_in = saved;
            Some(e?)
        } else {
            None
        };
        self.semi()?;
        Ok(Some((
            Member::Field {
                name,
                computed,
                loc: self.loc_from(member_start),
                ty,
                optional,
                definite,
                readonly,
                is_static,
                declare,
                accessor,
                init,
                doc_anchor: anchor,
            },
            declare || is_abstract,
        )))
    }

    // ----- expressions ---------------------------------------------------------------------

    fn expr(&mut self) -> PResult<Expr> {
        let start = self.start();
        let first = self.assign()?;
        if !self.is_punct(",") {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.eat_punct(",") {
            items.push(self.assign()?);
        }
        Ok(Expr {
            kind: ExprKind::Seq(items),
            loc: self.loc_from(start),
        })
    }

    /// Composes `>`-led operators from adjacent `>` / `=` tokens.
    fn gt_op(&self) -> (&'static str, usize) {
        let adjacent = |k: usize| self.peek_tok(k).start == self.peek_tok(k - 1).end;
        let is = |k: usize, p: &str| matches!(self.peek(k), Tok::Punct(q) if *q == p);
        if is(1, ">") && adjacent(1) {
            if is(2, ">") && adjacent(2) {
                if is(3, "=") && adjacent(3) {
                    return (">>>=", 4);
                }
                return (">>>", 3);
            }
            if is(2, "=") && adjacent(2) {
                return (">>=", 3);
            }
            return (">>", 2);
        }
        if is(1, "=") && adjacent(1) {
            return (">=", 2);
        }
        (">", 1)
    }

    fn assign(&mut self) -> PResult<Expr> {
        let start = self.start();
        let allow_ret = !std::mem::take(&mut self.no_ret_arrow);
        if let Some(arrow) = self.try_arrow(allow_ret)? {
            return Ok(arrow);
        }
        if self.is_word("yield") && self.in_generator {
            self.advance();
            let delegate = self.eat_punct("*");
            let _ = delegate;
            let arg = if self.nl_before_cur()
                || matches!(
                    self.tok(),
                    Tok::Punct(")" | "]" | "}" | "," | ";" | ":") | Tok::Eof
                )
                || (self.is_word("in") && self.no_in)
            {
                None
            } else {
                Some(Box::new(self.assign()?))
            };
            return Ok(Expr {
                kind: ExprKind::Yield(arg),
                loc: self.loc_from(start),
            });
        }
        let target = self.conditional()?;
        let op: Option<(&'static str, usize)> = match self.tok() {
            Tok::Punct(p) if assign_op(p) => Some((p, 1)),
            Tok::Punct(">") => match self.gt_op() {
                (op @ (">>=" | ">>>="), n) => Some((op, n)),
                _ => None,
            },
            _ => None,
        };
        if let Some((op, n)) = op {
            for _ in 0..n {
                self.advance();
            }
            let value = self.assign()?;
            let target = match target.kind {
                ExprKind::Object(_) | ExprKind::Array(_) if op == "=" => Expr {
                    kind: ExprKind::Unsupported("destructuring assignment"),
                    loc: target.loc,
                },
                _ => target,
            };
            return Ok(Expr {
                kind: ExprKind::Assign {
                    op,
                    target: Box::new(target),
                    value: Box::new(value),
                },
                loc: self.loc_from(start),
            });
        }
        Ok(target)
    }

    /// Arrow functions (`x =>`, `async x =>`, `(…) =>`, `(…): T =>`, `<T>(…) =>`).
    fn try_arrow(&mut self, allow_ret: bool) -> PResult<Option<Expr>> {
        let start = self.start();
        let is_async = self.is_word("async")
            && !self.peek_nl(1)
            && (self.peek_is_punct(1, "(")
                || (self.ts() && self.peek_is_punct(1, "<"))
                || (self.is_ident_tok(1) && self.peek_is_punct(2, "=>")));
        let k = usize::from(is_async);
        // `x => …`
        if self.is_ident_tok(k) && self.peek_is_punct(k + 1, "=>") && !self.peek_nl(k + 1) {
            if is_async {
                self.advance();
            }
            let id = self.reserve_fn(FnKind::Arrow, start);
            let (name, loc) = self.ident()?;
            self.advance(); // =>
            self.funcs[id].params = vec![ParamNode {
                pat: Pattern::Ident { name, loc },
                ty: None,
                optional: false,
                rest: false,
                default: None,
                property: false,
                loc,
            }];
            return self.arrow_body(id, is_async, start).map(Some);
        }
        let open = self.pos + k;
        let candidate = match &self.toks[open].tok {
            Tok::Punct("(") => self.matching(open).is_some_and(|close| {
                let next = &self.toks[(close + 1).min(self.toks.len() - 1)];
                (matches!(next.tok, Tok::Punct("=>")) && !next.nl_before)
                    || (self.ts() && matches!(next.tok, Tok::Punct(":")))
            }),
            Tok::Punct("<") => self.ts(),
            _ => false,
        };
        if !candidate {
            return Ok(None);
        }
        let head = self.try_parse(|p| {
            if is_async {
                p.advance();
            }
            let id = p.reserve_fn(FnKind::Arrow, start);
            let type_params = p.type_params_opt()?;
            // The engine's arrow source starts at `async` or at the parameter list; type
            // parameters are blanked by the strip.
            if !is_async {
                p.funcs[id].start = p.start();
            }
            p.fn_stack.push(id);
            let (sa, sg) = (p.in_async, p.in_generator);
            p.in_async = is_async;
            p.in_generator = false;
            let r = (|| {
                let (this_type, params) = p.params()?;
                let close = p
                    .pos
                    .checked_sub(1)
                    .map(|k| (p.toks[k].start, p.toks[k].end));
                let (ret, predicate) = p.return_annotation()?;
                if !p.is_punct("=>") || p.nl_before_cur() {
                    return Err(p.err("'=>' expected"));
                }
                if let (Some((cs, ce)), true) = (close, ret.is_some() && p.ts()) {
                    let arrow = p.start();
                    let between = &p.src[ce as usize..arrow as usize];
                    if between.contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
                        let last_end = p.toks[p.pos - 1].end as usize;
                        let last = p.src[..last_end]
                            .char_indices()
                            .next_back()
                            .map_or(0, |c| c.0);
                        p.erase(cs, ce);
                        p.plan.parens.push(last as u32);
                    }
                }
                p.advance();
                Ok((this_type, params, ret, predicate))
            })();
            p.fn_stack.pop();
            p.in_async = sa;
            p.in_generator = sg;
            let (this_type, params, ret, predicate) = r?;
            let f = &mut p.funcs[id];
            f.type_params = type_params;
            f.this_type = this_type;
            f.params = params;
            f.ret = ret;
            f.predicate = predicate;
            Ok(id)
        });
        match head {
            Some(id) if !allow_ret && self.funcs[id].ret.is_some() => {
                // `c ? (x): T => y : z`: only an arrow when a `:` follows its body.
                let fstart = self.funcs[id].start;
                let arrow = self.try_parse(|p| {
                    let e = p.arrow_body(id, is_async, fstart)?;
                    if p.is_punct(":") {
                        Ok(e)
                    } else {
                        Err(p.err("':' expected"))
                    }
                });
                if arrow.is_none() {
                    // Forget the arrow head (its function and erasures) and reparse.
                    self.pos = self.toks.iter().position(|t| t.start >= start).unwrap_or(0);
                    self.funcs.truncate(id);
                    self.plan.erase.retain(|&(s, _)| s < start);
                    self.plan.semis.retain(|&s| s < start);
                    self.plan.unsupported.retain(|&(s, _, _)| s < start);
                    self.plan.parens.retain(|&s| s < start);
                }
                Ok(arrow)
            }
            Some(id) => {
                let fstart = self.funcs[id].start;
                self.arrow_body(id, is_async, fstart).map(Some)
            }
            None => Ok(None),
        }
    }

    fn arrow_body(&mut self, id: FnId, is_async: bool, start: u32) -> PResult<Expr> {
        self.fn_stack.push(id);
        let (sa, sg) = (self.in_async, self.in_generator);
        self.in_async = is_async;
        self.in_generator = false;
        let body = if self.is_punct("{") {
            let saved = self.no_in;
            self.no_in = false;
            let b = self.block_body();
            self.no_in = saved;
            b.map(Body::Block)
        } else {
            self.assign().map(|e| Body::Expr(Box::new(e)))
        };
        self.fn_stack.pop();
        self.in_async = sa;
        self.in_generator = sg;
        let body = body?;
        let end = self.prev_end();
        let f = &mut self.funcs[id];
        f.is_async = is_async;
        f.body = Some(body);
        f.end = end;
        f.loc = Loc {
            start: f.loc.start.min(start),
            end,
        };
        Ok(Expr {
            kind: ExprKind::Func(id),
            loc: Loc { start, end },
        })
    }

    fn conditional(&mut self) -> PResult<Expr> {
        let start = self.start();
        let test = self.binary(0)?;
        if !self.is_punct("?") {
            return Ok(test);
        }
        self.advance();
        let saved = self.no_in;
        self.no_in = false;
        self.no_ret_arrow = true;
        let cons = self.assign();
        self.no_ret_arrow = false;
        self.no_in = saved;
        let cons = cons?;
        self.expect_punct(":")?;
        let alt = self.assign()?;
        Ok(Expr {
            kind: ExprKind::Cond {
                test: Box::new(test),
                cons: Box::new(cons),
                alt: Box::new(alt),
            },
            loc: self.loc_from(start),
        })
    }

    fn binary(&mut self, min_prec: u8) -> PResult<Expr> {
        let start = self.start();
        let mut left = self.unary()?;
        loop {
            // `as` / `satisfies` bind like relational operators.
            if self.ts()
                && !self.nl_before_cur()
                && (self.is_word("as") || self.is_word("satisfies"))
                && 8 >= min_prec
            {
                let is_as = self.is_word("as");
                let as_start = self.start();
                self.advance();
                let kind = if is_as && self.is_word("const") {
                    self.advance();
                    ExprKind::As {
                        expr: Box::new(left),
                        ty: None,
                    }
                } else {
                    let ty = self.ty()?;
                    if is_as {
                        ExprKind::As {
                            expr: Box::new(left),
                            ty: Some(ty),
                        }
                    } else {
                        ExprKind::Satisfies {
                            expr: Box::new(left),
                            ty,
                        }
                    }
                };
                self.erase_from(as_start);
                // `a as T` ends the expression; a `(`/`[`/template on the next line would
                // join it in JavaScript.
                if self.nl_before_cur()
                    && matches!(
                        self.tok(),
                        Tok::Punct("(" | "[") | Tok::Template { head: true, .. }
                    )
                {
                    self.plan.semis.push(as_start);
                }
                left = Expr {
                    kind,
                    loc: self.loc_from(start),
                };
                continue;
            }
            if matches!(self.tok(), Tok::Regex) {
                // In operator position a `/` is a division.
                self.relex(false)?;
            }
            let (op, ntok): (&'static str, usize) = match self.tok() {
                Tok::Punct(">") => self.gt_op(),
                Tok::Punct(p) => (p, 1),
                Tok::Ident(w) if w == "instanceof" => ("instanceof", 1),
                Tok::Ident(w) if w == "in" && !self.no_in => ("in", 1),
                _ => break,
            };
            let Some(prec) = binary_prec(op) else { break };
            if prec < min_prec {
                break;
            }
            for _ in 0..ntok {
                self.advance();
            }
            let next_min = if op == "**" { prec } else { prec + 1 };
            let right = self.binary(next_min)?;
            left = Expr {
                kind: ExprKind::Binary {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                loc: self.loc_from(start),
            };
        }
        Ok(left)
    }

    fn unary(&mut self) -> PResult<Expr> {
        let start = self.start();
        let op: Option<&'static str> = match self.tok() {
            Tok::Punct(p @ ("!" | "~" | "+" | "-")) => Some(p),
            Tok::Ident(w) if w == "typeof" => Some("typeof"),
            Tok::Ident(w) if w == "void" => Some("void"),
            Tok::Ident(w) if w == "delete" => Some("delete"),
            _ => None,
        };
        if let Some(op) = op {
            self.advance();
            let arg = self.unary()?;
            return Ok(Expr {
                kind: ExprKind::Unary {
                    op,
                    arg: Box::new(arg),
                },
                loc: self.loc_from(start),
            });
        }
        if let Tok::Punct(op @ ("++" | "--")) = self.tok() {
            let op: &'static str = op;
            self.advance();
            let arg = self.unary()?;
            return Ok(Expr {
                kind: ExprKind::Update {
                    op,
                    prefix: true,
                    arg: Box::new(arg),
                },
                loc: self.loc_from(start),
            });
        }
        // `await` in async code, or top-level await in a module; an identifier elsewhere.
        let top_level_await = self.fn_stack.is_empty()
            && !self.peek_nl(1)
            && !matches!(
                self.peek(1),
                Tok::Punct(")" | "]" | "}" | "," | ";" | "=" | ":" | "." | "?." | "=>") | Tok::Eof
            );
        if self.is_word("await") && (self.in_async || top_level_await) {
            self.advance();
            let arg = self.unary()?;
            return Ok(Expr {
                kind: ExprKind::Await(Box::new(arg)),
                loc: self.loc_from(start),
            });
        }
        if self.ts() && self.is_punct("<") {
            // `<T>expr` (generic arrows were tried in `assign`).
            let t = self.cur().clone();
            self.unsupported(
                t.start,
                t.end,
                "The angle-bracket syntax for type assertions, `<T>expr`, is not supported in type strip mode. Instead, use the 'as' syntax: `expr as T`.",
            );
            self.advance();
            let ty = self.ty()?;
            self.expect_punct(">")?;
            let expr = self.unary()?;
            return Ok(Expr {
                kind: ExprKind::TypeAssert {
                    ty,
                    expr: Box::new(expr),
                },
                loc: self.loc_from(start),
            });
        }
        let e = self.postfix()?;
        Ok(e)
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let start = self.start();
        let e = self.lhs()?;
        if let Tok::Punct(op @ ("++" | "--")) = self.tok() {
            if !self.nl_before_cur() {
                let op: &'static str = op;
                self.advance();
                return Ok(Expr {
                    kind: ExprKind::Update {
                        op,
                        prefix: false,
                        arg: Box::new(e),
                    },
                    loc: self.loc_from(start),
                });
            }
        }
        Ok(e)
    }

    fn lhs(&mut self) -> PResult<Expr> {
        let start = self.start();
        if self.is_word("new") {
            self.advance();
            if self.eat_punct(".") {
                self.ident()?;
                let e = Expr {
                    kind: ExprKind::MetaProp,
                    loc: self.loc_from(start),
                };
                return self.call_member(e, false);
            }
            let callee = if self.is_word("new") {
                self.lhs_new_callee()?
            } else {
                let p = self.primary()?;
                self.call_member(p, true)?
            };
            let type_args = if self.ts() && self.is_punct("<") {
                self.try_parse(|p| p.type_args()).unwrap_or_default()
            } else {
                Vec::new()
            };
            let args = if self.is_punct("(") {
                self.args()?
            } else {
                Vec::new()
            };
            let e = Expr {
                kind: ExprKind::New {
                    callee: Box::new(callee),
                    args,
                    type_args,
                },
                loc: self.loc_from(start),
            };
            return self.call_member(e, false);
        }
        let p = self.primary()?;
        self.call_member(p, false)
    }

    fn lhs_new_callee(&mut self) -> PResult<Expr> {
        // `new new X()()`: the inner `new X()` is the callee.
        let start = self.start();
        self.advance();
        let p = self.primary()?;
        let callee = self.call_member(p, true)?;
        let args = if self.is_punct("(") {
            self.args()?
        } else {
            Vec::new()
        };
        Ok(Expr {
            kind: ExprKind::New {
                callee: Box::new(callee),
                args,
                type_args: Vec::new(),
            },
            loc: self.loc_from(start),
        })
    }

    fn args(&mut self) -> PResult<Vec<Expr>> {
        self.expect_punct("(")?;
        let saved = self.no_in;
        self.no_in = false;
        let mut args = Vec::new();
        let r = (|| {
            while !self.eat_punct(")") {
                let start = self.start();
                if self.eat_punct("...") {
                    let e = self.assign()?;
                    args.push(Expr {
                        kind: ExprKind::Spread(Box::new(e)),
                        loc: self.loc_from(start),
                    });
                } else {
                    args.push(self.assign()?);
                }
                if !self.eat_punct(",") {
                    self.expect_punct(")")?;
                    break;
                }
            }
            Ok(())
        })();
        self.no_in = saved;
        r.map(|_| args)
    }

    /// Member accesses, calls, tagged templates and TS postfix operators after a primary.
    /// With `no_call`, stops before a call (for `new` callees).
    fn call_member(&mut self, mut e: Expr, no_call: bool) -> PResult<Expr> {
        let start = e.loc.start;
        loop {
            match self.tok().clone() {
                Tok::Punct(".") => {
                    self.advance();
                    let t = self.advance();
                    let name = match t.tok {
                        Tok::Ident(n) | Tok::Private(n) => n,
                        _ => return Err(self.err("Identifier expected")),
                    };
                    e = Expr {
                        kind: ExprKind::Member {
                            obj: Box::new(e),
                            name,
                            name_loc: Loc {
                                start: t.start,
                                end: t.end,
                            },
                            optional: false,
                        },
                        loc: self.loc_from(start),
                    };
                }
                Tok::Punct("?.") => {
                    if no_call {
                        break;
                    }
                    self.advance();
                    let type_args = if self.ts() && self.is_punct("<") {
                        self.type_args()?
                    } else {
                        Vec::new()
                    };
                    if self.is_punct("(") {
                        let args = self.args()?;
                        e = Expr {
                            kind: ExprKind::Call {
                                callee: Box::new(e),
                                args,
                                optional: true,
                                type_args,
                            },
                            loc: self.loc_from(start),
                        };
                    } else if self.eat_punct("[") {
                        let index = self.expr()?;
                        self.expect_punct("]")?;
                        e = Expr {
                            kind: ExprKind::Index {
                                obj: Box::new(e),
                                index: Box::new(index),
                                optional: true,
                            },
                            loc: self.loc_from(start),
                        };
                    } else {
                        let t = self.advance();
                        let name = match t.tok {
                            Tok::Ident(n) | Tok::Private(n) => n,
                            _ => return Err(self.err("Identifier expected")),
                        };
                        e = Expr {
                            kind: ExprKind::Member {
                                obj: Box::new(e),
                                name,
                                name_loc: Loc {
                                    start: t.start,
                                    end: t.end,
                                },
                                optional: true,
                            },
                            loc: self.loc_from(start),
                        };
                    }
                }
                Tok::Punct("[") => {
                    self.advance();
                    let saved = self.no_in;
                    self.no_in = false;
                    let index = self.expr();
                    self.no_in = saved;
                    let index = index?;
                    self.expect_punct("]")?;
                    e = Expr {
                        kind: ExprKind::Index {
                            obj: Box::new(e),
                            index: Box::new(index),
                            optional: false,
                        },
                        loc: self.loc_from(start),
                    };
                }
                Tok::Punct("(") if !no_call => {
                    let args = self.args()?;
                    e = Expr {
                        kind: ExprKind::Call {
                            callee: Box::new(e),
                            args,
                            optional: false,
                            type_args: Vec::new(),
                        },
                        loc: self.loc_from(start),
                    };
                }
                Tok::Template { head: true, .. } => {
                    let parts = self.template()?;
                    e = Expr {
                        kind: ExprKind::TaggedTemplate(Box::new(e), parts),
                        loc: self.loc_from(start),
                    };
                }
                Tok::Punct("!") if self.ts() && !self.nl_before_cur() => {
                    let t = self.advance();
                    self.erase(t.start, t.end);
                    e = Expr {
                        kind: ExprKind::NonNull(Box::new(e)),
                        loc: self.loc_from(start),
                    };
                }
                Tok::Punct("<") if self.ts() && !no_call => {
                    // `f<T>(x)` / `` f<T>`…` `` / `f<T>` (an instantiation expression): type
                    // arguments only when what follows cannot continue a comparison (tsc's
                    // `canFollowTypeArgumentsInExpression`).
                    let args = self.try_parse(|p| {
                        let a = p.type_args()?;
                        if p.is_punct("(")
                            || matches!(p.tok(), Tok::Template { head: true, .. })
                            || p.can_follow_type_args()
                        {
                            Ok(a)
                        } else {
                            Err(p.err("not a call"))
                        }
                    });
                    let Some(type_args) = args else { break };
                    if self.is_punct("(") {
                        let args = self.args()?;
                        e = Expr {
                            kind: ExprKind::Call {
                                callee: Box::new(e),
                                args,
                                optional: false,
                                type_args,
                            },
                            loc: self.loc_from(start),
                        };
                    }
                }
                _ => break,
            }
        }
        Ok(e)
    }

    /// After `f<T>`: whether the type arguments stand alone (an instantiation expression)
    /// rather than `<`/`>` being comparisons.
    fn can_follow_type_args(&self) -> bool {
        match self.tok() {
            Tok::Punct("<" | ">" | "+" | "-") => false,
            _ if self.nl_before_cur() => true,
            Tok::Punct(p) => binary_prec(p).is_some() || !starts_expr_punct(p),
            Tok::Ident(w) => matches!(w.as_str(), "in" | "instanceof" | "as" | "satisfies"),
            Tok::Eof => true,
            _ => false,
        }
    }

    fn template(&mut self) -> PResult<Vec<Expr>> {
        let mut parts = Vec::new();
        loop {
            let t = self.advance();
            let Tok::Template { tail, .. } = t.tok else {
                return Err(self.err("Template expected"));
            };
            if tail {
                return Ok(parts);
            }
            let saved = self.no_in;
            self.no_in = false;
            let e = self.expr();
            self.no_in = saved;
            parts.push(e?);
            if !matches!(self.tok(), Tok::Template { head: false, .. }) {
                return Err(self.err("'}' expected"));
            }
        }
    }

    fn primary(&mut self) -> PResult<Expr> {
        let start = self.start();
        let lit = |p: &Self, kind: ExprKind| Expr {
            kind,
            loc: p.loc_from(start),
        };
        match self.tok().clone() {
            Tok::Num(n) => {
                self.advance();
                Ok(lit(self, ExprKind::Num(n)))
            }
            Tok::Str(s) => {
                self.advance();
                Ok(lit(self, ExprKind::Str(s)))
            }
            Tok::BigInt(b) => {
                self.advance();
                Ok(lit(self, ExprKind::BigInt(b)))
            }
            Tok::Regex => {
                self.advance();
                Ok(lit(self, ExprKind::Regex))
            }
            Tok::Punct("/" | "/=") => {
                // In operand position a `/` starts a regular expression.
                self.relex(true)?;
                if !matches!(self.tok(), Tok::Regex) {
                    return Err(self.err("Unterminated regular expression"));
                }
                self.advance();
                Ok(lit(self, ExprKind::Regex))
            }
            Tok::Template { .. } => {
                let parts = self.template()?;
                Ok(lit(self, ExprKind::Template(parts)))
            }
            Tok::Private(n) => {
                self.advance();
                Ok(lit(self, ExprKind::Ident(n)))
            }
            Tok::Punct("(") => {
                let cast = if self.lang == Lang::Js {
                    self.jsdoc_before(start)
                        .and_then(|c| super::jsdoc::cast_type(self.src, c))
                } else {
                    None
                };
                self.advance();
                let saved = self.no_in;
                self.no_in = false;
                let e = self.expr();
                self.no_in = saved;
                let e = e?;
                self.expect_punct(")")?;
                let kind = match cast {
                    Some(ty) => ExprKind::JsDocCast {
                        ty,
                        expr: Box::new(e),
                    },
                    None => ExprKind::Paren(Box::new(e)),
                };
                Ok(lit(self, kind))
            }
            Tok::Punct("[") => self.array_literal(),
            Tok::Punct("{") => self.object_literal(),
            Tok::Punct("@") => {
                self.decorators()?;
                let id = self.class(start, true, false, false)?;
                Ok(lit(self, ExprKind::Class(id)))
            }
            Tok::Ident(w) => match w.as_str() {
                "function" => {
                    let id = self.function(start, false, FnKind::Expr)?;
                    Ok(lit(self, ExprKind::Func(id)))
                }
                "async" if self.peek_is_word(1, "function") && !self.peek_nl(1) => {
                    let id = self.function(start, true, FnKind::Expr)?;
                    Ok(lit(self, ExprKind::Func(id)))
                }
                "class" => {
                    let id = self.class(start, true, false, false)?;
                    Ok(lit(self, ExprKind::Class(id)))
                }
                "this" => {
                    self.advance();
                    Ok(lit(self, ExprKind::This))
                }
                "super" => {
                    self.advance();
                    Ok(lit(self, ExprKind::Super))
                }
                "null" => {
                    self.advance();
                    Ok(lit(self, ExprKind::Null))
                }
                "true" | "false" => {
                    self.advance();
                    Ok(lit(self, ExprKind::Bool(w == "true")))
                }
                "import" => {
                    self.advance();
                    if self.eat_punct(".") {
                        self.ident()?;
                        return Ok(lit(self, ExprKind::MetaProp));
                    }
                    let mut args = self.args()?;
                    let arg = if args.is_empty() {
                        return Err(self.err("import() needs an argument"));
                    } else {
                        args.remove(0)
                    };
                    Ok(lit(self, ExprKind::ImportCall(Box::new(arg))))
                }
                _ => {
                    if RESERVED.contains(&w.as_str()) && w != "new" {
                        return Err(self.err(format!("Unexpected keyword '{w}'")));
                    }
                    self.advance();
                    Ok(lit(self, ExprKind::Ident(w)))
                }
            },
            _ => Err(self.err("Expression expected")),
        }
    }

    fn array_literal(&mut self) -> PResult<Expr> {
        let start = self.start();
        self.advance();
        let saved = self.no_in;
        self.no_in = false;
        let mut items = Vec::new();
        let r = (|| {
            while !self.eat_punct("]") {
                if self.eat_punct(",") {
                    items.push(None);
                    continue;
                }
                let s = self.start();
                let e = if self.eat_punct("...") {
                    let e = self.assign()?;
                    Expr {
                        kind: ExprKind::Spread(Box::new(e)),
                        loc: self.loc_from(s),
                    }
                } else {
                    self.assign()?
                };
                items.push(Some(e));
                if !self.eat_punct(",") {
                    self.expect_punct("]")?;
                    break;
                }
            }
            Ok(())
        })();
        self.no_in = saved;
        r?;
        Ok(Expr {
            kind: ExprKind::Array(items),
            loc: self.loc_from(start),
        })
    }

    fn object_literal(&mut self) -> PResult<Expr> {
        let start = self.start();
        self.advance();
        let saved = self.no_in;
        self.no_in = false;
        let mut props = Vec::new();
        let r = (|| {
            while !self.eat_punct("}") {
                props.push(self.object_prop()?);
                if !self.eat_punct(",") {
                    self.expect_punct("}")?;
                    break;
                }
            }
            Ok(())
        })();
        self.no_in = saved;
        r?;
        Ok(Expr {
            kind: ExprKind::Object(props),
            loc: self.loc_from(start),
        })
    }

    fn object_prop(&mut self) -> PResult<Prop> {
        let anchor = self.start();
        if self.eat_punct("...") {
            return Ok(Prop::Spread(self.assign()?));
        }
        let mut kind = FnKind::Method;
        if (self.is_word("get") || self.is_word("set"))
            && !matches!(self.peek(1), Tok::Punct("," | ":" | "}" | "(" | "="))
        {
            kind = if self.is_word("get") {
                FnKind::Getter
            } else {
                FnKind::Setter
            };
            self.advance();
        }
        let is_async = kind == FnKind::Method
            && self.is_word("async")
            && !matches!(self.peek(1), Tok::Punct("," | ":" | "}" | "(" | "="))
            && !self.peek_nl(1);
        if is_async {
            self.advance();
        }
        let is_generator = self.eat_punct("*");
        let key_start = self.start();
        let shorthand_name = match self.tok() {
            Tok::Ident(n) => Some(n.clone()),
            _ => None,
        };
        let (key, _) = self.prop_key()?;
        if self.is_punct("(")
            || (self.ts() && self.is_punct("<"))
            || kind != FnKind::Method
            || is_async
            || is_generator
        {
            let id = self.reserve_fn(kind, anchor);
            self.funcs[id].name = key.static_name();
            self.function_rest(id, is_async, is_generator)?;
            return Ok(Prop::Method { key, func: id });
        }
        if self.eat_punct(":") {
            let value = self.assign()?;
            self.anchor_expr(&value, anchor);
            return Ok(Prop::KeyValue { key, value });
        }
        let Some(name) = shorthand_name else {
            return Err(self.err("':' expected"));
        };
        let loc = self.loc_from(key_start);
        if self.eat_punct("=") {
            // Cover grammar for a destructuring target (`({ a = 1 } = obj)`).
            self.assign()?;
        }
        Ok(Prop::Shorthand { name, loc })
    }

    // ----- types ---------------------------------------------------------------------------

    fn type_params_opt(&mut self) -> PResult<Vec<TypeParam>> {
        if !self.ts() || !self.is_punct("<") {
            return Ok(Vec::new());
        }
        let start = self.start();
        let out = self.type_params_list()?;
        self.erase_from(start);
        Ok(out)
    }

    fn type_params_list(&mut self) -> PResult<Vec<TypeParam>> {
        self.advance();
        let mut out = Vec::new();
        while !self.eat_punct(">") {
            // `const T`, `in T`, `out T` modifiers.
            while matches!(self.tok(), Tok::Ident(w) if matches!(w.as_str(), "const" | "in" | "out"))
                && self.is_ident_tok(1)
            {
                self.advance();
            }
            let (name, _) = self.ident()?;
            let constraint = if self.eat_word("extends") {
                Some(self.ty()?)
            } else {
                None
            };
            let default = if self.eat_punct("=") {
                Some(self.ty()?)
            } else {
                None
            };
            out.push(TypeParam {
                name,
                constraint,
                default,
            });
            if !self.eat_punct(",") {
                self.expect_punct(">")?;
                break;
            }
        }
        Ok(out)
    }

    /// `<T, U>` (erased).
    fn type_args(&mut self) -> PResult<Vec<Type>> {
        let start = self.start();
        self.expect_punct("<")?;
        let mut out = Vec::new();
        while !self.eat_punct(">") {
            out.push(self.ty()?);
            if !self.eat_punct(",") {
                self.expect_punct(">")?;
                break;
            }
        }
        self.erase_from(start);
        Ok(out)
    }

    fn type_reference(&mut self) -> PResult<Type> {
        let mut name = self.ident()?.0;
        while self.eat_punct(".") {
            name.push('.');
            name.push_str(&self.ident()?.0);
        }
        let arguments = if self.is_punct("<") {
            self.type_args()?
        } else {
            Vec::new()
        };
        Ok(Type::Reference { name, arguments })
    }

    /// A full type, including function, constructor and conditional types.
    pub(crate) fn ty(&mut self) -> PResult<Type> {
        self.ty_cond(true)
    }

    /// A type; `allow_cond` is false for the `extends` operand of a conditional type, which
    /// cannot itself be a conditional type (tsc's `disallowConditionalTypes` context).
    fn ty_cond(&mut self, allow_cond: bool) -> PResult<Type> {
        let saved = self.disallow_cond;
        self.disallow_cond = !allow_cond;
        let r = self.ty_cond_inner(allow_cond);
        self.disallow_cond = saved;
        r
    }

    fn ty_cond_inner(&mut self, allow_cond: bool) -> PResult<Type> {
        if self.jsdoc && self.is_word("function") && self.peek_is_punct(1, "(") {
            return self.jsdoc_function_type();
        }
        if self.is_punct("<") {
            let type_params = self.type_params_opt()?;
            return self.fn_type(type_params, false);
        }
        if self.is_word("new") || (self.is_word("abstract") && self.peek_is_word(1, "new")) {
            self.eat_word("abstract");
            self.advance();
            let type_params = self.type_params_opt()?;
            return self.fn_type(type_params, true);
        }
        if self.is_punct("(") && self.is_fn_type_start() {
            return self.fn_type(Vec::new(), false);
        }
        let t = self.union_ty()?;
        if allow_cond && self.is_word("extends") && !self.nl_before_cur() {
            // Conditional type: out of scope.
            self.advance();
            self.ty_cond(false)?;
            self.expect_punct("?")?;
            self.ty()?;
            self.expect_punct(":")?;
            self.ty()?;
            return Ok(Type::Opaque("conditional type".into()));
        }
        Ok(t)
    }

    /// At `(`: whether a function type starts here (tsc's
    /// `isUnambiguouslyStartOfFunctionType`), not a parenthesized type.
    fn is_fn_type_start(&self) -> bool {
        if let Tok::Punct(")" | "...") = self.peek(1) {
            return true;
        }
        let mut k = 1;
        // Parameter modifiers.
        while matches!(self.peek(k), Tok::Ident(w) if matches!(w.as_str(), "public" | "private" | "protected" | "readonly"))
            && matches!(self.peek(k + 1), Tok::Ident(_) | Tok::Punct("[" | "{"))
        {
            k += 1;
        }
        match self.peek(k) {
            Tok::Ident(_) => k += 1,
            Tok::Punct("[" | "{") => match self.matching(self.pos + k) {
                Some(close) => k = close + 1 - self.pos,
                None => return false,
            },
            _ => return false,
        }
        match self.peek(k) {
            Tok::Punct(":" | "," | "?" | "=") => true,
            Tok::Punct(")") => self.peek_is_punct(k + 1, "=>"),
            _ => false,
        }
    }

    fn fn_type(&mut self, type_params: Vec<TypeParam>, construct: bool) -> PResult<Type> {
        let (this, params) = self.params()?;
        self.expect_punct("=>")?;
        let (ret, predicate) = self.return_type()?;
        Ok(Type::Function(Box::new(FnType {
            type_params,
            this,
            params: params_to_types(&params),
            ret: ret.unwrap_or(Type::Void),
            predicate,
            construct,
        })))
    }

    fn jsdoc_function_type(&mut self) -> PResult<Type> {
        self.advance(); // function
        self.expect_punct("(")?;
        let mut params = Vec::new();
        let mut this = None;
        let mut construct = false;
        while !self.eat_punct(")") {
            if (self.is_word("this") || self.is_word("new")) && self.peek_is_punct(1, ":") {
                let is_new = self.is_word("new");
                self.advance();
                self.advance();
                let t = self.ty()?;
                if is_new {
                    construct = true;
                } else {
                    this = Some(t);
                }
            } else {
                let rest = self.eat_punct("...");
                let ty = self.ty()?;
                let optional = self.eat_punct("=");
                params.push(Param {
                    name: format!("p{}", params.len()),
                    ty: if rest { Type::Array(Box::new(ty)) } else { ty },
                    optional,
                    rest,
                });
            }
            if !self.eat_punct(",") {
                self.expect_punct(")")?;
                break;
            }
        }
        let ret = if self.eat_punct(":") {
            self.ty()?
        } else {
            Type::Void
        };
        Ok(Type::Function(Box::new(FnType {
            type_params: Vec::new(),
            this,
            params,
            ret,
            predicate: None,
            construct,
        })))
    }

    fn union_ty(&mut self) -> PResult<Type> {
        self.eat_punct("|");
        let mut members = vec![self.intersection_ty()?];
        while self.eat_punct("|") {
            members.push(self.intersection_ty()?);
        }
        Ok(if members.len() == 1 {
            members.pop().unwrap()
        } else {
            Type::Union(members)
        })
    }

    fn intersection_ty(&mut self) -> PResult<Type> {
        self.eat_punct("&");
        let mut members = vec![self.operator_ty()?];
        while self.eat_punct("&") {
            members.push(self.operator_ty()?);
        }
        Ok(if members.len() == 1 {
            members.pop().unwrap()
        } else {
            Type::Intersection(members)
        })
    }

    fn operator_ty(&mut self) -> PResult<Type> {
        if self.is_word("keyof")
            && !matches!(
                self.peek(1),
                Tok::Punct(")" | "]" | "," | ">" | ";" | "=" | "|" | "&" | "}" | "?" | ":" | ".")
                    | Tok::Eof
            )
        {
            self.advance();
            self.operator_ty()?;
            return Ok(Type::Opaque("keyof".into()));
        }
        if self.is_word("unique") && self.peek_is_word(1, "symbol") {
            self.advance();
            self.advance();
            return Ok(Type::Symbol);
        }
        if self.is_word("readonly")
            && matches!(self.peek(1), Tok::Ident(_) | Tok::Punct("(" | "[" | "{"))
        {
            self.advance();
            return Ok(match self.operator_ty()? {
                Type::Array(t) => Type::ReadonlyArray(t),
                Type::Tuple(ts) => Type::ReadonlyArray(Box::new(Type::union(ts))),
                t => t,
            });
        }
        if self.is_word("infer") && self.is_ident_tok(1) {
            self.advance();
            self.advance();
            if self.is_word("extends") {
                // `infer U extends C`: in a conditional's check position a following `?`
                // means the `extends` starts the conditional instead (tsc's
                // `tryParseConstraintOfInferType`).
                let outer_disallow = self.disallow_cond;
                self.try_parse(|p| {
                    p.advance();
                    let t = p.ty_cond(false)?;
                    if !outer_disallow && p.is_punct("?") {
                        return Err(p.err("conditional"));
                    }
                    Ok(t)
                });
            }
            return Ok(Type::Opaque("infer".into()));
        }
        if self.jsdoc {
            if self.is_punct("?") {
                // `?` alone is any; `?T` is `T | null`.
                if matches!(
                    self.peek(1),
                    Tok::Punct(")" | "," | ">" | "]" | "}" | "=" | "|") | Tok::Eof
                ) {
                    self.advance();
                    return Ok(Type::Any);
                }
                self.advance();
                let t = self.operator_ty()?;
                return Ok(Type::union(vec![t, Type::Null]));
            }
            if self.eat_punct("!") {
                return self.operator_ty();
            }
        }
        self.postfix_ty()
    }

    fn postfix_ty(&mut self) -> PResult<Type> {
        let mut t = self.primary_ty()?;
        loop {
            if self.is_punct("[") && !self.nl_before_cur() {
                self.advance();
                if self.eat_punct("]") {
                    t = Type::Array(Box::new(t));
                } else {
                    self.ty()?;
                    self.expect_punct("]")?;
                    t = Type::Opaque("indexed access type".into());
                }
            } else if self.jsdoc && self.is_punct("?") && !matches!(self.peek(1), Tok::Ident(_)) {
                self.advance();
                t = Type::union(vec![t, Type::Null]);
            } else if self.jsdoc && self.is_punct("!") {
                self.advance();
            } else {
                return Ok(t);
            }
        }
    }

    fn primary_ty(&mut self) -> PResult<Type> {
        match self.tok().clone() {
            Tok::Str(s) => {
                self.advance();
                Ok(Type::StringLiteral(s))
            }
            Tok::Num(n) => {
                self.advance();
                Ok(Type::NumberLiteral(n))
            }
            Tok::BigInt(b) => {
                self.advance();
                Ok(Type::BigIntLiteral(b))
            }
            Tok::Punct("-") => {
                self.advance();
                match self.advance().tok {
                    Tok::Num(n) => Ok(Type::NumberLiteral(-n)),
                    Tok::BigInt(b) => Ok(Type::BigIntLiteral(format!("-{b}"))),
                    _ => Err(self.err("Number expected")),
                }
            }
            Tok::Template { head, tail, cooked } => {
                if head && tail {
                    self.advance();
                    return Ok(Type::StringLiteral(cooked));
                }
                self.advance();
                loop {
                    self.ty()?;
                    match self.advance().tok {
                        Tok::Template { tail: true, .. } => break,
                        Tok::Template { .. } => {}
                        _ => return Err(self.err("'}' expected")),
                    }
                }
                Ok(Type::Opaque("template literal type".into()))
            }
            Tok::Punct("{") => self.object_type(),
            Tok::Punct("[") => self.tuple_type(),
            Tok::Punct("(") => {
                self.advance();
                let t = self.ty()?;
                self.expect_punct(")")?;
                Ok(t)
            }
            Tok::Punct("*") if self.jsdoc => {
                self.advance();
                Ok(Type::Any)
            }
            Tok::Ident(w) => {
                let keyword = match w.as_str() {
                    "any" => Some(Type::Any),
                    "unknown" => Some(Type::Unknown),
                    "never" => Some(Type::Never),
                    "void" => Some(Type::Void),
                    "undefined" => Some(Type::Undefined),
                    "null" => Some(Type::Null),
                    "boolean" => Some(Type::Boolean),
                    "number" => Some(Type::Number),
                    "bigint" => Some(Type::BigInt),
                    "string" => Some(Type::String),
                    "symbol" => Some(Type::Symbol),
                    "object" => Some(Type::NonPrimitive),
                    "true" => Some(Type::BooleanLiteral(true)),
                    "false" => Some(Type::BooleanLiteral(false)),
                    "this" => Some(Type::This),
                    _ => None,
                };
                if let Some(t) = keyword {
                    self.advance();
                    return Ok(t);
                }
                if w == "typeof" {
                    self.advance();
                    if self.is_word("import") {
                        self.import_type()?;
                    } else {
                        self.ident()?;
                        while self.eat_punct(".") {
                            let t = self.advance();
                            if !matches!(t.tok, Tok::Ident(_) | Tok::Private(_)) {
                                return Err(self.err("Identifier expected"));
                            }
                        }
                        if self.is_punct("<") && !self.nl_before_cur() {
                            self.type_args()?;
                        }
                    }
                    return Ok(Type::Opaque("typeof type".into()));
                }
                if w == "import" {
                    self.import_type()?;
                    return Ok(Type::Opaque("import type".into()));
                }
                self.advance();
                let mut name = w;
                while self.is_punct(".") && !(self.jsdoc && self.peek_is_punct(1, "<")) {
                    self.advance();
                    name.push('.');
                    name.push_str(&self.ident()?.0);
                }
                if self.jsdoc && self.is_punct(".") && self.peek_is_punct(1, "<") {
                    self.advance();
                }
                let arguments = if self.is_punct("<") && !self.nl_before_cur() {
                    self.type_args()?
                } else {
                    Vec::new()
                };
                if self.jsdoc {
                    return Ok(jsdoc_named(name, arguments));
                }
                Ok(Type::Reference { name, arguments })
            }
            _ => Err(self.err("Type expected")),
        }
    }

    fn import_type(&mut self) -> PResult<()> {
        self.expect_word("import")?;
        self.expect_punct("(")?;
        self.module_specifier()?;
        self.expect_punct(")")?;
        while self.eat_punct(".") {
            self.ident()?;
        }
        if self.is_punct("<") {
            self.type_args()?;
        }
        Ok(())
    }

    fn tuple_type(&mut self) -> PResult<Type> {
        self.expect_punct("[")?;
        let mut elems = Vec::new();
        let mut variadic = false;
        while !self.eat_punct("]") {
            let rest = self.eat_punct("...");
            // Named member `name: T` / `name?: T`.
            if matches!(self.tok(), Tok::Ident(_))
                && (self.peek_is_punct(1, ":")
                    || (self.peek_is_punct(1, "?") && self.peek_is_punct(2, ":")))
            {
                self.advance();
                let optional = self.eat_punct("?");
                self.advance();
                let t = self.ty()?;
                elems.push(if optional {
                    Type::union(vec![t, Type::Undefined])
                } else {
                    t
                });
            } else {
                let t = self.ty()?;
                let optional = self.eat_punct("?");
                elems.push(if optional {
                    Type::union(vec![t, Type::Undefined])
                } else {
                    t
                });
            }
            if rest {
                variadic = true;
            }
            if !self.eat_punct(",") {
                self.expect_punct("]")?;
                break;
            }
        }
        if variadic {
            // `[A, ...B[]]`: only the element union is kept.
            let members = elems
                .into_iter()
                .map(|t| match t {
                    Type::Array(e) | Type::ReadonlyArray(e) => *e,
                    t => t,
                })
                .collect();
            return Ok(Type::Array(Box::new(Type::union(members))));
        }
        Ok(Type::Tuple(elems))
    }

    fn type_member_sep(&mut self) -> PResult<()> {
        if self.eat_punct(";") || self.eat_punct(",") || self.is_punct("}") || self.nl_before_cur()
        {
            Ok(())
        } else {
            Err(self.err("';' expected"))
        }
    }

    fn object_type(&mut self) -> PResult<Type> {
        let open = self.pos;
        self.expect_punct("{")?;
        // Mapped type: `{ [K in T]: U }` (with optional `readonly`/`+`/`-` modifiers).
        let mut k = 0;
        while matches!(self.peek(k), Tok::Punct("+" | "-")) || self.peek_is_word(k, "readonly") {
            k += 1;
        }
        if self.peek_is_punct(k, "[") && self.is_ident_tok(k + 1) && self.peek_is_word(k + 2, "in")
        {
            let close = self
                .matching(open)
                .ok_or_else(|| self.err("'}' expected"))?;
            self.pos = close + 1;
            return Ok(Type::Opaque("mapped type".into()));
        }
        let mut obj = ObjectType::default();
        while !self.eat_punct("}") {
            if self.at_eof() {
                return Err(self.err("'}' expected"));
            }
            let readonly = self.is_word("readonly") && !self.member_name_ends(1);
            if readonly {
                self.advance();
            }
            // Index signature.
            if self.is_punct("[") && self.is_ident_tok(1) && self.peek_is_punct(2, ":") {
                self.advance();
                self.advance();
                self.advance();
                let key = self.ty()?;
                self.expect_punct("]")?;
                let value = if self.eat_punct(":") {
                    self.ty()?
                } else {
                    Type::Any
                };
                obj.index.push(IndexSignature {
                    key,
                    value,
                    readonly,
                });
                self.type_member_sep()?;
                continue;
            }
            // Call / construct signatures.
            if self.is_punct("(") || self.is_punct("<") {
                let type_params = self.type_params_opt()?;
                let (this, params) = self.params()?;
                let (ret, predicate) = if self.eat_punct(":") {
                    self.return_type()?
                } else {
                    (Some(Type::Any), None)
                };
                obj.calls.push(FnType {
                    type_params,
                    this,
                    params: params_to_types(&params),
                    ret: ret.unwrap_or(Type::Any),
                    predicate,
                    construct: false,
                });
                self.type_member_sep()?;
                continue;
            }
            if self.is_word("new") && (self.peek_is_punct(1, "(") || self.peek_is_punct(1, "<")) {
                self.advance();
                let type_params = self.type_params_opt()?;
                let (_, params) = self.params()?;
                let ret = if self.eat_punct(":") {
                    self.ty()?
                } else {
                    Type::Any
                };
                obj.constructs.push(FnType {
                    type_params,
                    this: None,
                    params: params_to_types(&params),
                    ret,
                    predicate: None,
                    construct: true,
                });
                self.type_member_sep()?;
                continue;
            }
            let mut accessor = None;
            if (self.is_word("get") || self.is_word("set")) && !self.member_name_ends(1) {
                accessor = Some(self.is_word("get"));
                self.advance();
            }
            let (key, _) = self.prop_key()?;
            let name = key.static_name().unwrap_or_else(|| "[computed]".into());
            let optional = self.eat_punct("?");
            if self.is_punct("(") || self.is_punct("<") {
                let type_params = self.type_params_opt()?;
                let (this, params) = self.params()?;
                let (ret, predicate) = if self.eat_punct(":") {
                    self.return_type()?
                } else {
                    (Some(Type::Any), None)
                };
                let f = FnType {
                    type_params,
                    this,
                    params: params_to_types(&params),
                    ret: ret.unwrap_or(Type::Any),
                    predicate,
                    construct: false,
                };
                match accessor {
                    Some(true) => obj.props.push(Property {
                        name,
                        optional,
                        readonly: true,
                        method: false,
                        ty: f.ret,
                    }),
                    Some(false) => {}
                    None => obj.props.push(Property {
                        name,
                        optional,
                        readonly,
                        method: true,
                        ty: Type::Function(Box::new(f)),
                    }),
                }
            } else {
                let ty = if self.eat_punct(":") {
                    self.ty()?
                } else {
                    Type::Any
                };
                obj.props.push(Property {
                    name,
                    optional,
                    readonly,
                    method: false,
                    ty,
                });
            }
            self.type_member_sep()?;
        }
        Ok(Type::Object(obj))
    }
}

fn params_to_types(params: &[ParamNode]) -> Vec<Param> {
    params
        .iter()
        .map(|p| Param {
            name: match &p.pat {
                Pattern::Ident { name, .. } => name.clone(),
                Pattern::Destructure { .. } => "_".into(),
            },
            ty: p.ty.clone().unwrap_or(Type::Any),
            optional: p.optional || p.default.is_some(),
            rest: p.rest,
        })
        .collect()
}

/// JSDoc's special names (§4.5): bare `Object`/`Function`/`Array` are `any`; boxed primitive
/// names mean the primitive; `Object.<K, V>` is an index signature.
fn jsdoc_named(name: String, arguments: Vec<Type>) -> Type {
    match (name.as_str(), arguments.len()) {
        ("Object" | "object" | "Function" | "function" | "Array" | "array", 0) => Type::Any,
        ("String", 0) => Type::String,
        ("Number", 0) => Type::Number,
        ("Boolean", 0) => Type::Boolean,
        ("Symbol", 0) => Type::Symbol,
        ("BigInt", 0) => Type::BigInt,
        ("Array" | "array", 1) => Type::Array(Box::new(arguments.into_iter().next().unwrap())),
        ("Object" | "object", 2) => {
            let mut it = arguments.into_iter();
            let key = it.next().unwrap();
            let value = it.next().unwrap();
            Type::Object(ObjectType {
                index: vec![IndexSignature {
                    key,
                    value,
                    readonly: false,
                }],
                ..ObjectType::default()
            })
        }
        _ => Type::Reference { name, arguments },
    }
}
