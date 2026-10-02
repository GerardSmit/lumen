//! JSON for both languages and the tools: string quoting ([`quote`]), a parser that builds each
//! caller's own values through a [`Sink`] ([`Parser`]), and a plain [`Value`] tree on top of it
//! ([`parse`], [`parse_jsonc`]).
//!
//! Strings are read and written in one of three [`Spelling`]s: plain Rust text, JavaScript's
//! UTF-16 strings or Python's code-point strings (see [`crate::smuggle`]), so lone surrogates
//! round-trip in the languages that have them. Error positions follow CPython's `_json` (as byte
//! offsets into the source).

use crate::smuggle;
use std::borrow::Cow;
use std::fmt;

/// How the strings being quoted or parsed carry characters a Rust `str` cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spelling {
    /// Plain text: a `\u` escape of a lone surrogate reads as U+FFFD.
    Plain,
    /// JavaScript strings: lone surrogates smuggled as UTF-16 code units.
    Utf16,
    /// Python strings: every code point, with the escape spelling of the reserved block.
    CodePoints,
}

// ---- quoting ----------------------------------------------------------------------------------

/// Which characters a quoted string escapes, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escapes {
    /// JSON's: `\" \\ \b \f \n \r \t`, any other control character as `\u00xx`.
    Json,
    /// JSON's, plus U+2028 and U+2029: a literal for generated JavaScript source.
    JsSource,
    /// Node's `util.inspect`: `\b \t \n \f \r`, other C0/C1 controls and DEL as `\xHH`.
    Inspect,
}

/// The options of [`quote`].
#[derive(Clone, Copy, Debug)]
pub struct Quote {
    /// The delimiter, escaped where it occurs inside (an ASCII character).
    pub quote: u8,
    pub escapes: Escapes,
    /// Escape everything outside printable ASCII as `\uxxxx` (pairs beyond the BMP).
    pub ascii_only: bool,
    /// Escape lone surrogates as `\udxxx` (`JSON.stringify`'s well-formed output).
    pub lone_surrogates: bool,
    pub spelling: Spelling,
}

impl Quote {
    /// A JSON string literal of plain text.
    pub const JSON: Quote = Quote {
        quote: b'"',
        escapes: Escapes::Json,
        ascii_only: false,
        lone_surrogates: false,
        spelling: Spelling::Plain,
    };
    /// A double-quoted JavaScript literal of plain text.
    pub const JS_SOURCE: Quote = Quote { escapes: Escapes::JsSource, ..Quote::JSON };
}

/// `s` quoted as `q` says.
pub fn quote(s: &str, q: &Quote) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    quote_into(&mut out, s, q);
    out
}

/// A JSON string literal of the plain text `s`.
pub fn json_string(s: &str) -> String {
    quote(s, &Quote::JSON)
}

/// Append `s`, quoted as `q` says, to `out`.
#[inline]
pub fn quote_into(out: &mut String, s: &str, q: &Quote) {
    out.reserve(s.len() + 2);
    out.push(q.quote as char);
    escape_into(out, s, q);
    out.push(q.quote as char);
}

/// Append the escaped contents of `s` (without delimiters) to `out`.
#[inline]
pub fn escape_into(out: &mut String, s: &str, q: &Quote) {
    let b = s.as_bytes();
    let del_escaped = q.ascii_only || q.escapes == Escapes::Inspect;
    let smuggled = q.spelling != Spelling::Plain && (q.lone_surrogates || q.ascii_only);
    let mut run = 0;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c < 0x80 {
            if c >= 0x20 && c != b'\\' && c != q.quote && !(c == 0x7f && del_escaped) {
                i += 1;
                continue;
            }
            out.push_str(&s[run..i]);
            push_escape(out, c as u32, q);
            i += 1;
            run = i;
            continue;
        }
        let decode = q.ascii_only
            || (c == 0xF4 && smuggled)
            || (c == 0xC2 && q.escapes == Escapes::Inspect)
            || (c == 0xE2 && q.escapes == Escapes::JsSource);
        if !decode {
            i += utf8_len(c);
            continue;
        }
        let (cp, n) = decode_at(s, i, q.spelling);
        let escape = q.ascii_only
            || (q.lone_surrogates && (0xD800..0xE000).contains(&cp))
            || (q.escapes == Escapes::Inspect && (0x80..0xA0).contains(&cp))
            || (q.escapes == Escapes::JsSource && (cp == 0x2028 || cp == 0x2029));
        if escape {
            out.push_str(&s[run..i]);
            push_escape(out, cp, q);
            run = i + n;
        }
        i += n;
    }
    out.push_str(&s[run..]);
}

#[inline]
fn utf8_len(lead: u8) -> usize {
    match lead {
        0xF0.. => 4,
        0xE0.. => 3,
        _ => 2,
    }
}

/// The code point at byte `i` of `s` as `spelling` reads it, and its length in bytes.
fn decode_at(s: &str, i: usize, spelling: Spelling) -> (u32, usize) {
    let c = s[i..].chars().next().unwrap_or('\0');
    match spelling {
        Spelling::Plain => (c as u32, c.len_utf8()),
        Spelling::CodePoints => smuggle::decode_at(s, i),
        Spelling::Utf16 => match smuggle::smuggled(c) {
            Some(hi) if smuggle::smuggled_high(c).is_some() => {
                let next = s[i + 4..].chars().next();
                match next.and_then(smuggle::smuggled_low) {
                    Some(lo) => (0x10000 + ((hi as u32 - 0xD800) << 10) + (lo as u32 - 0xDC00), 8),
                    None => (hi as u32, 4),
                }
            }
            Some(u) => (u as u32, 4),
            None => (c as u32, c.len_utf8()),
        },
    }
}

fn push_escape(out: &mut String, cp: u32, q: &Quote) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let named = match cp {
        0x5C => Some('\\'),
        0x08 => Some('b'),
        0x0C => Some('f'),
        0x0A => Some('n'),
        0x0D => Some('r'),
        0x09 => Some('t'),
        c if c == q.quote as u32 => Some(q.quote as char),
        _ => None,
    };
    if let Some(n) = named {
        out.push('\\');
        out.push(n);
        return;
    }
    if q.escapes == Escapes::Inspect && (cp < 0x20 || (0x7F..0xA0).contains(&cp)) {
        const UPPER: &[u8; 16] = b"0123456789ABCDEF";
        out.push_str("\\x");
        out.push(UPPER[(cp >> 4) as usize & 0xF] as char);
        out.push(UPPER[cp as usize & 0xF] as char);
        return;
    }
    let mut unit = |u: u32| {
        out.push_str("\\u");
        for shift in [12, 8, 4, 0] {
            out.push(HEX[(u >> shift) as usize & 0xF] as char);
        }
    };
    if cp >= 0x10000 {
        let v = cp - 0x10000;
        unit(0xD800 + (v >> 10));
        unit(0xDC00 + (v & 0x3FF));
    } else {
        unit(cp);
    }
}

// ---- parsing ----------------------------------------------------------------------------------

/// What went wrong, with CPython's message for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// No value where one must start (Python's scanner raises `StopIteration` for it).
    ExpectingValue,
    ExpectingKey,
    ExpectingColon,
    ExpectingComma,
    /// Text after a complete document.
    ExtraData,
    UnterminatedString,
    ControlCharacter,
    BadEscape,
    BadUnicodeEscape,
    UnterminatedComment,
    TooDeep,
}

impl ErrorKind {
    pub fn message(self) -> &'static str {
        match self {
            ErrorKind::ExpectingValue => "Expecting value",
            ErrorKind::ExpectingKey => "Expecting property name enclosed in double quotes",
            ErrorKind::ExpectingColon => "Expecting ':' delimiter",
            ErrorKind::ExpectingComma => "Expecting ',' delimiter",
            ErrorKind::ExtraData => "Extra data",
            ErrorKind::UnterminatedString => "Unterminated string starting at",
            ErrorKind::ControlCharacter => "Invalid control character at",
            ErrorKind::BadEscape => "Invalid \\escape",
            ErrorKind::BadUnicodeEscape => "Invalid \\uXXXX escape",
            ErrorKind::UnterminatedComment => "Unterminated comment",
            ErrorKind::TooDeep => "Nesting too deep",
        }
    }
}

/// A parse error at byte `pos` of the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub pos: usize,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.kind.message(), self.pos)
    }
}

impl std::error::Error for Error {}

/// The grammar a [`Parser`] accepts.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Reject raw control characters in strings.
    pub strict: bool,
    /// Accept `NaN`, `Infinity` and `-Infinity` (Python's extension).
    pub constants: bool,
    /// JSONC: `//` and `/* */` comments, trailing commas and byte-order marks count as space.
    pub jsonc: bool,
    pub spelling: Spelling,
}

impl Options {
    pub const JSON: Options = Options { strict: true, constants: false, jsonc: false, spelling: Spelling::Plain };
    pub const JSONC: Options = Options { jsonc: true, ..Options::JSON };
}

/// A decoded string. `text` borrows the source when the literal has no escapes.
pub struct Str<'a> {
    pub text: Cow<'a, str>,
    /// The byte span of the literal as written, quotes included.
    pub start: usize,
    pub end: usize,
    /// In [`Spelling::Utf16`]: an escape produced a lone surrogate, which may pair with a
    /// neighbor and need canonicalizing.
    pub lone_surrogate: bool,
}

/// A number literal: the longest prefix at the cursor matching JSON's number grammar.
#[derive(Clone, Copy, Debug)]
pub struct Number<'a> {
    pub text: &'a str,
    /// It has a fraction or an exponent.
    pub is_float: bool,
    /// The value, when it is an integer of at most 15 digits (exact in an `f64`) other than -0.
    small: Option<i64>,
}

impl Number<'_> {
    /// The integer value, when it has at most 15 digits and is not -0.
    #[inline]
    pub fn small_int(&self) -> Option<i64> {
        self.small
    }

    #[inline]
    pub fn to_f64(&self) -> f64 {
        match self.small {
            Some(n) => n as f64,
            None => self.text.parse().unwrap_or(f64::NAN),
        }
    }
}

/// Python's named constants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Constant {
    NaN,
    Infinity,
    NegInfinity,
}

impl Constant {
    pub fn text(self) -> &'static str {
        match self {
            Constant::NaN => "NaN",
            Constant::Infinity => "Infinity",
            Constant::NegInfinity => "-Infinity",
        }
    }
}

/// Builds the values a [`Parser`] reads. `'a` is the source's lifetime.
pub trait Sink<'a> {
    type Value;
    type Key;
    type Object;
    type Array;
    type Error;

    fn error(&mut self, e: Error) -> Self::Error;
    /// Entering an object or an array (a depth check).
    fn enter(&mut self, array: bool) -> Result<(), Self::Error> {
        let _ = array;
        Ok(())
    }
    fn leave(&mut self) {}

    fn null(&mut self) -> Result<Self::Value, Self::Error>;
    fn bool(&mut self, b: bool) -> Result<Self::Value, Self::Error>;
    fn number(&mut self, n: Number<'a>) -> Result<Self::Value, Self::Error>;
    fn string(&mut self, s: Str<'a>) -> Result<Self::Value, Self::Error>;
    fn constant(&mut self, c: Constant) -> Result<Self::Value, Self::Error>;

    fn object(&mut self) -> Result<Self::Object, Self::Error>;
    fn key(&mut self, s: Str<'a>) -> Result<Self::Key, Self::Error>;
    fn member(&mut self, obj: &mut Self::Object, key: Self::Key, v: Self::Value) -> Result<(), Self::Error>;
    fn end_object(&mut self, obj: Self::Object) -> Result<Self::Value, Self::Error>;

    fn array(&mut self) -> Result<Self::Array, Self::Error>;
    fn element(&mut self, arr: &mut Self::Array, v: Self::Value) -> Result<(), Self::Error>;
    fn end_array(&mut self, arr: Self::Array) -> Result<Self::Value, Self::Error>;
}

/// A recursive-descent JSON reader over `src`, at byte `pos`.
pub struct Parser<'a> {
    src: &'a str,
    b: &'a [u8],
    pub pos: usize,
    opts: Options,
}

impl<'a> Parser<'a> {
    pub fn new(src: &'a str, opts: Options) -> Self {
        Parser { src, b: src.as_bytes(), pos: 0, opts }
    }

    pub fn src(&self) -> &'a str {
        self.src
    }

    #[inline]
    pub fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    /// Skip whitespace (and, in JSONC, comments and byte-order marks).
    #[inline]
    pub fn ws(&mut self) -> Result<(), Error> {
        while let Some(&c) = self.b.get(self.pos) {
            match c {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                b'/' | 0xEF if self.opts.jsonc => {
                    if !self.jsonc_space()? {
                        break;
                    }
                }
                _ => break,
            }
        }
        Ok(())
    }

    #[cold]
    fn jsonc_space(&mut self) -> Result<bool, Error> {
        let rest = &self.b[self.pos..];
        if rest.starts_with(b"\xEF\xBB\xBF") {
            self.pos += 3;
        } else if rest.starts_with(b"//") {
            self.pos += rest.iter().position(|&c| c == b'\n').unwrap_or(rest.len());
        } else if rest.starts_with(b"/*") {
            let close = rest[2..].windows(2).position(|w| w == b"*/");
            let close = close.ok_or(Error { kind: ErrorKind::UnterminatedComment, pos: self.pos })?;
            self.pos += close + 4;
        } else {
            return Ok(false);
        }
        Ok(true)
    }

    #[cold]
    fn fail<S: Sink<'a>>(&self, sink: &mut S, kind: ErrorKind, pos: usize) -> S::Error {
        sink.error(Error { kind, pos })
    }

    #[inline(always)]
    fn space<S: Sink<'a>>(&mut self, sink: &mut S) -> Result<(), S::Error> {
        match self.peek() {
            Some(b' ' | b'\t' | b'\n' | b'\r' | b'/' | 0xEF) => self.ws().map_err(|e| sink.error(e)),
            _ => Ok(()),
        }
    }

    /// One whole document: space, a value, space, and nothing after.
    pub fn document<S: Sink<'a>>(&mut self, sink: &mut S) -> Result<S::Value, S::Error> {
        self.space(sink)?;
        let v = self.value(sink)?;
        self.space(sink)?;
        if self.pos != self.b.len() {
            return Err(self.fail(sink, ErrorKind::ExtraData, self.pos));
        }
        Ok(v)
    }

    /// The value starting exactly at the cursor (no leading space), as CPython's `scan_once`.
    pub fn value<S: Sink<'a>>(&mut self, sink: &mut S) -> Result<S::Value, S::Error> {
        let Some(c) = self.peek() else {
            return Err(self.fail(sink, ErrorKind::ExpectingValue, self.pos));
        };
        match c {
            b'0'..=b'9' => match self.number() {
                Some(n) => sink.number(n),
                None => Err(self.fail(sink, ErrorKind::ExpectingValue, self.pos)),
            },
            b'"' => {
                self.pos += 1;
                let s = self.string_body().map_err(|e| sink.error(e))?;
                sink.string(s)
            }
            b'{' => {
                sink.enter(false)?;
                let r = self.object(sink);
                sink.leave();
                r
            }
            b'[' => {
                sink.enter(true)?;
                let r = self.array(sink);
                sink.leave();
                r
            }
            b'n' if self.word(b"null") => sink.null(),
            b't' if self.word(b"true") => sink.bool(true),
            b'f' if self.word(b"false") => sink.bool(false),
            b'N' if self.opts.constants && self.word(b"NaN") => sink.constant(Constant::NaN),
            b'I' if self.opts.constants && self.word(b"Infinity") => sink.constant(Constant::Infinity),
            b'-' if self.opts.constants && self.word(b"-Infinity") => sink.constant(Constant::NegInfinity),
            _ => match self.number() {
                Some(n) => sink.number(n),
                None => Err(self.fail(sink, ErrorKind::ExpectingValue, self.pos)),
            },
        }
    }

    #[inline]
    fn word(&mut self, w: &[u8]) -> bool {
        if self.b[self.pos..].starts_with(w) {
            self.pos += w.len();
            true
        } else {
            false
        }
    }

    fn object<S: Sink<'a>>(&mut self, sink: &mut S) -> Result<S::Value, S::Error> {
        self.pos += 1;
        let mut obj = sink.object()?;
        self.space(sink)?;
        if self.peek() != Some(b'}') {
            loop {
                if self.peek() != Some(b'"') {
                    return Err(self.fail(sink, ErrorKind::ExpectingKey, self.pos));
                }
                self.pos += 1;
                let k = self.string_body().map_err(|e| sink.error(e))?;
                let key = sink.key(k)?;
                self.space(sink)?;
                if self.peek() != Some(b':') {
                    return Err(self.fail(sink, ErrorKind::ExpectingColon, self.pos));
                }
                self.pos += 1;
                self.space(sink)?;
                let v = self.value(sink)?;
                sink.member(&mut obj, key, v)?;
                self.space(sink)?;
                match self.peek() {
                    Some(b'}') => break,
                    Some(b',') => {
                        self.pos += 1;
                        self.space(sink)?;
                        if self.opts.jsonc && self.peek() == Some(b'}') {
                            break;
                        }
                    }
                    _ => return Err(self.fail(sink, ErrorKind::ExpectingComma, self.pos)),
                }
            }
        }
        self.pos += 1;
        sink.end_object(obj)
    }

    fn array<S: Sink<'a>>(&mut self, sink: &mut S) -> Result<S::Value, S::Error> {
        self.pos += 1;
        let mut arr = sink.array()?;
        self.space(sink)?;
        if self.peek() != Some(b']') {
            loop {
                let v = self.value(sink)?;
                sink.element(&mut arr, v)?;
                self.space(sink)?;
                match self.peek() {
                    Some(b']') => break,
                    Some(b',') => {
                        self.pos += 1;
                        self.space(sink)?;
                        if self.opts.jsonc && self.peek() == Some(b']') {
                            break;
                        }
                    }
                    _ => return Err(self.fail(sink, ErrorKind::ExpectingComma, self.pos)),
                }
            }
        }
        self.pos += 1;
        sink.end_array(arr)
    }

    /// The number at the cursor, as CPython's `_match_number`: the longest prefix that is a JSON
    /// number (a fraction or exponent without digits is left unread).
    #[inline(always)]
    pub fn number(&mut self) -> Option<Number<'a>> {
        let b = self.b;
        let start = self.pos;
        let digit = |i: usize| b.get(i).is_some_and(u8::is_ascii_digit);
        let mut i = start;
        let neg = b.get(i) == Some(&b'-');
        if neg {
            i += 1;
        }
        let int_start = i;
        let mut n: i64 = 0;
        match b.get(i) {
            Some(b'0') => i += 1,
            Some(&c @ b'1'..=b'9') => {
                n = (c - b'0') as i64;
                i += 1;
                while let Some(&c) = b.get(i).filter(|c| c.is_ascii_digit()) {
                    n = n.wrapping_mul(10).wrapping_add((c - b'0') as i64);
                    i += 1;
                }
            }
            _ => return None,
        }
        let mut small = (i - int_start <= 15).then_some(if neg { -n } else { n });
        let mut is_float = false;
        if b.get(i) == Some(&b'.') && digit(i + 1) {
            is_float = true;
            i += 2;
            while digit(i) {
                i += 1;
            }
        }
        if matches!(b.get(i), Some(b'e' | b'E')) {
            let mut j = i + 1;
            if matches!(b.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if digit(j) {
                while digit(j) {
                    j += 1;
                }
                is_float = true;
                i = j;
            }
        }
        if is_float || neg && n == 0 {
            small = None;
        }
        self.pos = i;
        Some(Number { text: &self.src[start..i], is_float, small })
    }

    /// A string's contents from the cursor (just past its opening quote) through the closing
    /// quote, as CPython's `scanstring`.
    #[inline(always)]
    pub fn string_body(&mut self) -> Result<Str<'a>, Error> {
        let start = self.pos;
        let mut i = start;
        while let Some(&c) = self.b.get(i) {
            if c == b'"' {
                self.pos = i + 1;
                return Ok(Str { text: Cow::Borrowed(&self.src[start..i]), start: start.saturating_sub(1), end: i + 1, lone_surrogate: false });
            }
            if c == b'\\' || c < 0x20 {
                break;
            }
            i += 1;
        }
        self.string_escaped(start, i)
    }

    /// [`Self::string_body`] from byte `i`, the first escape or control character (or the end).
    #[inline(never)]
    fn string_escaped(&mut self, start: usize, mut i: usize) -> Result<Str<'a>, Error> {
        let b = self.b;
        let begin = start.saturating_sub(1);
        let err = |kind, pos| Error { kind, pos };
        let strict = self.opts.strict;
        let mut out = String::with_capacity(i - start + 16);
        out.push_str(&self.src[start..i]);
        let mut lone = false;
        loop {
            let run = i;
            loop {
                match b.get(i) {
                    None => return Err(err(ErrorKind::UnterminatedString, begin)),
                    Some(b'"' | b'\\') => break,
                    Some(&c) if c < 0x20 && strict => return Err(err(ErrorKind::ControlCharacter, i)),
                    Some(_) => i += 1,
                }
            }
            out.push_str(&self.src[run..i]);
            if b[i] == b'"' {
                i += 1;
                break;
            }
            let backslash = i;
            i += 1;
            let Some(&e) = b.get(i) else {
                return Err(err(ErrorKind::UnterminatedString, begin));
            };
            if e != b'u' {
                out.push(match e {
                    b'"' => '"',
                    b'\\' => '\\',
                    b'/' => '/',
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    _ => return Err(err(ErrorKind::BadEscape, backslash)),
                });
                i += 1;
                continue;
            }
            let u = i;
            i += 1;
            if i + 4 >= b.len() {
                return Err(err(ErrorKind::BadUnicodeEscape, u));
            }
            let mut cp = hex4(&b[i..i + 4]).ok_or(err(ErrorKind::BadUnicodeEscape, u))?;
            i += 4;
            if (0xD800..0xDC00).contains(&cp) && i + 6 < b.len() && b[i] == b'\\' && b[i + 1] == b'u' {
                let lo = hex4(&b[i + 2..i + 6]).ok_or(err(ErrorKind::BadUnicodeEscape, i + 1))?;
                if (0xDC00..0xE000).contains(&lo) {
                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                    i += 6;
                }
            }
            lone |= push_code_point(&mut out, cp, self.opts.spelling);
        }
        self.pos = i;
        Ok(Str { text: Cow::Owned(out), start: begin, end: i, lone_surrogate: lone })
    }
}

#[inline]
fn hex4(h: &[u8]) -> Option<u32> {
    h.iter().try_fold(0u32, |n, &c| Some(n * 16 + (c as char).to_digit(16)?))
}

/// Append `cp` in `spelling`; whether it was a lone surrogate.
fn push_code_point(out: &mut String, cp: u32, spelling: Spelling) -> bool {
    let lone = (0xD800..0xE000).contains(&cp);
    match spelling {
        Spelling::Plain => out.push(char::from_u32(cp).unwrap_or('\u{FFFD}')),
        Spelling::CodePoints => {
            smuggle::push_code_point(out, cp);
        }
        Spelling::Utf16 => match char::from_u32(cp) {
            Some(c) => smuggle::push_char_utf16(out, c),
            None => out.push(smuggle::smuggle(cp as u16)),
        },
    }
    lone
}

// ---- a plain value tree -------------------------------------------------------------------------

/// A parsed JSON document, objects keeping their members in source order.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

impl Value {
    /// Member `key` of an object (the last one, as `JSON.parse` keeps).
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(items) => items.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Parse a JSON document.
pub fn parse(text: &str) -> Result<Value, Error> {
    Parser::new(text, Options::JSON).document(&mut TreeSink { depth: 0 })
}

/// Parse JSONC: JSON plus comments, trailing commas and byte-order marks.
pub fn parse_jsonc(text: &str) -> Result<Value, Error> {
    Parser::new(text, Options::JSONC).document(&mut TreeSink { depth: 0 })
}

const TREE_DEPTH: usize = 512;

struct TreeSink {
    depth: usize,
}

impl<'a> Sink<'a> for TreeSink {
    type Value = Value;
    type Key = String;
    type Object = Vec<(String, Value)>;
    type Array = Vec<Value>;
    type Error = Error;

    fn error(&mut self, e: Error) -> Error {
        e
    }
    fn enter(&mut self, _array: bool) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > TREE_DEPTH {
            return Err(Error { kind: ErrorKind::TooDeep, pos: 0 });
        }
        Ok(())
    }
    fn leave(&mut self) {
        self.depth -= 1;
    }
    fn null(&mut self) -> Result<Value, Error> {
        Ok(Value::Null)
    }
    fn bool(&mut self, b: bool) -> Result<Value, Error> {
        Ok(Value::Bool(b))
    }
    fn number(&mut self, n: Number<'a>) -> Result<Value, Error> {
        Ok(Value::Num(n.to_f64()))
    }
    fn string(&mut self, s: Str<'a>) -> Result<Value, Error> {
        Ok(Value::Str(s.text.into_owned()))
    }
    fn constant(&mut self, c: Constant) -> Result<Value, Error> {
        Ok(Value::Num(match c {
            Constant::NaN => f64::NAN,
            Constant::Infinity => f64::INFINITY,
            Constant::NegInfinity => f64::NEG_INFINITY,
        }))
    }
    fn object(&mut self) -> Result<Self::Object, Error> {
        Ok(Vec::new())
    }
    fn key(&mut self, s: Str<'a>) -> Result<String, Error> {
        Ok(s.text.into_owned())
    }
    fn member(&mut self, obj: &mut Self::Object, key: String, v: Value) -> Result<(), Error> {
        obj.push((key, v));
        Ok(())
    }
    fn end_object(&mut self, obj: Self::Object) -> Result<Value, Error> {
        Ok(Value::Obj(obj))
    }
    fn array(&mut self) -> Result<Vec<Value>, Error> {
        Ok(Vec::new())
    }
    fn element(&mut self, arr: &mut Vec<Value>, v: Value) -> Result<(), Error> {
        arr.push(v);
        Ok(())
    }
    fn end_array(&mut self, arr: Vec<Value>) -> Result<Value, Error> {
        Ok(Value::Arr(arr))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(text: &str) -> (ErrorKind, usize) {
        let e = parse(text).unwrap_err();
        (e.kind, e.pos)
    }

    #[test]
    fn parses_documents() {
        let v = parse(r#" {"a": [1, -2.5e1, true, false, null], "b": "x\u00e9\ud83d\ude00", "a": 0} "#).unwrap();
        assert_eq!(v.get("a"), Some(&Value::Num(0.0)));
        assert_eq!(v.get("b").and_then(Value::as_str), Some("xé😀"));
        let Value::Obj(items) = &v else { panic!() };
        assert_eq!(items[0].1, Value::Arr(vec![Value::Num(1.0), Value::Num(-25.0), Value::Bool(true), Value::Bool(false), Value::Null]));
    }

    #[test]
    fn errors_match_cpython_positions() {
        assert_eq!(err(""), (ErrorKind::ExpectingValue, 0));
        assert_eq!(err("[42"), (ErrorKind::ExpectingComma, 3));
        assert_eq!(err("[\"spam"), (ErrorKind::UnterminatedString, 1));
        assert_eq!(err("{\"spam\""), (ErrorKind::ExpectingColon, 7));
        assert_eq!(err("{\"spam\":42,}"), (ErrorKind::ExpectingKey, 11));
        assert_eq!(err("[42,]"), (ErrorKind::ExpectingValue, 4));
        assert_eq!(err("[]]"), (ErrorKind::ExtraData, 2));
        assert_eq!(err("\"a\\x\""), (ErrorKind::BadEscape, 2));
        assert_eq!(err("\"\\u12\""), (ErrorKind::BadUnicodeEscape, 2));
        assert_eq!(err("\"\\ud834\\u0x20\""), (ErrorKind::BadUnicodeEscape, 8));
        assert_eq!(err("\"a\nb\""), (ErrorKind::ControlCharacter, 2));
        for bad in ["01", "1.", "1e", "-", ".5", "+1", "NaN", "tru", "[1,]", "{\"a\":1,}"] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn numbers_take_the_longest_valid_prefix() {
        let mut p = Parser::new("1.e5", Options::JSON);
        let n = p.number().unwrap();
        assert_eq!((n.text, n.is_float, p.pos), ("1", false, 1));
        let mut p = Parser::new("-0.5E+3x", Options::JSON);
        let n = p.number().unwrap();
        assert_eq!((n.text, n.is_float, n.to_f64()), ("-0.5E+3", true, -500.0));
        let n = Parser::new("-0", Options::JSON).number().unwrap();
        assert_eq!(n.to_f64().to_bits(), (-0.0f64).to_bits());
        let n = Parser::new("-123456789012345 ", Options::JSON).number().unwrap();
        assert_eq!((n.small_int(), n.to_f64()), (Some(-123456789012345), -123456789012345.0));
        let n = Parser::new("1234567890123456", Options::JSON).number().unwrap();
        assert_eq!((n.small_int(), n.to_f64()), (None, 1234567890123456.0));
    }

    #[test]
    fn jsonc_allows_comments_trailing_commas_and_bom() {
        let v = parse_jsonc("\u{feff}// head\n{ /* c */ \"a\": [1, 2,], }").unwrap();
        assert_eq!(v.get("a"), Some(&Value::Arr(vec![Value::Num(1.0), Value::Num(2.0)])));
        assert_eq!(parse_jsonc("{} /* open").unwrap_err().kind, ErrorKind::UnterminatedComment);
    }

    #[test]
    fn lone_surrogates_follow_the_spelling() {
        let parse_str = |text: &str, spelling| {
            let mut p = Parser::new(text, Options { spelling, ..Options::JSON });
            p.pos = 1;
            let s = p.string_body().unwrap();
            (s.text.into_owned(), s.lone_surrogate)
        };
        assert_eq!(parse_str("\"\\ud800\"", Spelling::Plain), ("\u{FFFD}".into(), true));
        assert_eq!(parse_str("\"\\ud800\"", Spelling::Utf16).0, smuggle::smuggle(0xD800).to_string());
        let mut cp = String::new();
        smuggle::push_code_point(&mut cp, 0xDC00);
        assert_eq!(parse_str("\"\\udc00\"", Spelling::CodePoints).0, cp);
    }

    #[test]
    fn quotes() {
        assert_eq!(json_string("a\"\\\n\u{1}é\u{2028}"), "\"a\\\"\\\\\\n\\u0001é\u{2028}\"");
        assert_eq!(quote("\u{2028}", &Quote::JS_SOURCE), "\"\\u2028\"");
        let ascii = Quote { ascii_only: true, ..Quote::JSON };
        assert_eq!(quote("é😀\u{7f}", &ascii), "\"\\u00e9\\ud83d\\ude00\\u007f\"");
        let inspect = Quote { quote: b'\'', escapes: Escapes::Inspect, ..Quote::JSON };
        assert_eq!(quote("it's \"q\"\u{1b}\u{85}", &inspect), "'it\\'s \"q\"\\x1B\\x85'");
        let mut lone = String::from("a");
        lone.push(smuggle::smuggle(0xD800));
        let js = Quote { lone_surrogates: true, spelling: Spelling::Utf16, ..Quote::JSON };
        assert_eq!(quote(&lone, &js), "\"a\\ud800\"");
        let mut pair = String::new();
        smuggle::push_char_utf16(&mut pair, '\u{10FFFF}');
        assert_eq!(quote(&pair, &js), format!("\"{pair}\""));
        assert_eq!(quote(&pair, &Quote { ascii_only: true, ..js }), "\"\\udbff\\udfff\"");
    }
}
