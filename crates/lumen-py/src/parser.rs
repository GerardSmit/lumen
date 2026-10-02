//! Python parser: tokens from [`crate::lexer`] to [`crate::ast::Module`].
//!
//! A hand-written recursive-descent parser for the Python 3.12 grammar (plus PEP 758 bare
//! `except A, B:`). PEP 695 type parameters are parsed and discarded.

mod expr;
mod fstring;
mod pattern;
mod stmt;

use std::fmt;

use crate::ast::{self, Expr, Ident, Pos};
use crate::lexer::{self, Tok, Token};

#[derive(Clone, Debug, PartialEq)]
pub struct SyntaxError {
    pub msg: String,
    pub line: u32,
    pub col: u32,
}

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SyntaxError: {} (line {}, column {})",
            self.msg,
            self.line,
            self.col + 1
        )
    }
}

impl std::error::Error for SyntaxError {}

type PResult<T> = Result<T, SyntaxError>;

const MAX_DEPTH: u32 = 250;
/// Longest left-nested chain (`a + b + ...`, `a.b.c...`, `elif` ladders) the parser accepts, so
/// later passes over the tree stay within stack limits.
const MAX_CHAIN: u32 = 1000;

pub fn parse(src: &str, _filename: &str) -> Result<ast::Module, SyntaxError> {
    let toks = lexer::tokenize(src)?;
    Parser::new(toks, 0).module()
}

pub(crate) struct Parser {
    toks: Vec<Token>,
    p: usize,
    depth: u32,
}

impl Parser {
    pub(crate) fn new(toks: Vec<Token>, depth: u32) -> Self {
        Parser { toks, p: 0, depth }
    }

    fn tok(&self) -> &Tok {
        &self.toks[self.p].tok
    }

    fn peek(&self, n: usize) -> &Tok {
        &self.toks[(self.p + n).min(self.toks.len() - 1)].tok
    }

    fn pos(&self) -> Pos {
        let t = &self.toks[self.p];
        Pos {
            line: t.line,
            col: t.col,
        }
    }

    fn advance(&mut self) {
        if self.p + 1 < self.toks.len() {
            self.p += 1;
        }
    }

    fn at_op(&self, s: &str) -> bool {
        matches!(self.tok(), Tok::Op(o) if *o == s)
    }

    fn at_kw(&self, s: &str) -> bool {
        matches!(self.tok(), Tok::Kw(k) if *k == s)
    }

    fn at_name(&self, s: &str) -> bool {
        matches!(self.tok(), Tok::Name(n) if &**n == s)
    }

    fn eat_op(&mut self, s: &str) -> bool {
        let ok = self.at_op(s);
        if ok {
            self.advance();
        }
        ok
    }

    fn eat_kw(&mut self, s: &str) -> bool {
        let ok = self.at_kw(s);
        if ok {
            self.advance();
        }
        ok
    }

    fn expect_op(&mut self, s: &str) -> PResult<()> {
        if self.eat_op(s) {
            return Ok(());
        }
        if s == ":" {
            return self.error("expected ':'");
        }
        Err(self.unexpected())
    }

    fn expect_kw(&mut self, s: &str) -> PResult<()> {
        if self.eat_kw(s) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn error<T>(&self, msg: impl Into<String>) -> PResult<T> {
        let pos = self.pos();
        Err(SyntaxError {
            msg: msg.into(),
            line: pos.line,
            col: pos.col,
        })
    }

    fn error_at<T>(&self, pos: Pos, msg: impl Into<String>) -> PResult<T> {
        Err(SyntaxError {
            msg: msg.into(),
            line: pos.line,
            col: pos.col,
        })
    }

    fn unexpected(&self) -> SyntaxError {
        let msg = match self.tok() {
            Tok::Indent => "unexpected indent",
            Tok::EndMarker => "unexpected EOF while parsing",
            _ => "invalid syntax",
        };
        let pos = self.pos();
        SyntaxError {
            msg: msg.into(),
            line: pos.line,
            col: pos.col,
        }
    }

    fn ident(&mut self) -> PResult<Ident> {
        if let Tok::Name(n) = self.tok() {
            let n = n.clone();
            self.advance();
            Ok(n)
        } else {
            Err(self.unexpected())
        }
    }

    fn enter(&mut self) -> PResult<()> {
        if self.depth >= MAX_DEPTH || lumen_common::stack::exhausted() {
            return self.error("too many nested parentheses");
        }
        self.depth += 1;
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn module(&mut self) -> PResult<ast::Module> {
        let mut body = Vec::new();
        loop {
            while matches!(self.tok(), Tok::Newline) {
                self.advance();
            }
            if matches!(self.tok(), Tok::EndMarker) {
                break;
            }
            self.statement(&mut body)?;
        }
        Ok(ast::Module { body })
    }
}

fn mk(pos: Pos, kind: ast::ExprKind) -> Expr {
    Expr { pos, kind }
}
