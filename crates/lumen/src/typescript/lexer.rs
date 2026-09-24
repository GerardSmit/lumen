//! Byte-offset tokenizer for TypeScript and JavaScript source.
//!
//! Offsets are UTF-8 byte offsets into the source, which is what the engine's
//! `FnSource::Range` records. `>` is always its own token: the parser composes `>=`, `>>`,
//! `>>=`, `>>>` and `>>>=` from adjacent tokens, so closing a type-argument list never has to
//! split a token.

use super::{Diagnostic, LineIndex};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Identifiers and keywords alike (the parser decides by context).
    Ident(String),
    /// `#name`, stored with the leading `#`.
    Private(String),
    Str(String),
    Num(f64),
    BigInt(String),
    /// One chunk of a template literal. `head` chunks start with a backtick, `tail` chunks end
    /// with one (a template without substitutions is both).
    Template {
        cooked: String,
        head: bool,
        tail: bool,
    },
    Regex,
    Punct(&'static str),
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub start: u32,
    pub end: u32,
    /// A line terminator occurs between the previous token and this one.
    pub nl_before: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Comment {
    pub start: u32,
    pub end: u32,
    pub block: bool,
}

impl Comment {
    /// `/** ... */` (but not `/**/`).
    pub fn is_jsdoc(&self, src: &str) -> bool {
        self.block
            && src.as_bytes().get(self.start as usize + 2) == Some(&b'*')
            && self.end - self.start > 4
    }
}

pub struct Lexed {
    pub tokens: Vec<Token>,
    pub comments: Vec<Comment>,
}

const PUNCTS: &[&str] = &[
    "...", "===", "!==", "**=", "<<=", "&&=", "||=", "??=", "=>", "==", "!=", "<=", "+=", "-=",
    "*=", "/=", "%=", "&=", "|=", "^=", "++", "--", "<<", "&&", "||", "??", "?.", "**", "{", "}",
    "(", ")", "[", "]", ";", ",", "<", ">", "+", "-", "*", "/", "%", "&", "|", "^", "!", "~", "?",
    ":", "=", ".", "@",
];

/// Keywords after which a `/` starts a regular expression rather than a division.
const REGEX_AFTER_WORD: &[&str] = &[
    "return",
    "typeof",
    "instanceof",
    "in",
    "of",
    "new",
    "delete",
    "void",
    "throw",
    "case",
    "do",
    "else",
    "yield",
    "await",
    "extends",
];

#[derive(Clone, Copy, PartialEq)]
enum Brace {
    Block,
    Template,
}

pub fn lex(src: &str) -> Result<Lexed, Diagnostic> {
    Lexer {
        src,
        b: src.as_bytes(),
        i: 0,
        tokens: Vec::new(),
        comments: Vec::new(),
        braces: Vec::new(),
        nl: false,
        force: None,
    }
    .run()
}

/// Re-tokenizes `src` from the token at index `at` onward, deciding whether a `/` there starts
/// a regular expression (`regex`) the way the parser's grammar position says, rather than by
/// the lexer's previous-token heuristic. Tokens before `at` (and their comments) are kept.
pub fn relex(src: &str, lexed: &mut Lexed, at: usize, regex: bool) -> Result<(), Diagnostic> {
    let Some(tok) = lexed.tokens.get(at) else {
        return Ok(());
    };
    let (start, nl) = (tok.start as usize, tok.nl_before);
    let mut braces = Vec::new();
    for t in &lexed.tokens[..at] {
        match &t.tok {
            Tok::Punct("{") => braces.push(Brace::Block),
            Tok::Punct("}") => {
                braces.pop();
            }
            Tok::Template { head, tail, .. } => {
                if !*head {
                    braces.pop();
                }
                if !*tail {
                    braces.push(Brace::Template);
                }
            }
            _ => {}
        }
    }
    let mut tokens = std::mem::take(&mut lexed.tokens);
    tokens.truncate(at);
    let mut comments = std::mem::take(&mut lexed.comments);
    comments.retain(|c| (c.start as usize) < start);
    *lexed = Lexer {
        src,
        b: src.as_bytes(),
        i: start,
        tokens,
        comments,
        braces,
        nl,
        force: Some(regex),
    }
    .run()?;
    Ok(())
}

struct Lexer<'s> {
    src: &'s str,
    b: &'s [u8],
    i: usize,
    tokens: Vec<Token>,
    comments: Vec<Comment>,
    braces: Vec<Brace>,
    nl: bool,
    /// For the first token of a [`relex`]: whether a `/` starts a regular expression.
    force: Option<bool>,
}

pub fn is_id_start(c: char) -> bool {
    c == '_' || c == '$' || c.is_ascii_alphabetic() || (!c.is_ascii() && c.is_alphabetic())
}

pub fn is_id_part(c: char) -> bool {
    is_id_start(c)
        || c.is_ascii_digit()
        || c == '\u{200c}'
        || c == '\u{200d}'
        || (!c.is_ascii() && c.is_alphanumeric())
}

impl<'s> Lexer<'s> {
    fn err(&self, at: usize, msg: impl Into<String>) -> Diagnostic {
        LineIndex::new(self.src).diagnostic(1127, msg, at as u32, at as u32 + 1)
    }

    fn ch(&self) -> Option<char> {
        self.src[self.i..].chars().next()
    }

    fn peek_byte(&self, k: usize) -> u8 {
        self.b.get(self.i + k).copied().unwrap_or(0)
    }

    fn push(&mut self, tok: Tok, start: usize) {
        self.tokens.push(Token {
            tok,
            start: start as u32,
            end: self.i as u32,
            nl_before: self.nl,
        });
        self.nl = false;
    }

    fn regex_allowed(&mut self) -> bool {
        if let Some(f) = self.force.take() {
            return f;
        }
        match self.tokens.last().map(|t| &t.tok) {
            None => true,
            Some(Tok::Punct(p)) => !matches!(*p, ")" | "]" | "}" | "++" | "--"),
            Some(Tok::Ident(w)) => REGEX_AFTER_WORD.contains(&w.as_str()),
            Some(Tok::Template { tail, .. }) => !*tail,
            Some(_) => false,
        }
    }

    fn run(mut self) -> Result<Lexed, Diagnostic> {
        if self.i == 0 && self.src.starts_with("#!") {
            while self.i < self.b.len() && self.b[self.i] != b'\n' {
                self.i += 1;
            }
        }
        loop {
            self.skip_trivia()?;
            let Some(c) = self.ch() else { break };
            let start = self.i;
            if is_id_start(c) || c == '\\' {
                let name = self.ident()?;
                self.push(Tok::Ident(name), start);
            } else if c == '#' {
                self.i += 1;
                let name = self.ident()?;
                self.push(Tok::Private(format!("#{name}")), start);
            } else if c.is_ascii_digit() || (c == '.' && self.peek_byte(1).is_ascii_digit()) {
                let tok = self.number()?;
                self.push(tok, start);
            } else if c == '"' || c == '\'' {
                let s = self.string(c)?;
                self.push(Tok::Str(s), start);
            } else if c == '`' {
                self.i += 1;
                self.template(start, true)?;
            } else if c == '}' && self.braces.last() == Some(&Brace::Template) {
                self.braces.pop();
                self.i += 1;
                self.template(start, false)?;
            } else if c == '/' && self.regex_allowed() && self.regex() {
                // A `/` the heuristic took for a regular expression that does not lex as one
                // falls through to the division punctuator below; the parser relexes when its
                // grammar position disagrees with the guess either way.
                self.push(Tok::Regex, start);
            } else {
                self.i = start;
                let rest = &self.src[self.i..];
                let Some(p) = PUNCTS.iter().find(|p| rest.starts_with(**p)) else {
                    return Err(self.err(self.i, format!("Invalid character '{c}'")));
                };
                // `?.` followed by a digit is `?` then a number (`a?.5:b`).
                let p: &'static str = if *p == "?." && self.peek_byte(2).is_ascii_digit() {
                    "?"
                } else {
                    p
                };
                self.i += p.len();
                match p {
                    "{" => self.braces.push(Brace::Block),
                    "}" => {
                        self.braces.pop();
                    }
                    _ => {}
                }
                self.push(Tok::Punct(p), start);
            }
        }
        let end = self.b.len();
        self.tokens.push(Token {
            tok: Tok::Eof,
            start: end as u32,
            end: end as u32,
            nl_before: self.nl,
        });
        Ok(Lexed {
            tokens: self.tokens,
            comments: self.comments,
        })
    }

    fn skip_trivia(&mut self) -> Result<(), Diagnostic> {
        loop {
            let Some(c) = self.ch() else { return Ok(()) };
            if c == '\n' || c == '\r' || c == '\u{2028}' || c == '\u{2029}' {
                self.nl = true;
                self.i += c.len_utf8();
            } else if c.is_whitespace() || c == '\u{feff}' {
                self.i += c.len_utf8();
            } else if c == '/' && self.peek_byte(1) == b'/' {
                let start = self.i;
                while let Some(c) = self.ch() {
                    if c == '\n' || c == '\r' || c == '\u{2028}' || c == '\u{2029}' {
                        break;
                    }
                    self.i += c.len_utf8();
                }
                self.comments.push(Comment {
                    start: start as u32,
                    end: self.i as u32,
                    block: false,
                });
            } else if c == '/' && self.peek_byte(1) == b'*' {
                let start = self.i;
                let Some(close) = self.src[self.i + 2..].find("*/") else {
                    return Err(self.err(start, "Unterminated comment"));
                };
                let end = self.i + 2 + close + 2;
                if self.src[self.i..end].contains(['\n', '\r', '\u{2028}', '\u{2029}']) {
                    self.nl = true;
                }
                self.i = end;
                self.comments.push(Comment {
                    start: start as u32,
                    end: end as u32,
                    block: true,
                });
            } else {
                return Ok(());
            }
        }
    }

    fn ident(&mut self) -> Result<String, Diagnostic> {
        let mut name = String::new();
        let mut first = true;
        while let Some(c) = self.ch() {
            if c == '\\' {
                let at = self.i;
                if self.peek_byte(1) != b'u' {
                    return Err(self.err(at, "Invalid escape in identifier"));
                }
                self.i += 2;
                let ch = self
                    .unicode_escape()
                    .ok_or_else(|| self.err(at, "Invalid escape"))?;
                name.push(ch);
            } else if (first && is_id_start(c)) || (!first && is_id_part(c)) {
                name.push(c);
                self.i += c.len_utf8();
            } else {
                break;
            }
            first = false;
        }
        if name.is_empty() {
            return Err(self.err(self.i, "Identifier expected"));
        }
        Ok(name)
    }

    /// After `\u`: `XXXX` or `{X...}`.
    fn unicode_escape(&mut self) -> Option<char> {
        if self.peek_byte(0) == b'{' {
            let close = self.src[self.i..].find('}')?;
            let v = u32::from_str_radix(&self.src[self.i + 1..self.i + close], 16).ok()?;
            self.i += close + 1;
            char::from_u32(v)
        } else {
            let hex = self.src.get(self.i..self.i + 4)?;
            let v = u32::from_str_radix(hex, 16).ok()?;
            self.i += 4;
            Some(char::from_u32(v).unwrap_or('\u{fffd}'))
        }
    }

    fn number(&mut self) -> Result<Tok, Diagnostic> {
        let start = self.i;
        let radix = if self.peek_byte(0) == b'0' {
            match self.peek_byte(1) | 0x20 {
                b'x' => 16,
                b'o' => 8,
                b'b' => 2,
                _ => 10,
            }
        } else {
            10
        };
        let mut text = String::new();
        if radix != 10 {
            self.i += 2;
            while self.peek_byte(0).is_ascii_alphanumeric() || self.peek_byte(0) == b'_' {
                if self.peek_byte(0) == b'n' {
                    break;
                }
                if self.peek_byte(0) != b'_' {
                    text.push(self.peek_byte(0) as char);
                }
                self.i += 1;
            }
            if self.peek_byte(0) == b'n' {
                self.i += 1;
                return Ok(Tok::BigInt(self.src[start..self.i].to_string()));
            }
            let v = u64::from_str_radix(&text, radix)
                .map(|v| v as f64)
                .or_else(|_| {
                    // Wider than u64: accumulate in f64.
                    let mut acc = 0f64;
                    for d in text.chars() {
                        acc = acc * radix as f64 + d.to_digit(radix).ok_or(())? as f64;
                    }
                    Ok::<f64, ()>(acc)
                })
                .map_err(|_| self.err(start, "Invalid numeric literal"))?;
            return Ok(Tok::Num(v));
        }
        let digits = |lx: &mut Self, text: &mut String| {
            while lx.peek_byte(0).is_ascii_digit() || lx.peek_byte(0) == b'_' {
                if lx.peek_byte(0) != b'_' {
                    text.push(lx.peek_byte(0) as char);
                }
                lx.i += 1;
            }
        };
        digits(self, &mut text);
        if self.peek_byte(0) == b'n' {
            self.i += 1;
            return Ok(Tok::BigInt(self.src[start..self.i].to_string()));
        }
        if self.peek_byte(0) == b'.' {
            text.push('.');
            self.i += 1;
            digits(self, &mut text);
        }
        if self.peek_byte(0) | 0x20 == b'e'
            && (self.peek_byte(1).is_ascii_digit()
                || (matches!(self.peek_byte(1), b'+' | b'-') && self.peek_byte(2).is_ascii_digit()))
        {
            text.push('e');
            self.i += 1;
            if matches!(self.peek_byte(0), b'+' | b'-') {
                text.push(self.peek_byte(0) as char);
                self.i += 1;
            }
            digits(self, &mut text);
        }
        if text.starts_with('.') {
            text.insert(0, '0');
        }
        if text.ends_with('.') {
            text.push('0');
        }
        text.parse::<f64>()
            .map(Tok::Num)
            .map_err(|_| self.err(start, "Invalid numeric literal"))
    }

    /// Reads one escape after `\` into `out`; returns false at end of input.
    fn escape(&mut self, out: &mut String) -> bool {
        let Some(c) = self.ch() else { return false };
        self.i += c.len_utf8();
        match c {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' if !self.peek_byte(0).is_ascii_digit() => out.push('\0'),
            'x' => {
                let v = self
                    .src
                    .get(self.i..self.i + 2)
                    .and_then(|h| u32::from_str_radix(h, 16).ok());
                if let Some(v) = v {
                    self.i += 2;
                    out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                }
            }
            'u' => {
                if let Some(ch) = self.unicode_escape() {
                    out.push(ch);
                }
            }
            '\r' => {
                if self.peek_byte(0) == b'\n' {
                    self.i += 1;
                }
            }
            '\n' | '\u{2028}' | '\u{2029}' => {}
            other => out.push(other),
        }
        true
    }

    fn string(&mut self, quote: char) -> Result<String, Diagnostic> {
        let start = self.i;
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.ch() else {
                return Err(self.err(start, "Unterminated string literal"));
            };
            if c == quote {
                self.i += 1;
                return Ok(out);
            }
            if c == '\n' || c == '\r' {
                return Err(self.err(start, "Unterminated string literal"));
            }
            self.i += c.len_utf8();
            if c == '\\' {
                if !self.escape(&mut out) {
                    return Err(self.err(start, "Unterminated string literal"));
                }
            } else {
                out.push(c);
            }
        }
    }

    /// Lexes a template chunk; the opening backtick or `}` has been consumed.
    fn template(&mut self, start: usize, head: bool) -> Result<(), Diagnostic> {
        let mut cooked = String::new();
        loop {
            let Some(c) = self.ch() else {
                return Err(self.err(start, "Unterminated template literal"));
            };
            if c == '`' {
                self.i += 1;
                self.push(
                    Tok::Template {
                        cooked,
                        head,
                        tail: true,
                    },
                    start,
                );
                return Ok(());
            }
            if c == '$' && self.peek_byte(1) == b'{' {
                self.i += 2;
                self.braces.push(Brace::Template);
                self.push(
                    Tok::Template {
                        cooked,
                        head,
                        tail: false,
                    },
                    start,
                );
                return Ok(());
            }
            self.i += c.len_utf8();
            if c == '\\' {
                if !self.escape(&mut cooked) {
                    return Err(self.err(start, "Unterminated template literal"));
                }
            } else {
                cooked.push(c);
            }
        }
    }

    /// Lexes a regular expression literal at `/`; false (with `i` unspecified; the caller
    /// resets it) when the text there is not one.
    fn regex(&mut self) -> bool {
        self.i += 1;
        let mut class = false;
        loop {
            let Some(c) = self.ch() else {
                return false;
            };
            if c == '\n' || c == '\r' || c == '\u{2028}' || c == '\u{2029}' {
                return false;
            }
            self.i += c.len_utf8();
            match c {
                '\\' => match self.ch() {
                    Some('\n' | '\r' | '\u{2028}' | '\u{2029}') | None => return false,
                    Some(n) => self.i += n.len_utf8(),
                },
                '[' => class = true,
                ']' => class = false,
                '/' if !class => break,
                _ => {}
            }
        }
        while let Some(c) = self.ch() {
            if !is_id_part(c) {
                break;
            }
            self.i += c.len_utf8();
        }
        true
    }
}
