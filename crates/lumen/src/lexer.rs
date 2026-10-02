//! Hand-written tokenizer over the UTF-8 source (token offsets are bytes). Produces the full
//! token list up front and resolves the classic `/`-is-it-a-regex-or-division ambiguity by
//! tracking whether the previously emitted token can end an expression.

use crate::token::{Name, RegexTok, SubToks, Tok, TokVec, Token, TplPart};
use std::rc::Rc;

#[derive(Debug, Clone)]
pub struct LexError {
    pub message: String,
    pub line: u32,
    /// The lexer ran out of input mid-construct (unterminated template/comment/...). A REPL uses
    /// this to keep reading lines instead of reporting a SyntaxError.
    pub at_eof: bool,
}

struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    /// Byte offset of the cursor in `src`.
    pos: usize,
    line: u32,
    out: TokVec,
    nl_pending: bool,
    /// Script goal: Annex B HTML-like comments are recognised. Off for modules.
    html_comments: bool,
    /// Set while reading a string that contained a legacy octal / `\8` / `\9` escape.
    pending_legacy: bool,
    /// Set while reading a string that contained a lone (unpaired) surrogate `\u` escape.
    pending_lone_surrogate: bool,
    /// One entry per open `{`: `true` if it opened a block/function body (statement position),
    /// `false` if an object literal (expression position). Used to disambiguate a `/` after `}`.
    brace_stack: Vec<bool>,
    /// Byte offset where the token currently being scanned began.
    tok_start: usize,
    /// Classification of the most recently closed `}` (`true` = a block). A `/` after a block-closing
    /// `}` begins a regex; after an object-literal-closing `}` it is division.
    last_close_block: bool,
    /// Parenthesized control headers end at statement position; call/grouping parentheses
    /// produce values. A following slash must distinguish those before scanning a regex body.
    paren_stack: Vec<bool>,
    last_close_control: bool,
    /// One entry per `function` keyword whose body has not opened yet: `true` if the function is an
    /// *expression* (so its body `}` is followed by division), `false` if a *declaration* (its `}`
    /// ends a statement, so a `/` after it is a regex).
    pending_fn: Vec<bool>,
    /// Set when a `class` keyword is pushed: the next `{` is the class body, classified by
    /// whether the class was a declaration (statement position) or an expression.
    pending_class: Option<bool>,
    /// TypeScript source: a postfix non-null `!` (`x!`) ends an expression, so a `/` after it is
    /// a division.
    ts: bool,
    /// Where `/**` comments go (their byte ranges), when the caller asked for them (the typed
    /// tier's JSDoc side table). Only a JSDoc comment costs anything.
    docs: Option<Vec<(u32, u32)>>,
    /// The range starts in operator position: a leading `/` is a division (the parser re-lexing a
    /// regex it met where an operator belongs).
    start_div: bool,
    /// Identifier names seen so far (see `Tok::Ident`).
    idents: crate::fasthash::FastSet<Name>,
    /// See [`LexOpts::offset`].
    offset: u32,
}

/// Tokenize `src`. A lex error is reported as a SyntaxError by the caller.
pub fn tokenize(src: &str) -> Result<Vec<Token>, LexError> {
    tokenize_goal(src, true).map(TokVec::into_vec)
}

/// Tokenize with an explicit goal: `html_comments` is true for Scripts (Annex B `<!--`/`-->`
/// comments apply) and false for Modules (where they are ordinary punctuation, i.e. errors).
pub fn tokenize_goal(src: &str, html_comments: bool) -> Result<TokVec, LexError> {
    tokenize_range(src, html_comments, 1)
}

/// Tokenize a range cut out of a larger source (a function body parsed lazily): `line` is the
/// line its first char is on, so the tokens carry the same lines they would have had in a
/// whole-file tokenization. Token offsets are bytes into `src`; the parser maps them back
/// to byte offsets in the whole file. The range must start at a token boundary where no
/// regex/division ambiguity carries over — a body's `{` does.
pub fn tokenize_range(src: &str, html_comments: bool, line: u32) -> Result<TokVec, LexError> {
    tokenize_opts(src, html_comments, line, LexOpts::default()).map(|l| l.tokens)
}

/// What [`tokenize_opts`] produced.
pub(crate) struct Lexed {
    pub tokens: TokVec,
    /// Byte ranges of the `/** */` comments, when asked for.
    pub docs: Vec<(u32, u32)>,
    /// TypeScript only: the error the lexer stopped at. The tokens end there (with an `Eof`), and
    /// the parser reports it unless re-lexing a mis-guessed `/` from an earlier token gets past
    /// it (a regex misread as a division can run into a string or template).
    pub soft_err: Option<LexError>,
}

/// Lexer options beyond the goal (see [`tokenize_opts`]).
#[derive(Clone, Copy, Default)]
pub(crate) struct LexOpts {
    /// TypeScript source (see `Lexer::ts`).
    pub ts: bool,
    /// Record the byte ranges of `/** */` comments.
    pub docs: bool,
    /// A leading `/` is a division.
    pub start_div: bool,
    /// Added to every token's byte offsets (the range starts this far into the parser's source).
    pub offset: u32,
}

/// [`tokenize_range`] with [`LexOpts`]; also returns the recorded JSDoc comment ranges.
pub(crate) fn tokenize_opts(
    src: &str,
    html_comments: bool,
    line: u32,
    opts: LexOpts,
) -> Result<Lexed, LexError> {
    let mut lx = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        line,
        // About one token per six bytes of ordinary source.
        out: TokVec::with_capacity(src.len() / 6 + 16),
        nl_pending: false,
        html_comments,
        pending_legacy: false,
        pending_lone_surrogate: false,
        brace_stack: Vec::new(),
        tok_start: 0,
        last_close_block: false,
        paren_stack: Vec::new(),
        last_close_control: false,
        pending_fn: Vec::new(),
        pending_class: None,
        ts: opts.ts,
        docs: opts.docs.then(Vec::new),
        start_div: opts.start_div,
        idents: Default::default(),
        offset: opts.offset,
    };
    let soft_err = match lx.run() {
        Ok(()) => None,
        Err(e) if lx.ts => {
            lx.tok_start = lx.pos.min(src.len());
            lx.push(Tok::Eof);
            Some(e)
        }
        Err(e) => return Err(e),
    };
    Ok(Lexed {
        tokens: lx.out,
        docs: lx.docs.unwrap_or_default(),
        soft_err,
    })
}

impl Lexer<'_> {
    /// The token `k` places before the last one pushed (`0` = the last).
    fn tok_back(&self, k: usize) -> Option<&Token> {
        self.out.get(self.out.len().checked_sub(k + 1)?)
    }
    #[inline]
    fn char_at(&self, at: usize) -> Option<char> {
        let b = *self.bytes.get(at)?;
        if b < 0x80 {
            Some(b as char)
        } else {
            self.src[at..].chars().next()
        }
    }
    /// The byte at `pos + k` as a char: exact for ASCII, and never ASCII for a byte of a
    /// multi-byte char — for matching ASCII syntax a few bytes ahead.
    #[inline]
    fn byte_at(&self, k: usize) -> Option<char> {
        self.bytes.get(self.pos + k).map(|&b| b as char)
    }
    #[inline]
    fn peek(&self) -> Option<char> {
        self.char_at(self.pos)
    }
    fn peek2(&self) -> Option<char> {
        self.peek_at(1)
    }
    fn peek_at(&self, ahead: usize) -> Option<char> {
        let mut at = self.pos;
        for _ in 0..ahead {
            at += self.char_at(at)?.len_utf8();
        }
        self.char_at(at)
    }
    #[inline]
    fn bump(&mut self) -> Option<char> {
        let c = self.peek();
        if let Some(c) = c {
            self.pos += c.len_utf8();
            if c == '\n' {
                self.line += 1;
            }
        }
        c
    }
    fn err(&self, message: impl Into<String>) -> LexError {
        crate::parser::set_error_span(self.pos as u32, self.pos as u32 + 1);
        LexError {
            message: message.into(),
            line: self.line,
            at_eof: self.pos >= self.bytes.len(),
        }
    }

    /// Whether the previously emitted token permits a regex literal to follow (i.e. we are at the
    /// start of an expression). After a value-producing token (`)`, `]`, identifier, number, etc.)
    /// a `/` is division; otherwise it begins a regex.
    fn regex_allowed(&self) -> bool {
        match self.tok_back(0).map(|t| &t.kind) {
            None => !self.start_div,
            Some(Tok::Num(_) | Tok::BigInt(_) | Tok::Str(_) | Tok::Template(_) | Tok::Regex(_)) => {
                false
            }
            // `await`/`yield` are contextual: when they are keywords (module top level, async or
            // generator bodies) they prefix an expression, so a following `/` starts a regex. They
            // are `Ident` tokens here since the lexer lacks that context; allow the regex form —
            // division right after a bare `await`/`yield` *identifier* is vanishingly rare.
            Some(Tok::Ident(w)) => matches!(&**w, "await" | "yield"),
            Some(Tok::Keyword(k)) => !matches!(*k, "this" | "super" | "true" | "false" | "null"),
            // Calls/grouping and brackets produce values; a control header ends a statement
            // prefix. Braces distinguish a block (regex) from an object literal (division).
            Some(Tok::Punct(p)) => match *p {
                ")" => self.last_close_control,
                "]" => false,
                "}" => self.last_close_block,
                "!" if self.ts => !self.bang_is_postfix(),
                _ => true,
            },
            Some(Tok::Eof) => false,
        }
    }

    /// In TypeScript, whether the `!` just pushed is a non-null assertion (`x!`): it follows a
    /// token that ends an operand on the same line, where a prefix `!` could not stand.
    fn bang_is_postfix(&self) -> bool {
        let (Some(last), Some(before)) = (self.tok_back(0), self.tok_back(1)) else {
            return false;
        };
        if last.nl_before {
            return false;
        }
        match &before.kind {
            Tok::Ident(_) | Tok::Num(_) | Tok::BigInt(_) | Tok::Str(_) | Tok::Template(_) => true,
            Tok::Keyword(k) => matches!(*k, "this" | "super" | "null" | "true" | "false"),
            Tok::Punct(p) => matches!(*p, ")" | "]" | "!"),
            _ => false,
        }
    }

    /// Classify an opening `{` (about to be pushed): does the matching `}` end at statement position
    /// (a block, control body, or function *declaration* body → a following `/` is a regex) or at
    /// value position (an object literal or function *expression* body → division)?
    fn brace_ends_statement(&mut self) -> bool {
        let prev_is_paren = matches!(self.tok_back(0).map(|t| &t.kind), Some(Tok::Punct(")")));
        if prev_is_paren {
            // A body after `)` — either a function body (classified by `pending_fn`) or a control
            // block / method (statement position).
            return match self.pending_fn.pop() {
                Some(is_expr) => !is_expr,
                None => true,
            };
        }
        match self.tok_back(0).map(|t| &t.kind) {
            None => true,
            Some(Tok::Punct(p)) => matches!(*p, ";" | "{" | "}" | "=>"),
            Some(Tok::Keyword(k)) => matches!(*k, "else" | "do" | "try" | "finally"),
            _ => false,
        }
    }

    /// Whether a `class` keyword (about to be pushed) sits in statement position (a declaration —
    /// so its body's closing `}` ends a statement and a following `/` starts a regex).
    fn class_is_declaration(&self) -> bool {
        match self.tok_back(0).map(|t| &t.kind) {
            None => true,
            Some(Tok::Punct(p)) => match *p {
                ";" | "{" => true,
                "}" => self.last_close_block,
                _ => false,
            },
            Some(Tok::Keyword(k)) => {
                matches!(*k, "else" | "do" | "try" | "finally" | "export" | "default")
            }
            _ => false,
        }
    }

    /// Whether a `function` keyword (about to be pushed) sits in statement position (a declaration)
    /// rather than expression position.
    fn function_is_declaration(&self) -> bool {
        match self.tok_back(0).map(|t| &t.kind) {
            None => true,
            Some(Tok::Punct(p)) => match *p {
                ";" | "{" => true,
                "}" => self.last_close_block,
                _ => false,
            },
            Some(Tok::Keyword(k)) => {
                matches!(*k, "else" | "do" | "try" | "finally" | "export" | "default")
            }
            _ => false,
        }
    }

    fn push(&mut self, kind: Tok) {
        match &kind {
            Tok::Punct("(") => {
                let last = self.tok_back(0).map(|t| &t.kind);
                let before = self.tok_back(1).map(|t| &t.kind);
                let control = matches!(
                    last,
                    Some(Tok::Keyword(
                        "if" | "for" | "while" | "with" | "switch" | "catch"
                    ))
                ) && !matches!(before, Some(Tok::Punct("." | "?.")))
                    || matches!(last, Some(Tok::Ident(word)) if &**word == "await")
                        && matches!(before, Some(Tok::Keyword("for")));
                self.paren_stack.push(control);
            }
            Tok::Punct(")") => {
                self.last_close_control = self.paren_stack.pop().unwrap_or(false);
            }
            Tok::Keyword(k) if *k == "function" => {
                let is_expr = !self.function_is_declaration();
                self.pending_fn.push(is_expr);
            }
            Tok::Keyword(k) if *k == "class" => {
                self.pending_class = Some(self.class_is_declaration());
            }
            Tok::Punct(p) if *p == "{" => {
                // A pending `class` head claims this `{` as its body: the closing `}` sits at
                // statement position exactly when the class is a declaration.
                let ends_stmt = match self.pending_class.take() {
                    Some(is_decl) => is_decl,
                    None => self.brace_ends_statement(),
                };
                self.brace_stack.push(ends_stmt);
            }
            Tok::Punct(p) if *p == "}" => {
                self.last_close_block = self.brace_stack.pop().unwrap_or(false);
            }
            _ => {}
        }
        let nl = self.nl_pending;
        self.nl_pending = false;
        self.out.push(Token {
            kind,
            line: self.line,
            start: self.tok_start as u32 + self.offset,
            end: self.pos as u32 + self.offset,
            nl_before: nl,
            legacy_octal: false,
            escaped: false,
            lone_surrogate: false,
        });
    }
    /// Flag the most recently pushed token as having contained a `\u` escape.
    fn mark_escaped(&mut self) {
        if let Some(t) = self.out.last_mut() {
            t.escaped = true;
        }
    }
    /// Flag the most recently pushed token as containing a lone surrogate.
    fn mark_lone_surrogate(&mut self) {
        if let Some(t) = self.out.last_mut() {
            t.lone_surrogate = true;
        }
    }
    /// Flag the most recently pushed token as a legacy-octal construct.
    fn mark_legacy_octal(&mut self) {
        if let Some(t) = self.out.last_mut() {
            t.legacy_octal = true;
        }
    }

    fn run(&mut self) -> Result<(), LexError> {
        // Hashbang comment: `#!...` only at the very start of the source.
        if self.peek() == Some('#') && self.peek2() == Some('!') {
            while let Some(c) = self.peek() {
                if is_line_terminator(c) {
                    break;
                }
                self.bump();
            }
        }
        while let Some(c) = self.peek() {
            self.step(c)?;
        }
        self.tok_start = self.pos;
        self.push(Tok::Eof);
        Ok(())
    }

    /// Scan one token (or skip one whitespace char / comment) starting with `c`.
    #[inline]
    fn step(&mut self, c: char) -> Result<(), LexError> {
        self.tok_start = self.pos;
        if is_line_terminator(c) {
            self.nl_pending = true;
            self.bump();
        } else if c.is_whitespace() || c == '\u{FEFF}' {
            // ZWNBSP (U+FEFF) is JS whitespace anywhere in the source, not just as a BOM.
            self.bump();
        } else if c == '/' && self.peek2() == Some('/') {
            self.skip_line_comment();
        } else if c == '/' && self.peek2() == Some('*') {
            self.skip_block_comment()?;
        } else if c == '<'
            && self.html_comments
            && self.peek_at(1) == Some('!')
            && self.peek_at(2) == Some('-')
            && self.peek_at(3) == Some('-')
        {
            // Annex B HTML-like comment: `<!--` opens a single-line comment.
            self.skip_line_comment();
        } else if c == '-'
            && self.html_comments
            && self.peek_at(1) == Some('-')
            && self.peek_at(2) == Some('>')
            && (self.nl_pending || self.out.is_empty())
        {
            // Annex B: `-->` at the start of a line (or of the source) is a comment to EOL.
            self.skip_line_comment();
        } else if c == '/' && self.regex_allowed() {
            if self.ts {
                // A guess the parser may overturn: when no regex ends on this line, it is a
                // division (the parser re-lexes if a regex was meant after all).
                let (pos, line) = (self.pos, self.line);
                if self.read_regex().is_err() {
                    self.pos = pos;
                    self.line = line;
                    self.read_punct()?;
                }
            } else {
                self.read_regex()?;
            }
        } else if c == '"' || c == '\'' {
            self.read_string(c)?;
        } else if c == '`' {
            self.read_template()?;
        } else if c.is_ascii_digit()
            || (c == '.' && self.peek2().is_some_and(|d| d.is_ascii_digit()))
        {
            self.read_number()?;
        } else if is_ident_start(c) || c == '#' || (c == '\\' && self.peek2() == Some('u')) {
            self.read_ident()?;
        } else {
            self.read_punct()?;
        }
        Ok(())
    }

    fn skip_line_comment(&mut self) {
        while let Some(c) = self.peek() {
            if is_line_terminator(c) {
                break;
            }
            self.bump();
        }
    }

    fn skip_block_comment(&mut self) -> Result<(), LexError> {
        let start = self.pos;
        self.bump();
        self.bump();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated block comment")),
                Some('*') if self.peek() == Some('/') => {
                    self.bump();
                    if let Some(docs) = &mut self.docs {
                        // `/** ... */`, but not the empty `/**/`.
                        if self.bytes.get(start + 2) == Some(&b'*') && self.pos - start > 4 {
                            docs.push((start as u32, self.pos as u32));
                        }
                    }
                    return Ok(());
                }
                Some(c) if is_line_terminator(c) => self.nl_pending = true,
                _ => {}
            }
        }
    }

    fn read_ident(&mut self) -> Result<(), LexError> {
        let mut s = String::new();
        // A leading `#` (private name) is part of the identifier but not an ident-continue char.
        if self.peek() == Some('#') {
            s.push('#');
            self.bump();
        }
        // `\uXXXX` / `\u{...}` escapes may appear in an identifier; track that so an escaped reserved
        // word stays an Identifier (a keyword written with an escape is not the keyword).
        let mut had_escape = false;
        // The first code point must be IdentifierStart; the rest IdentifierPart. This holds for an
        // escaped code point too — so `#x` (escaped `#`) and a leading combining mark are errors.
        let mut first = true;
        // The ASCII run in one go (an ASCII IdentifierStart: digits went to `read_number`).
        if self.peek().is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$') {
            let from = self.pos;
            let mut end = from;
            while let Some(&c) = self.bytes.get(end) {
                if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' {
                    end += 1;
                } else {
                    break;
                }
            }
            self.pos = end;
            if !self.bytes.get(end).is_some_and(|&b| b == b'\\' || b >= 0x80) {
                // The whole identifier was that run: no copy beyond the interned name.
                let text = &self.src[self.tok_start..end];
                match keyword(text) {
                    Some(kw) => self.push(Tok::Keyword(kw)),
                    None => {
                        let name = self.intern(text);
                        self.push(Tok::Ident(name));
                    }
                }
                return Ok(());
            }
            s.push_str(&self.src[from..end]);
            first = false;
        }
        loop {
            match self.peek() {
                Some('\\') if self.peek2() == Some('u') => {
                    self.bump();
                    self.bump();
                    match self.read_unicode_escape_char() {
                        Some(ch) => {
                            let ok = if first {
                                is_ident_start(ch)
                            } else {
                                is_ident_part(ch)
                            };
                            if !ok {
                                return Err(self.err("invalid character in escaped identifier"));
                            }
                            had_escape = true;
                            s.push(ch);
                            first = false;
                        }
                        None => return Err(self.err("invalid unicode escape in identifier")),
                    }
                }
                Some(c) if (first && is_ident_start(c)) || (!first && is_ident_part(c)) => {
                    s.push(c);
                    self.bump();
                    first = false;
                }
                _ => break,
            }
        }
        // A reserved word is always a keyword — even spelled with a `\u` escape. An escaped reserved
        // word can't be an Identifier (the parser rejects a keyword there), but it still works as a
        // property name (keywords are accepted in those positions).
        if let Some(kw) = keyword(&s) {
            self.push(Tok::Keyword(kw));
            if had_escape {
                self.mark_escaped();
            }
            return Ok(());
        }
        let name = self.intern(&s);
        self.push(Tok::Ident(name));
        if had_escape {
            self.mark_escaped();
        }
        Ok(())
    }

    fn intern(&mut self, name: &str) -> Name {
        if let Some(n) = self.idents.get(name) {
            return n.clone();
        }
        let n = Name(Rc::from(name));
        self.idents.insert(n.clone());
        n
    }

    /// Read the body of a `\u` identifier/string escape (already consumed `\u`): either `{HEX+}` or
    /// exactly four hex digits, yielding the code point as a `char`.
    fn read_unicode_escape_char(&mut self) -> Option<char> {
        let mut hex = String::new();
        if self.peek() == Some('{') {
            self.bump();
            while let Some(c) = self.peek() {
                if c == '}' {
                    self.bump();
                    break;
                } else if c.is_ascii_hexdigit() {
                    hex.push(c);
                    self.bump();
                } else {
                    return None;
                }
            }
        } else {
            for _ in 0..4 {
                match self.peek() {
                    Some(c) if c.is_ascii_hexdigit() => {
                        hex.push(c);
                        self.bump();
                    }
                    _ => return None,
                }
            }
        }
        u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
    }

    fn read_string(&mut self, quote: char) -> Result<(), LexError> {
        self.bump();
        let mut s = String::new();
        let mut had_escape = false;
        self.pending_legacy = false;
        self.pending_lone_surrogate = false;
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated string literal")),
                Some(c) if c == quote => break,
                Some('\\') => {
                    had_escape = true;
                    self.read_escape(&mut s)?
                }
                // U+2028/U+2029 may appear literally in a string (json-superset); only CR/LF end it.
                Some(c @ ('\u{2028}' | '\u{2029}')) => s.push(c),
                Some(c) if is_line_terminator(c) => {
                    return Err(self.err("unterminated string literal"))
                }
                Some(c) => s.push(c),
            }
        }
        self.push(Tok::Str(Rc::from(s)));
        // A directive prologue string that used escapes never matches "use strict".
        if had_escape {
            self.mark_escaped();
        }
        if self.pending_legacy {
            self.mark_legacy_octal();
            self.pending_legacy = false;
        }
        if self.pending_lone_surrogate {
            self.mark_lone_surrogate();
            self.pending_lone_surrogate = false;
        }
        Ok(())
    }

    fn read_template(&mut self) -> Result<(), LexError> {
        let (start, nl) = (self.tok_start, std::mem::take(&mut self.nl_pending));
        let parts = self.read_template_parts()?;
        self.tok_start = start;
        self.nl_pending = nl;
        self.push(Tok::Template(Box::new(parts)));
        Ok(())
    }

    fn read_template_parts(&mut self) -> Result<Vec<TplPart>, LexError> {
        self.bump(); // opening backtick
        let mut parts: Vec<TplPart> = Vec::new();
        let mut cooked = String::new();
        // An invalid escape poisons the chunk's cooked value (None) instead of erroring: the
        // parser rejects it later unless the template is tagged.
        let mut invalid = false;
        let mut raw_start = self.pos; // raw source of the current chunk starts here
        loop {
            // The template's raw value normalizes line terminators: <CR><LF> and <CR> → <LF>.
            let raw_of = |text: &str| -> String {
                if !text.contains('\r') {
                    return text.to_string();
                }
                text.replace("\r\n", "\n").replace('\r', "\n")
            };
            match self.peek() {
                None => return Err(self.err("unterminated template literal")),
                Some('`') => {
                    let raw = raw_of(&self.src[raw_start..self.pos]);
                    self.bump();
                    parts.push(TplPart::Str {
                        cooked: (!std::mem::take(&mut invalid))
                            .then(|| std::mem::take(&mut cooked)),
                        raw,
                    });
                    break;
                }
                Some('$') if self.bytes.get(self.pos + 1) == Some(&b'{') => {
                    let raw = raw_of(&self.src[raw_start..self.pos]);
                    parts.push(TplPart::Str {
                        cooked: (!std::mem::take(&mut invalid))
                            .then(|| std::mem::take(&mut cooked)),
                        raw,
                    });
                    cooked.clear();
                    self.bump(); // '$'
                    self.bump(); // '{'
                    parts.push(self.read_template_sub()?);
                    raw_start = self.pos;
                }
                Some('\\') => {
                    // Octal / `\8` / `\9` / malformed hex-unicode escapes poison the chunk's
                    // cooked value; a tagged template accepts them (cooked = undefined).
                    if !self.template_escape_ok() {
                        invalid = true;
                        self.bump(); // the backslash
                        self.bump(); // the character after it
                        continue;
                    }
                    self.pending_legacy = false;
                    self.bump(); // consume the backslash
                    self.read_escape(&mut cooked)?;
                    if self.pending_legacy {
                        invalid = true;
                    }
                }
                Some('\r') => {
                    // The cooked value normalizes <CR><LF> and lone <CR> to <LF> (like raw).
                    self.bump();
                    if self.peek() == Some('\n') {
                        self.bump();
                    }
                    cooked.push('\n');
                }
                Some(c) => {
                    self.bump();
                    cooked.push(c);
                }
            }
        }
        Ok(parts)
    }

    /// Lex the inside of a `${ ... }` hole in place, up to its closing `}`, into a token list of
    /// its own ending with an `Eof` there. Nested templates recurse.
    fn read_template_sub(&mut self) -> Result<TplPart, LexError> {
        if crate::stack::exhausted() {
            return Err(self.err("template literal nesting too deep"));
        }
        let saved = (
            std::mem::replace(&mut self.out, TokVec::with_capacity(16)),
            std::mem::take(&mut self.brace_stack),
            std::mem::take(&mut self.paren_stack),
            std::mem::take(&mut self.pending_fn),
            self.pending_class.take(),
            self.last_close_block,
            self.last_close_control,
            std::mem::take(&mut self.start_div),
        );
        let r = loop {
            match self.peek() {
                None => break Err(self.err("unterminated template substitution")),
                Some('}') if self.brace_stack.is_empty() => {
                    self.tok_start = self.pos;
                    self.push(Tok::Eof);
                    self.bump();
                    break Ok(());
                }
                Some(c) => {
                    if let Err(e) = self.step(c) {
                        break Err(e);
                    }
                }
            }
        };
        let toks = std::mem::replace(&mut self.out, saved.0);
        self.brace_stack = saved.1;
        self.paren_stack = saved.2;
        self.pending_fn = saved.3;
        self.pending_class = saved.4;
        self.last_close_block = saved.5;
        self.last_close_control = saved.6;
        self.start_div = saved.7;
        r.map(|()| TplPart::Sub(SubToks(Rc::new(toks))))
    }

    /// Whether the escape starting at the current `\` is valid in a template's cooked string:
    /// no octal / `\8` / `\9`, `\x` needs two hex digits, `\u` four (or a braced code point).
    fn template_escape_ok(&self) -> bool {
        let at = |k: usize| self.byte_at(k);
        let hex = |c: Option<char>| c.is_some_and(|c| c.is_ascii_hexdigit());
        match at(1) {
            Some('0') => !matches!(at(2), Some(c) if c.is_ascii_digit()),
            Some(c @ '1'..='9') => {
                let _ = c;
                false
            }
            Some('x') => hex(at(2)) && hex(at(3)),
            Some('u') => {
                if at(2) == Some('{') {
                    let mut k = 3;
                    let mut digits = 0u32;
                    let mut v: u64 = 0;
                    loop {
                        match at(k) {
                            Some('}') => return digits > 0 && v <= 0x10FFFF,
                            Some(c) if c.is_ascii_hexdigit() => {
                                digits += 1;
                                v = (v.saturating_mul(16)) + c.to_digit(16).unwrap() as u64;
                                if digits > 8 {
                                    return false;
                                }
                            }
                            _ => return false,
                        }
                        k += 1;
                    }
                } else {
                    hex(at(2)) && hex(at(3)) && hex(at(4)) && hex(at(5))
                }
            }
            None => false,
            _ => true,
        }
    }

    fn read_escape(&mut self, out: &mut String) -> Result<(), LexError> {
        match self.bump() {
            None => Err(self.err("unterminated escape")),
            Some('n') => {
                out.push('\n');
                Ok(())
            }
            Some('t') => {
                out.push('\t');
                Ok(())
            }
            Some('r') => {
                out.push('\r');
                Ok(())
            }
            Some('b') => {
                out.push('\u{0008}');
                Ok(())
            }
            Some('f') => {
                out.push('\u{000C}');
                Ok(())
            }
            Some('v') => {
                out.push('\u{000B}');
                Ok(())
            }
            Some('0') if !self.peek().is_some_and(|c| c.is_ascii_digit()) => {
                out.push('\0');
                Ok(())
            }
            // Legacy octal escape: 1-3 octal digits (first three only if value <= 0o377).
            Some(c @ '0'..='7') => {
                let mut val = c.to_digit(8).unwrap();
                let max = if c <= '3' { 2 } else { 1 };
                let mut taken = 0;
                while taken < max && self.peek().is_some_and(|d| ('0'..='7').contains(&d)) {
                    val = val * 8 + self.bump().unwrap().to_digit(8).unwrap();
                    taken += 1;
                }
                out.push(char::from_u32(val).unwrap_or('\u{FFFD}'));
                self.pending_legacy = true;
                Ok(())
            }
            // `\8` / `\9` (NonOctalDecimalEscape): the digit itself, but still legacy.
            Some(c @ ('8' | '9')) => {
                out.push(c);
                self.pending_legacy = true;
                Ok(())
            }
            Some('x') => {
                let hi = self.bump().ok_or_else(|| self.err("bad \\x escape"))?;
                let lo = self.bump().ok_or_else(|| self.err("bad \\x escape"))?;
                let n = u32::from_str_radix(&format!("{hi}{lo}"), 16)
                    .map_err(|_| self.err("bad \\x escape"))?;
                out.push(char::from_u32(n).unwrap_or('\u{FFFD}'));
                Ok(())
            }
            Some('u') => self.read_unicode_escape(out),
            Some('\r') => {
                // A CRLF pair is a single LineTerminatorSequence for a line continuation.
                if self.peek() == Some('\n') {
                    self.bump();
                }
                Ok(())
            }
            Some(c) if is_line_terminator(c) => Ok(()), // line continuation
            Some(c) => {
                out.push(c);
                Ok(())
            }
        }
    }

    fn read_unicode_escape(&mut self, out: &mut String) -> Result<(), LexError> {
        let mut hex = String::new();
        if self.peek() == Some('{') {
            self.bump();
            while let Some(c) = self.peek() {
                if c == '}' {
                    break;
                }
                hex.push(c);
                self.bump();
            }
            if self.bump() != Some('}') {
                return Err(self.err("unterminated \\u{...} escape"));
            }
        } else {
            for _ in 0..4 {
                hex.push(self.bump().ok_or_else(|| self.err("bad \\u escape"))?);
            }
        }
        let n = u32::from_str_radix(&hex, 16).map_err(|_| self.err("bad \\u escape"))?;
        if n > 0x10FFFF {
            return Err(self.err("undefined Unicode code-point"));
        }
        if n >= crate::jstr::SMUGGLE_BASE {
            crate::jstr::push_char_utf16(out, char::from_u32(n).unwrap_or('\u{FFFD}'));
            return Ok(());
        }
        if let Some(c) = char::from_u32(n) {
            out.push(c);
            return Ok(());
        }
        // `n` is a surrogate code point. A high surrogate followed by a `\u` low surrogate combines
        // into a single astral code point; anything else is a lone surrogate (representable as a JS
        // string but flagged so it can be rejected as a ModuleExportName).
        if (0xD800..=0xDBFF).contains(&n) && self.peek() == Some('\\') && self.peek2() == Some('u')
        {
            let save = self.pos;
            self.bump(); // '\'
            self.bump(); // 'u'
            let mut lo = String::new();
            let mut ok = true;
            if self.peek() == Some('{') {
                self.bump();
                while let Some(c) = self.peek() {
                    if c == '}' {
                        break;
                    }
                    lo.push(c);
                    self.bump();
                }
                if self.bump() != Some('}') {
                    ok = false;
                }
            } else {
                for _ in 0..4 {
                    match self.peek() {
                        Some(c) if c.is_ascii_hexdigit() => {
                            lo.push(c);
                            self.bump();
                        }
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            let low = if ok {
                u32::from_str_radix(&lo, 16).ok()
            } else {
                None
            };
            if let Some(low) = low.filter(|l| (0xDC00..=0xDFFF).contains(l)) {
                let cp = 0x10000 + ((n - 0xD800) << 10) + (low - 0xDC00);
                if cp < crate::jstr::SMUGGLE_BASE {
                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                } else {
                    // A smuggle-range character is canonically its smuggled pair.
                    out.push(crate::jstr::smuggle(n as u16));
                    out.push(crate::jstr::smuggle(low as u16));
                }
                return Ok(());
            }
            // Not a valid low surrogate: rewind and treat the high surrogate as lone.
            self.pos = save;
        }
        self.pending_lone_surrogate = true;
        out.push(crate::jstr::smuggle(n as u16));
        Ok(())
    }

    /// A numeric separator `_` is only legal immediately between two digits of the given radix.
    fn validate_seps(&self, lo: usize, hi: usize, radix: u32) -> Result<(), LexError> {
        let s = &self.bytes[lo..hi];
        for (i, &c) in s.iter().enumerate() {
            if c == b'_' {
                let prev = i.checked_sub(1).and_then(|j| s.get(j));
                let next = s.get(i + 1);
                let ok = prev.is_some_and(|&p| (p as char).is_digit(radix))
                    && next.is_some_and(|&n| (n as char).is_digit(radix));
                if !ok {
                    return Err(self.err("invalid use of numeric separator"));
                }
            }
        }
        Ok(())
    }

    fn read_number(&mut self) -> Result<(), LexError> {
        self.read_number_inner()?;
        // The SourceCharacter following a NumericLiteral must not be an IdentifierStart or digit.
        if self
            .peek()
            .is_some_and(|c| is_ident_start(c) || c.is_ascii_digit() || c == '\\')
        {
            return Err(self.err("identifier starts immediately after numeric literal"));
        }
        Ok(())
    }

    fn read_number_inner(&mut self) -> Result<(), LexError> {
        let start = self.pos;
        let mut radix = 10u32;
        if self.peek() == Some('0') {
            match self.peek2() {
                Some('x' | 'X') => radix = 16,
                Some('o' | 'O') => radix = 8,
                Some('b' | 'B') => radix = 2,
                _ => {}
            }
        }
        if radix != 10 {
            self.bump();
            self.bump();
            let digits_start = self.pos;
            while let Some(c) = self.peek() {
                if c == '_' || c.is_digit(radix) {
                    self.bump();
                } else {
                    break;
                }
            }
            self.validate_seps(digits_start, self.pos, radix)?;
            let digits: String = self.src[digits_start..self.pos]
                .chars()
                .filter(|c| *c != '_')
                .collect();
            if self.peek() == Some('n') {
                self.bump();
                let n = crate::bigint::JsBigInt::parse_radix(&digits, radix)
                    .ok_or_else(|| self.err("invalid BigInt literal"))?;
                self.push(Tok::BigInt(n));
                return Ok(());
            }
            let n = u64::from_str_radix(&digits, radix)
                .map_err(|_| self.err("invalid numeric literal"))?;
            self.push(Tok::Num(n as f64));
            return Ok(());
        }
        // Decimal: integer . fraction e exponent
        while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '_') {
            self.bump();
        }
        // Legacy octal (`010`) / non-octal-decimal (`08`): a leading-zero integer with no fraction,
        // exponent, or `n` suffix. Octal value in sloppy mode; the parser rejects it in strict.
        if self.bytes[start] == b'0'
            && self.pos - start > 1
            && !matches!(self.peek(), Some('.' | 'e' | 'E' | 'n' | '_'))
        {
            // A leading-zero integer (legacy octal / non-octal decimal) admits no separators.
            if self.src[start..self.pos].contains('_') {
                return Err(self.err("numeric separator not allowed in legacy literal"));
            }
            let text = &self.src[start..self.pos];
            if text.chars().all(|c| ('0'..='7').contains(&c)) {
                let n = i64::from_str_radix(text, 8).unwrap_or(0);
                self.push(Tok::Num(n as f64));
                self.mark_legacy_octal();
                return Ok(());
            } else if text.chars().all(|c| c.is_ascii_digit()) {
                let n: f64 = text.parse().unwrap_or(0.0);
                self.push(Tok::Num(n));
                self.mark_legacy_octal();
                return Ok(());
            }
        }
        // A BigInt literal is an integer immediately followed by `n` (no fraction/exponent).
        if self.peek() == Some('n') {
            // A leading-zero integer (legacy octal / non-octal decimal) admits no BigInt suffix.
            if self.bytes[start] == b'0' && self.pos - start > 1 {
                return Err(self.err("invalid BigInt literal (leading zero)"));
            }
            self.validate_seps(start, self.pos, 10)?;
            let text: String = self.src[start..self.pos]
                .chars()
                .filter(|c| *c != '_')
                .collect();
            self.bump(); // n
            let n = crate::bigint::JsBigInt::parse_radix(&text, 10)
                .ok_or_else(|| self.err("invalid BigInt literal"))?;
            self.push(Tok::BigInt(n));
            return Ok(());
        }
        if self.peek() == Some('.') {
            self.bump();
            while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '_') {
                self.bump();
            }
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            self.bump();
            if matches!(self.peek(), Some('+' | '-')) {
                self.bump();
            }
            while self.peek().is_some_and(|c| c.is_ascii_digit() || c == '_') {
                self.bump();
            }
        }
        self.validate_seps(start, self.pos, 10)?;
        let text: String = self.src[start..self.pos]
            .chars()
            .filter(|c| *c != '_')
            .collect();
        let n: f64 = text
            .parse()
            .map_err(|_| self.err("invalid numeric literal"))?;
        self.push(Tok::Num(n));
        Ok(())
    }

    fn read_regex(&mut self) -> Result<(), LexError> {
        self.bump(); // opening /
        let mut body = String::new();
        let mut in_class = false;
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated regular expression")),
                Some(c) if is_line_terminator(c) => {
                    return Err(self.err("unterminated regular expression"))
                }
                Some('\\') => {
                    body.push('\\');
                    // A backslash sequence can't contain a line terminator either.
                    match self.bump() {
                        Some(c) if is_line_terminator(c) => {
                            return Err(self.err("unterminated regular expression"))
                        }
                        Some(c) => body.push(c),
                        None => return Err(self.err("unterminated regular expression")),
                    }
                }
                Some('[') => {
                    in_class = true;
                    body.push('[');
                }
                Some(']') => {
                    in_class = false;
                    body.push(']');
                }
                Some('/') if !in_class => break,
                Some(c) => body.push(c),
            }
        }
        let mut flags = String::new();
        while let Some(c) = self.peek() {
            if is_ident_part(c) {
                flags.push(c);
                self.bump();
            } else {
                break;
            }
        }
        self.push(Tok::Regex(Box::new(RegexTok { body, flags })));
        Ok(())
    }

    fn read_punct(&mut self) -> Result<(), LexError> {
        // `?.` followed by a digit is `?` then `.` (a conditional like `x ? .5 : .3`), not optional
        // chaining.
        if self.peek() == Some('?')
            && self.peek2() == Some('.')
            && self
                .bytes
                .get(self.pos + 2)
                .is_some_and(|c| c.is_ascii_digit())
        {
            self.bump();
            self.push(Tok::Punct("?"));
            return Ok(());
        }
        // The longest punctuator at `pos` (the same maximal munch as scanning `PUNCTUATORS`,
        // which lists longer spellings first).
        let at = |k: usize| self.byte_at(k).unwrap_or('\0');
        let (c0, c1, c2, c3) = (at(0), at(1), at(2), at(3));
        let p: &'static str = match (c0, c1, c2, c3) {
            ('>', '>', '>', '=') => ">>>=",
            ('.', '.', '.', _) => "...",
            ('=', '=', '=', _) => "===",
            ('!', '=', '=', _) => "!==",
            ('*', '*', '=', _) => "**=",
            ('<', '<', '=', _) => "<<=",
            ('>', '>', '=', _) => ">>=",
            ('>', '>', '>', _) => ">>>",
            ('&', '&', '=', _) => "&&=",
            ('|', '|', '=', _) => "||=",
            ('?', '?', '=', _) => "??=",
            ('=', '>', _, _) => "=>",
            ('=', '=', _, _) => "==",
            ('!', '=', _, _) => "!=",
            ('<', '=', _, _) => "<=",
            ('>', '=', _, _) => ">=",
            ('&', '&', _, _) => "&&",
            ('|', '|', _, _) => "||",
            ('?', '?', _, _) => "??",
            ('?', '.', _, _) => "?.",
            ('+', '+', _, _) => "++",
            ('-', '-', _, _) => "--",
            ('+', '=', _, _) => "+=",
            ('-', '=', _, _) => "-=",
            ('*', '=', _, _) => "*=",
            ('/', '=', _, _) => "/=",
            ('%', '=', _, _) => "%=",
            ('&', '=', _, _) => "&=",
            ('|', '=', _, _) => "|=",
            ('^', '=', _, _) => "^=",
            ('*', '*', _, _) => "**",
            ('<', '<', _, _) => "<<",
            ('>', '>', _, _) => ">>",
            ('{', ..) => "{",
            ('}', ..) => "}",
            ('(', ..) => "(",
            (')', ..) => ")",
            ('[', ..) => "[",
            (']', ..) => "]",
            ('.', ..) => ".",
            (';', ..) => ";",
            (',', ..) => ",",
            ('<', ..) => "<",
            ('>', ..) => ">",
            ('+', ..) => "+",
            ('-', ..) => "-",
            ('*', ..) => "*",
            ('/', ..) => "/",
            ('%', ..) => "%",
            ('&', ..) => "&",
            ('|', ..) => "|",
            ('^', ..) => "^",
            ('!', ..) => "!",
            ('~', ..) => "~",
            ('?', ..) => "?",
            (':', ..) => ":",
            ('=', ..) => "=",
            ('@', ..) => "@",
            _ => {
                return Err(self.err(format!(
                    "unexpected character {:?}",
                    self.peek().unwrap_or('\0')
                )))
            }
        };
        for _ in 0..p.len() {
            self.bump();
        }
        self.push(Tok::Punct(p));
        Ok(())
    }
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}
/// Whether code point `c` is in the Unicode property `name` (an ASCII-free path; the bundled UCD
/// tables give the exact ID_Start/ID_Continue sets).
fn prop_has(name: &str, c: char) -> bool {
    let u = c as u32;
    crate::unicode_props::lookup(name, None)
        .map(|r| {
            r.binary_search_by(|&(lo, hi)| {
                if u < lo {
                    std::cmp::Ordering::Greater
                } else if u > hi {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
        })
        .unwrap_or(false)
}
/// The reserved word spelled `s` (one of `token::KEYWORDS`).
fn keyword(s: &str) -> Option<&'static str> {
    Some(match s {
        "break" => "break",
        "case" => "case",
        "catch" => "catch",
        "class" => "class",
        "const" => "const",
        "continue" => "continue",
        "debugger" => "debugger",
        "default" => "default",
        "delete" => "delete",
        "do" => "do",
        "else" => "else",
        "enum" => "enum",
        "export" => "export",
        "extends" => "extends",
        "false" => "false",
        "finally" => "finally",
        "for" => "for",
        "function" => "function",
        "if" => "if",
        "import" => "import",
        "in" => "in",
        "instanceof" => "instanceof",
        "new" => "new",
        "null" => "null",
        "return" => "return",
        "super" => "super",
        "switch" => "switch",
        "this" => "this",
        "throw" => "throw",
        "true" => "true",
        "try" => "try",
        "typeof" => "typeof",
        "var" => "var",
        "void" => "void",
        "while" => "while",
        "with" => "with",
        _ => return None,
    })
}

fn is_ident_start(c: char) -> bool {
    // IdentifierStart = ID_Start ∪ {$, _} (plus `\u` escapes, handled by the caller).
    if c.is_ascii() {
        return c == '_' || c == '$' || c.is_ascii_alphabetic();
    }
    prop_has("ID_Start", c)
}
fn is_ident_part(c: char) -> bool {
    // IdentifierPart = ID_Continue ∪ {$, _, ZWNJ, ZWJ}.
    if c.is_ascii() {
        return c == '_' || c == '$' || c.is_ascii_alphanumeric();
    }
    c == '\u{200C}' || c == '\u{200D}' || prop_has("ID_Continue", c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::{KEYWORDS, PUNCTUATORS};

    #[test]
    fn keyword_match_covers_the_table() {
        for k in KEYWORDS {
            assert_eq!(keyword(k), Some(*k));
        }
        assert_eq!(keyword("let"), None);
        assert_eq!(keyword("functions"), None);
    }

    #[test]
    fn punctuators_lex_by_maximal_munch() {
        for p in PUNCTUATORS {
            // A trailing space ends the token; `/` alone would start a regex in this position.
            let src = format!("x {p} ");
            let toks = tokenize(&src).unwrap();
            assert!(matches!(toks[1].kind, Tok::Punct(q) if q == *p), "{p}");
        }
        let toks = tokenize("a>>>=b?.5:.3").unwrap();
        let puncts: Vec<&str> = toks
            .iter()
            .filter_map(|t| match t.kind {
                Tok::Punct(p) => Some(p),
                _ => None,
            })
            .collect();
        assert_eq!(puncts, [">>>=", "?", ":"]);
    }
}
