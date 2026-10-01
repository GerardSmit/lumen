//! Python tokenizer.
//!
//! Produces the token stream CPython's tokenizer would, minus comments and blank-line `NL`
//! tokens: names, keywords, operators, numbers, strings, and `INDENT`/`DEDENT`/`NEWLINE`
//! bookkeeping. Integer literals are converted to decimal digit strings, string literals are
//! decoded; f-strings are delimited here (PEP 701) and kept raw for the parser to split.

pub(crate) mod string;

use std::rc::Rc;

use crate::ast::Ident;
use crate::parser::SyntaxError;
use crate::unicode::{is_xid_continue, is_xid_start, nfkc};
use string::{decode_bytes, decode_str, find_end, Quote, ScanErr};

const MAX_BRACKET_DEPTH: usize = 200;
const MAX_INDENT_DEPTH: usize = 100;
const TAB_SIZE: u32 = 8;

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Name(Ident),
    /// A reserved word.
    Kw(&'static str),
    /// An operator or delimiter.
    Op(&'static str),
    /// Decimal digits of an integer literal.
    Int(Rc<str>),
    Float(f64),
    Imag(f64),
    Str(Rc<str>),
    Bytes(Rc<[u8]>),
    /// An f-string; `body` is the raw text between the quotes.
    FStr {
        body: Rc<[char]>,
        raw: bool,
        line: u32,
        col: u32,
    },
    Newline,
    Indent,
    Dedent,
    EndMarker,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub line: u32,
    pub col: u32,
}

const OPS: &[&str] = &[
    "**=", "//=", ">>=", "<<=", "...", "**", "//", ">>", "<<", "<=", ">=", "==", "!=", "->", "+=",
    "-=", "*=", "/=", "%=", "&=", "|=", "^=", "@=", ":=", "+", "-", "*", "/", "%", "@", "&", "|",
    "^", "~", "<", ">", "(", ")", "[", "]", "{", "}", ",", ":", ".", ";", "=",
];

pub fn keyword(s: &str) -> Option<&'static str> {
    Some(match s {
        "False" => "False",
        "None" => "None",
        "True" => "True",
        "and" => "and",
        "as" => "as",
        "assert" => "assert",
        "async" => "async",
        "await" => "await",
        "break" => "break",
        "class" => "class",
        "continue" => "continue",
        "def" => "def",
        "del" => "del",
        "elif" => "elif",
        "else" => "else",
        "except" => "except",
        "finally" => "finally",
        "for" => "for",
        "from" => "from",
        "global" => "global",
        "if" => "if",
        "import" => "import",
        "in" => "in",
        "is" => "is",
        "lambda" => "lambda",
        "nonlocal" => "nonlocal",
        "not" => "not",
        "or" => "or",
        "pass" => "pass",
        "raise" => "raise",
        "return" => "return",
        "try" => "try",
        "while" => "while",
        "with" => "with",
        "yield" => "yield",
        _ => return None,
    })
}

/// Converts digits in `radix` (2, 8 or 16; no separators) to a decimal digit string.
fn radix_to_decimal(digits: &[char], radix: u64) -> String {
    if digits.len() > 64 {
        let text: String = digits.iter().collect();
        if let Some(n) = lumen_common::bigint::BigInt::parse_radix(&text, radix as u32) {
            return n.to_string_radix(10);
        }
    }
    const BASE: u64 = 1_000_000_000;
    let mut limbs: Vec<u64> = vec![0];
    for d in digits {
        let mut carry = d.to_digit(16).unwrap_or(0) as u64;
        for limb in limbs.iter_mut() {
            let v = *limb * radix + carry;
            *limb = v % BASE;
            carry = v / BASE;
        }
        while carry > 0 {
            limbs.push(carry % BASE);
            carry /= BASE;
        }
    }
    let mut out = String::new();
    for (k, limb) in limbs.iter().rev().enumerate() {
        if k == 0 {
            out.push_str(&limb.to_string());
        } else {
            out.push_str(&format!("{limb:09}"));
        }
    }
    out
}

pub fn tokenize(src: &str) -> Result<Vec<Token>, SyntaxError> {
    let mut chars: Vec<char> = Vec::with_capacity(src.len());
    let mut it = src.chars().peekable();
    if it.peek() == Some(&'\u{feff}') {
        it.next();
    }
    while let Some(c) = it.next() {
        match c {
            '\r' => {
                if it.peek() == Some(&'\n') {
                    it.next();
                }
                chars.push('\n');
            }
            '\0' => {
                return Err(SyntaxError {
                    msg: "source code string cannot contain null bytes".into(),
                    line: 1,
                    col: 0,
                })
            }
            _ => chars.push(c),
        }
    }
    Lexer::new(&chars, 1, 0, false).run()
}

/// Tokenizes an expression embedded in an f-string: newlines are insignificant and there is
/// no indentation.
pub(crate) fn tokenize_fragment(
    chars: &[char],
    line: u32,
    col: u32,
) -> Result<Vec<Token>, SyntaxError> {
    Lexer::new(chars, line, col, true).run()
}

struct Lexer<'a> {
    src: &'a [char],
    i: usize,
    line: u32,
    col: u32,
    toks: Vec<Token>,
    indents: Vec<(u32, u32)>,
    brackets: Vec<(char, u32, u32)>,
    fragment: bool,
}

impl<'a> Lexer<'a> {
    fn new(src: &'a [char], line: u32, col: u32, fragment: bool) -> Self {
        Lexer {
            src,
            i: 0,
            line,
            col,
            toks: Vec::new(),
            indents: vec![(0, 0)],
            brackets: Vec::new(),
            fragment,
        }
    }

    fn err<T>(&self, msg: impl Into<String>, line: u32, col: u32) -> Result<T, SyntaxError> {
        Err(SyntaxError {
            msg: msg.into(),
            line,
            col,
        })
    }

    fn peek(&self, k: usize) -> Option<char> {
        self.src.get(self.i + k).copied()
    }

    fn bump(&mut self) {
        if self.src[self.i] == '\n' {
            self.line += 1;
            self.col = 0;
        } else {
            self.col += 1;
        }
        self.i += 1;
    }

    fn advance_to(&mut self, idx: usize) {
        while self.i < idx {
            self.bump();
        }
    }

    fn push(&mut self, tok: Tok, line: u32, col: u32) {
        self.toks.push(Token { tok, line, col });
    }

    fn run(mut self) -> Result<Vec<Token>, SyntaxError> {
        let mut at_line_start = !self.fragment;
        while self.i < self.src.len() {
            if at_line_start && self.brackets.is_empty() {
                if self.line_start()? {
                    continue;
                }
                at_line_start = false;
            }
            let c = self.src[self.i];
            match c {
                ' ' | '\t' | '\x0c' => self.bump(),
                '#' => {
                    while self.i < self.src.len() && self.src[self.i] != '\n' {
                        self.bump();
                    }
                }
                '\n' => {
                    let (line, col) = (self.line, self.col);
                    self.bump();
                    if self.brackets.is_empty() && !self.fragment {
                        self.push(Tok::Newline, line, col);
                        at_line_start = true;
                    }
                }
                '\\' => self.continuation()?,
                '0'..='9' => self.number()?,
                '.' if self.peek(1).is_some_and(|d| d.is_ascii_digit()) => self.number()?,
                '"' | '\'' => self.string(0, false, false, false)?,
                _ if is_xid_start(c) => self.name()?,
                _ => self.operator()?,
            }
        }
        self.finish()
    }

    fn finish(mut self) -> Result<Vec<Token>, SyntaxError> {
        if let Some(&(c, line, col)) = self.brackets.last() {
            return self.err(format!("'{c}' was never closed"), line, col);
        }
        let (line, col) = (self.line, self.col);
        if !self.fragment {
            if !matches!(
                self.toks.last(),
                None | Some(Token {
                    tok: Tok::Newline,
                    ..
                })
            ) {
                self.push(Tok::Newline, line, col);
            }
            while self.indents.len() > 1 {
                self.indents.pop();
                self.push(Tok::Dedent, line, col);
            }
        }
        self.push(Tok::EndMarker, line, col);
        Ok(self.toks)
    }

    fn continuation(&mut self) -> Result<(), SyntaxError> {
        let (line, col) = (self.line, self.col);
        match self.peek(1) {
            Some('\n') => {
                self.bump();
                self.bump();
                if self.i >= self.src.len() {
                    return self.err("unexpected EOF while parsing", line, col);
                }
                Ok(())
            }
            None => self.err("unexpected EOF while parsing", line, col),
            Some(_) => self.err(
                "unexpected character after line continuation character",
                line,
                col,
            ),
        }
    }

    /// Handles indentation at the start of a logical line. Returns true if the line was blank
    /// (and consumed), false if tokens follow.
    fn line_start(&mut self) -> Result<bool, SyntaxError> {
        let (mut col, mut alt) = (0u32, 0u32);
        let mut j = self.i;
        loop {
            match self.src.get(j) {
                Some(' ') => {
                    col += 1;
                    alt += 1;
                }
                Some('\t') => {
                    col = (col / TAB_SIZE + 1) * TAB_SIZE;
                    alt += 1;
                }
                Some('\x0c') => {
                    col = 0;
                    alt = 0;
                }
                _ => break,
            }
            j += 1;
        }
        match self.src.get(j) {
            None | Some('\n') | Some('#') => {
                self.advance_to(j);
                while self.i < self.src.len() && self.src[self.i] != '\n' {
                    self.bump();
                }
                if self.i < self.src.len() {
                    self.bump();
                }
                return Ok(true);
            }
            Some('\\') if self.src.get(j + 1) == Some(&'\n') => {
                self.advance_to(j);
                return Ok(false);
            }
            _ => {}
        }
        self.advance_to(j);
        let (line, pos) = (self.line, self.col);
        let &(top, alt_top) = self.indents.last().unwrap_or(&(0, 0));
        let tab_err = |s: &Self| {
            s.err(
                "inconsistent use of tabs and spaces in indentation",
                line,
                pos,
            )
        };
        if col == top {
            if alt != alt_top {
                return tab_err(self);
            }
        } else if col > top {
            if alt <= alt_top {
                return tab_err(self);
            }
            if self.indents.len() >= MAX_INDENT_DEPTH {
                return self.err("too many levels of indentation", line, pos);
            }
            self.indents.push((col, alt));
            self.push(Tok::Indent, line, pos);
        } else {
            while self.indents.last().is_some_and(|&(c, _)| c > col) {
                self.indents.pop();
                self.push(Tok::Dedent, line, pos);
            }
            let &(top, alt_top) = self.indents.last().unwrap_or(&(0, 0));
            if top != col {
                return self.err(
                    "unindent does not match any outer indentation level",
                    line,
                    pos,
                );
            }
            if alt_top != alt {
                return tab_err(self);
            }
        }
        Ok(false)
    }

    fn name(&mut self) -> Result<(), SyntaxError> {
        let (line, col) = (self.line, self.col);
        let mut j = self.i;
        while self.src.get(j).is_some_and(|&c| is_xid_continue(c)) {
            j += 1;
        }
        if j - self.i <= 2 && matches!(self.src.get(j), Some('"') | Some('\'')) {
            let prefix: String = self.src[self.i..j]
                .iter()
                .map(|c| c.to_ascii_lowercase())
                .collect();
            let kind = match prefix.as_str() {
                "r" => Some((false, true, false)),
                "u" => Some((false, false, false)),
                "b" => Some((true, false, false)),
                "br" | "rb" => Some((true, true, false)),
                "f" => Some((false, false, true)),
                "fr" | "rf" => Some((false, true, true)),
                "t" | "tr" | "rt" => {
                    return self.err("t-strings are not supported", line, col);
                }
                _ => None,
            };
            if let Some((bytes, raw, fmt)) = kind {
                return self.string(j - self.i, bytes, raw, fmt);
            }
        }
        let text = nfkc(&self.src[self.i..j].iter().collect::<String>());
        self.advance_to(j);
        let tok = match keyword(&text) {
            Some(k) => Tok::Kw(k),
            None => Tok::Name(Rc::from(text.as_str())),
        };
        self.push(tok, line, col);
        Ok(())
    }

    fn operator(&mut self) -> Result<(), SyntaxError> {
        let (line, col) = (self.line, self.col);
        let c = self.src[self.i];
        let op = OPS.iter().find(|op| {
            op.chars()
                .enumerate()
                .all(|(k, ch)| self.peek(k) == Some(ch))
        });
        let Some(&op) = op else {
            let msg = if c.is_control() || !c.is_ascii() {
                format!("invalid character '{c}' (U+{:04X})", c as u32)
            } else {
                format!("invalid syntax: unexpected character '{c}'")
            };
            return self.err(msg, line, col);
        };
        for _ in 0..op.len() {
            self.bump();
        }
        match op {
            "(" | "[" | "{" => {
                if self.brackets.len() >= MAX_BRACKET_DEPTH {
                    return self.err("too many nested parentheses", line, col);
                }
                self.brackets.push((c, line, col));
            }
            ")" | "]" | "}" => {
                let Some((open, oline, _)) = self.brackets.pop() else {
                    return self.err(format!("unmatched '{c}'"), line, col);
                };
                let want = match open {
                    '(' => ')',
                    '[' => ']',
                    _ => '}',
                };
                if want != c {
                    let mut msg = format!(
                        "closing parenthesis '{c}' does not match opening parenthesis '{open}'"
                    );
                    if oline != line {
                        msg.push_str(&format!(" on line {oline}"));
                    }
                    return self.err(msg, line, col);
                }
            }
            _ => {}
        }
        self.push(Tok::Op(op), line, col);
        Ok(())
    }

    fn string(
        &mut self,
        prefix: usize,
        bytes: bool,
        raw: bool,
        fmt: bool,
    ) -> Result<(), SyntaxError> {
        let (line, col) = (self.line, self.col);
        self.advance_to(self.i + prefix);
        let q = self.src[self.i];
        let triple = self.peek(1) == Some(q) && self.peek(2) == Some(q);
        let qlen = if triple { 3 } else { 1 };
        self.advance_to(self.i + qlen);
        let (body_line, body_col) = (self.line, self.col);
        let start = self.i;
        let end = match find_end(self.src, start, Quote { ch: q, triple }, fmt, raw, 0) {
            Ok(e) => e,
            Err(ScanErr::Msg(m)) => return self.err(m, line, col),
            Err(ScanErr::Unterminated) => {
                let newlines = self.src[start..].iter().filter(|&&c| c == '\n').count() as u32;
                let msg = if triple {
                    format!(
                        "unterminated triple-quoted string literal (detected at line {})",
                        self.line + newlines
                    )
                } else {
                    format!(
                        "unterminated string literal (detected at line {})",
                        self.line
                    )
                };
                return self.err(msg, line, col);
            }
        };
        let body = &self.src[start..end];
        let tok = if fmt {
            Tok::FStr {
                body: Rc::from(body),
                raw,
                line: body_line,
                col: body_col,
            }
        } else if bytes {
            match decode_bytes(body, raw) {
                Ok(b) => Tok::Bytes(Rc::from(b)),
                Err(m) => return self.err(m, line, col),
            }
        } else {
            match decode_str(body, raw) {
                Ok(s) => Tok::Str(Rc::from(s.as_str())),
                Err(m) => return self.err(m, line, col),
            }
        };
        self.advance_to(end + qlen);
        self.push(tok, line, col);
        Ok(())
    }

    /// Reads digits (with single `_` separators) valid in `radix` starting at `j`.
    fn digits(&self, mut j: usize, radix: u32, out: &mut Vec<char>) -> Result<usize, SyntaxError> {
        let is_digit = |c: Option<&char>| c.is_some_and(|c| c.is_digit(radix));
        while is_digit(self.src.get(j)) {
            out.push(self.src[j]);
            j += 1;
            if self.src.get(j) == Some(&'_') {
                if !is_digit(self.src.get(j + 1)) {
                    return self.err("invalid decimal literal", self.line, self.col);
                }
                j += 1;
            }
        }
        Ok(j)
    }

    fn number(&mut self) -> Result<(), SyntaxError> {
        let (line, col) = (self.line, self.col);
        let start = self.i;
        let c0 = self.src[start];
        let radix_kind = match (c0, self.peek(1)) {
            ('0', Some('x' | 'X')) => Some((16, "hexadecimal")),
            ('0', Some('o' | 'O')) => Some((8, "octal")),
            ('0', Some('b' | 'B')) => Some((2, "binary")),
            _ => None,
        };
        let mut digits: Vec<char> = Vec::new();
        let tok;
        let mut j;
        if let Some((radix, name)) = radix_kind {
            j = start + 2;
            if self.src.get(j) == Some(&'_') {
                j += 1;
            }
            j = self.digits(j, radix, &mut digits)?;
            if digits.is_empty() {
                return self.err(format!("invalid {name} literal"), line, col);
            }
            if let Some(&d) = self.src.get(j).filter(|c| c.is_ascii_digit()) {
                return self.err(format!("invalid digit '{d}' in {name} literal"), line, col);
            }
            tok = Tok::Int(Rc::from(radix_to_decimal(&digits, radix as u64).as_str()));
        } else {
            j = self.digits(start, 10, &mut digits)?;
            let int_end = digits.len();
            let mut is_float = false;
            let mut text: String = digits.iter().collect();
            if self.src.get(j) == Some(&'.') {
                is_float = true;
                text.push('.');
                j += 1;
                let mut frac = Vec::new();
                j = self.digits(j, 10, &mut frac)?;
                text.extend(frac);
            }
            if matches!(self.src.get(j), Some('e' | 'E')) {
                let sign = matches!(self.src.get(j + 1), Some('+' | '-'));
                let d = self.src.get(j + 1 + sign as usize);
                if d.is_some_and(|c| c.is_ascii_digit()) {
                    is_float = true;
                    text.push('e');
                    if sign {
                        text.push(self.src[j + 1]);
                    }
                    let mut exp = Vec::new();
                    j = self.digits(j + 1 + sign as usize, 10, &mut exp)?;
                    text.extend(exp);
                }
            }
            if matches!(self.src.get(j), Some('j' | 'J')) {
                j += 1;
                let v: f64 = text.parse().unwrap_or(0.0);
                tok = Tok::Imag(v);
            } else if is_float {
                tok = Tok::Float(text.parse().unwrap_or(0.0));
            } else {
                let trimmed = digits[..int_end].iter().collect::<String>();
                let trimmed = trimmed.trim_start_matches('0');
                if trimmed.is_empty() {
                    tok = Tok::Int(Rc::from("0"));
                } else if digits[0] == '0' {
                    return self.err(
                        "leading zeros in decimal integer literals are not permitted; use an 0o prefix for octal integers",
                        line,
                        col,
                    );
                } else {
                    let limit = crate::limits::literal_digit_limit();
                    if limit != 0 && int_end > limit {
                        let msg = crate::limits::digit_limit_message(limit, Some(int_end));
                        return self.err(
                            format!("{msg} - Consider hexadecimal for huge integer literals to avoid decimal conversion limits."),
                            line,
                            col,
                        );
                    }
                    tok = Tok::Int(Rc::from(trimmed));
                }
            }
        }
        if self.src.get(j).is_some_and(|&c| is_xid_start(c)) {
            let mut k = j;
            while self.src.get(k).is_some_and(|&c| is_xid_continue(c)) {
                k += 1;
            }
            let word: String = self.src[j..k].iter().collect();
            if keyword(&word).is_none() {
                return self.err("invalid decimal literal", line, col);
            }
        }
        self.advance_to(j);
        self.push(tok, line, col);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        tokenize(src).unwrap().into_iter().map(|t| t.tok).collect()
    }

    fn op(s: &'static str) -> Tok {
        Tok::Op(s)
    }

    fn name(s: &str) -> Tok {
        Tok::Name(Rc::from(s))
    }

    fn int(s: &str) -> Tok {
        Tok::Int(Rc::from(s))
    }

    fn err(src: &str) -> String {
        tokenize(src).unwrap_err().msg
    }

    #[test]
    fn simple_line() {
        assert_eq!(
            toks("x = 1\n"),
            vec![name("x"), op("="), int("1"), Tok::Newline, Tok::EndMarker]
        );
    }

    #[test]
    fn missing_trailing_newline() {
        assert_eq!(toks("x"), vec![name("x"), Tok::Newline, Tok::EndMarker]);
    }

    #[test]
    fn indent_dedent() {
        let t = toks("if x:\n    a\n    if y:\n        b\nc\n");
        let indents = t.iter().filter(|t| **t == Tok::Indent).count();
        let dedents = t.iter().filter(|t| **t == Tok::Dedent).count();
        assert_eq!((indents, dedents), (2, 2));
    }

    #[test]
    fn dedent_at_eof() {
        let t = toks("if x:\n    a");
        assert_eq!(
            &t[t.len() - 3..],
            &[Tok::Newline, Tok::Dedent, Tok::EndMarker]
        );
    }

    #[test]
    fn blank_and_comment_lines_ignored() {
        let t = toks("a\n\n   # hi\n\t\nb\n");
        assert_eq!(
            t,
            vec![
                name("a"),
                Tok::Newline,
                name("b"),
                Tok::Newline,
                Tok::EndMarker
            ]
        );
    }

    #[test]
    fn inconsistent_dedent() {
        assert_eq!(
            err("if x:\n        a\n    b\n"),
            "unindent does not match any outer indentation level"
        );
    }

    #[test]
    fn tabs_and_spaces() {
        assert!(tokenize("if x:\n\ta\n        b\n").is_err());
        assert!(tokenize("if x:\n\ta\n\tb\n").is_ok());
    }

    #[test]
    fn implicit_joining() {
        assert_eq!(
            toks("f(a,\n  b)\n"),
            vec![
                name("f"),
                op("("),
                name("a"),
                op(","),
                name("b"),
                op(")"),
                Tok::Newline,
                Tok::EndMarker
            ]
        );
    }

    #[test]
    fn backslash_continuation() {
        assert_eq!(
            toks("a = 1 + \\\n    2\n"),
            vec![
                name("a"),
                op("="),
                int("1"),
                op("+"),
                int("2"),
                Tok::Newline,
                Tok::EndMarker
            ]
        );
    }

    #[test]
    fn crlf() {
        assert_eq!(toks("a\r\nb\r\n"), toks("a\nb\n"));
    }

    #[test]
    fn numbers() {
        assert_eq!(toks("0xff")[0], int("255"));
        assert_eq!(toks("0o17")[0], int("15"));
        assert_eq!(toks("0b1_01")[0], int("5"));
        assert_eq!(toks("1_000")[0], int("1000"));
        assert_eq!(toks("000")[0], int("0"));
        assert_eq!(toks("1.")[0], Tok::Float(1.0));
        assert_eq!(toks(".5")[0], Tok::Float(0.5));
        assert_eq!(toks("1e10")[0], Tok::Float(1e10));
        assert_eq!(toks("1_000.0")[0], Tok::Float(1000.0));
        assert_eq!(toks("3j")[0], Tok::Imag(3.0));
        assert_eq!(toks("1.5e3J")[0], Tok::Imag(1500.0));
        assert_eq!(toks("1if x else 2")[0], int("1"));
    }

    #[test]
    fn big_ints() {
        assert_eq!(
            toks("0xffffffffffffffffffffffffffffffffff")[0],
            int("87112285931760246646623899502532662132735")
        );
        assert_eq!(
            toks("123456789012345678901234567890")[0],
            int("123456789012345678901234567890")
        );
    }

    #[test]
    fn bad_numbers() {
        assert!(err("01").contains("leading zeros"));
        assert!(err("1_").contains("invalid decimal"));
        assert!(err("0x").contains("hexadecimal"));
        assert!(err("0b2").contains("binary"));
        assert!(err("1abc").contains("invalid decimal literal"));
    }

    #[test]
    fn strings() {
        assert_eq!(toks("'a\\nb'")[0], Tok::Str(Rc::from("a\nb")));
        assert_eq!(toks("r'a\\nb'")[0], Tok::Str(Rc::from("a\\nb")));
        assert_eq!(toks("b'\\x41\\101'")[0], Tok::Bytes(Rc::from(&b"AA"[..])));
        assert_eq!(toks("'''a\nb'''")[0], Tok::Str(Rc::from("a\nb")));
        assert_eq!(
            toks("'\\u00e9\\N{BULLET}'")[0],
            Tok::Str(Rc::from("\u{e9}\u{2022}"))
        );
        assert_eq!(toks("'\\q'")[0], Tok::Str(Rc::from("\\q")));
        assert_eq!(toks("Rb'\\n'")[0], Tok::Bytes(Rc::from(&b"\\n"[..])));
        assert_eq!(toks("''")[0], Tok::Str(Rc::from("")));
    }

    #[test]
    fn string_errors() {
        assert!(err("'abc").contains("unterminated string literal"));
        assert!(err("'''abc").contains("unterminated triple-quoted"));
        assert!(err("b'\u{e9}'").contains("ASCII"));
        assert!(err("'\\x4'").contains("truncated"));
    }

    #[test]
    fn fstring_nested_quotes() {
        let t = toks("f\"{d[\"a\"]}\" + 1");
        assert!(matches!(t[0], Tok::FStr { .. }));
        assert_eq!(t[1], op("+"));
    }

    #[test]
    fn deep_nesting() {
        let src = "(".repeat(10_000);
        assert!(err(&src).contains("too many nested parentheses"));
    }

    #[test]
    fn bracket_mismatch() {
        assert!(err("(]").contains("does not match"));
        assert!(err(")").contains("unmatched"));
        assert!(err("(1").contains("was never closed"));
    }

    #[test]
    fn operators() {
        assert_eq!(toks("a **= b // c")[1], op("**="));
        assert_eq!(toks("a := b")[1], op(":="));
        assert_eq!(toks("...")[0], op("..."));
        assert_eq!(toks("a->b")[1], op("->"));
    }

    #[test]
    fn unicode_identifiers() {
        assert_eq!(toks("caf\u{e9} = 1")[0], name("caf\u{e9}"));
    }
}
