//! Token kinds produced by the [`crate::lexer`].

use std::rc::Rc;

/// One lexical token plus the source bookkeeping the parser needs: the 1-based line (for error
/// messages) and whether a line terminator appeared before this token (for Automatic Semicolon
/// Insertion and the handful of "[no LineTerminator here]" grammar rules).
#[derive(Debug, Clone)]
pub struct Token {
    pub kind: Tok,
    pub line: u32,
    /// Byte offsets of this token in the source (for function `toString` source slices).
    pub start: u32,
    pub end: u32,
    pub nl_before: bool,
    /// A legacy-octal number (`010`) or a string with a legacy octal/`\8`/`\9` escape — a
    /// SyntaxError in strict mode.
    pub legacy_octal: bool,
    /// The identifier contained a `\u` escape — so it can't be recognized as a contextual keyword
    /// (`async`/`get`/`set`/`of`/`static`/…).
    pub escaped: bool,
    /// A string literal that contains a lone (unpaired) surrogate code point — well-formed enough to
    /// be a JS string, but not a valid ModuleExportName.
    pub lone_surrogate: bool,
}

/// Every payload is at most two words, so a token vector for a multi-megabyte bundle stays at
/// 40 bytes per token.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Num(f64),
    /// A BigInt literal (`123n`).
    BigInt(crate::bigint::JsBigInt),
    Str(Rc<str>),
    /// A template literal, split into cooked string chunks and `${...}` substitution tokens.
    /// `` `a${x}b` `` lexes to `[Str("a"), Sub([x, Eof]), Str("b")]`. The parser desugars it to a
    /// string concatenation, sub-parsing each `Sub`.
    Template(Box<Vec<TplPart>>),
    /// Native JSX syntax, with expression containers tokenized in the enclosing source.
    Jsx(Rc<crate::ast::JsxElement>),
    /// An identifier, interned per lexed source: every occurrence of a name shares one string.
    Ident(Name),
    /// A reserved word. The text is interned to a `&'static str` so the parser can match by value.
    Keyword(&'static str),
    /// A punctuator. Interned to `&'static str` (e.g. `"=>"`, `"==="`, `"+="`).
    Punct(&'static str),
    /// A regular-expression literal: `/body/flags`.
    Regex(Box<RegexTok>),
    Eof,
}

/// An identifier's text (see `Tok::Ident`): a shared string that compares with `str`.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Name(pub Rc<str>);

impl Name {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl std::ops::Deref for Name {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}
impl std::borrow::Borrow<str> for Name {
    fn borrow(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Debug for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&*self.0, f)
    }
}
impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}
impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}
impl PartialEq<String> for Name {
    fn eq(&self, other: &String) -> bool {
        *self.0 == **other
    }
}
impl From<Name> for String {
    fn from(n: Name) -> String {
        n.0.to_string()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegexTok {
    pub body: String,
    pub flags: String,
}

/// One piece of a template literal: a literal chunk (with both the cooked value and the raw source,
/// the latter needed for tagged templates' `strings.raw`) or the tokens of a `${...}` hole.
#[derive(Debug, Clone, PartialEq)]
pub enum TplPart {
    /// A literal chunk. `cooked` is None when the chunk contains an invalid escape sequence —
    /// legal only in a *tagged* template (the cooked value is undefined there).
    Str { cooked: Option<String>, raw: String },
    /// The hole's tokens, lexed in place (offsets and lines in the enclosing source's
    /// coordinates), ending with an `Eof` at the closing `}`. Shared so a nested template is
    /// never copied per nesting level.
    Sub(SubToks),
}

/// See [`TplPart::Sub`]. Compares by identity.
#[derive(Debug, Clone)]
pub struct SubToks(pub Rc<TokVec>);

impl PartialEq for SubToks {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

/// The *always-reserved* words. The lexer hands these back as `Keyword` tokens so `var`/`function`/
/// etc. can never be plain identifiers and reserved-word misuse surfaces as a SyntaxError.
///
/// Contextual keywords (`let`, `const` is reserved but `of`/`async`/`get`/`set`/`static`/`yield`/
/// `await`/`as`/`from`) are deliberately NOT here — they are valid identifiers in many positions,
/// so they stay `Ident` and the parser recognises them by text where the grammar calls for them.
pub const KEYWORDS: &[&str] = &[
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
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "import",
    "in",
    "instanceof",
    "new",
    "null",
    "return",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
];

/// Multi-char punctuators, longest first so the lexer is maximal-munch.
pub const PUNCTUATORS: &[&str] = &[
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=",
    "<=", ">=", "&&", "||", "??", "?.", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=",
    "**", "<<", ">>", "{", "}", "(", ")", "[", "]", ".", ";", ",", "<", ">", "+", "-", "*", "/",
    "%", "&", "|", "^", "!", "~", "?", ":", "=", "@",
];

/// Tokens per [`TokVec`] chunk (a power of two).
const CHUNK_BITS: u32 = 14;
const CHUNK: usize = 1 << CHUNK_BITS;

/// A token sequence stored in fixed-size chunks. A whole-file token list for a large bundle
/// grows chunk by chunk instead of doubling (and being copied) like a `Vec`, so it never exists
/// twice over.
#[derive(Debug, Clone, Default)]
pub struct TokVec {
    /// Every chunk but the last holds exactly [`CHUNK`] tokens.
    chunks: Vec<Vec<Token>>,
    len: usize,
}

impl TokVec {
    /// Room for `n` tokens in the first chunk (a hint for small sources).
    pub fn with_capacity(n: usize) -> Self {
        TokVec {
            chunks: vec![Vec::with_capacity(n.min(CHUNK))],
            len: 0,
        }
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    #[inline]
    pub fn get(&self, i: usize) -> Option<&Token> {
        if i < self.len {
            // SAFETY: every chunk before the last is full and `len` counts all of them, so
            // index `i < len` lies inside chunk `i / CHUNK` at `i % CHUNK`.
            Some(unsafe {
                self.chunks
                    .get_unchecked(i >> CHUNK_BITS)
                    .get_unchecked(i & (CHUNK - 1))
            })
        } else {
            None
        }
    }
    pub fn last_mut(&mut self) -> Option<&mut Token> {
        let i = self.len.checked_sub(1)?;
        Some(&mut self[i])
    }
    pub fn push(&mut self, t: Token) {
        match self.chunks.last_mut() {
            Some(c) if c.len() < CHUNK => {
                if c.len() == c.capacity() {
                    let n = c.len().max(16).min(CHUNK - c.len());
                    c.reserve_exact(n);
                }
                c.push(t);
            }
            _ => {
                let mut c = Vec::with_capacity(if self.len == 0 { 16 } else { CHUNK });
                c.push(t);
                self.chunks.push(c);
            }
        }
        self.len += 1;
    }
    pub fn pop(&mut self) -> Option<Token> {
        let c = self.chunks.last_mut()?;
        let t = c.pop()?;
        if c.is_empty() && self.chunks.len() > 1 {
            self.chunks.pop();
        }
        self.len -= 1;
        Some(t)
    }
    pub fn truncate(&mut self, n: usize) {
        while self.len > n {
            let c = self.chunks.last_mut().expect("non-empty");
            let keep = c.len().saturating_sub(self.len - n);
            self.len -= c.len() - keep;
            c.truncate(keep);
            if c.is_empty() && self.chunks.len() > 1 {
                self.chunks.pop();
            }
        }
    }
    pub fn extend(&mut self, toks: impl IntoIterator<Item = Token>) {
        for t in toks {
            self.push(t);
        }
    }
    /// Replace `range` with `toks` (shifting the tail, like `Vec::splice`).
    pub fn splice(&mut self, range: std::ops::Range<usize>, toks: impl IntoIterator<Item = Token>) {
        let mut tail = Vec::with_capacity(self.len - range.end);
        while self.len > range.end {
            tail.push(self.pop().expect("non-empty"));
        }
        self.truncate(range.start);
        self.extend(toks);
        self.extend(tail.into_iter().rev());
    }
    pub fn remove(&mut self, i: usize) {
        self.splice(i..i + 1, None);
    }
    /// The tokens from index `from` on.
    pub fn iter_from(&self, from: usize) -> impl Iterator<Item = &Token> {
        let skip = from & (CHUNK - 1);
        self.chunks
            .iter()
            .skip(from >> CHUNK_BITS)
            .flatten()
            .skip(skip)
    }
    pub fn into_vec(self) -> Vec<Token> {
        let mut v = Vec::with_capacity(self.len);
        for c in self.chunks {
            v.extend(c);
        }
        v
    }
}

impl std::ops::Index<usize> for TokVec {
    type Output = Token;
    #[inline]
    fn index(&self, i: usize) -> &Token {
        self.get(i).expect("token index in range")
    }
}
impl std::ops::IndexMut<usize> for TokVec {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut Token {
        assert!(i < self.len, "token index in range");
        &mut self.chunks[i >> CHUNK_BITS][i & (CHUNK - 1)]
    }
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<Token>() <= 40);

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(n: usize) -> Token {
        Token {
            kind: Tok::Num(n as f64),
            line: 1,
            start: n as u32,
            end: n as u32,
            nl_before: false,
            legacy_octal: false,
            escaped: false,
            lone_surrogate: false,
        }
    }
    fn starts(v: &TokVec) -> Vec<u32> {
        v.iter_from(0).map(|t| t.start).collect()
    }

    #[test]
    fn tokvec_edits_across_chunks() {
        let n = 3 * CHUNK + 5;
        let mut v = TokVec::with_capacity(4);
        v.extend((0..n).map(tok));
        let mut want: Vec<u32> = (0..n as u32).collect();
        assert_eq!(starts(&v), want);
        assert_eq!(v[CHUNK + 7].start, (CHUNK + 7) as u32);
        assert_eq!(
            v.iter_from(2 * CHUNK - 1).next().map(|t| t.start),
            Some(2 * CHUNK as u32 - 1)
        );
        v.splice(CHUNK - 2..CHUNK + 3, [tok(9000), tok(9001)]);
        want.splice(CHUNK - 2..CHUNK + 3, [9000, 9001]);
        assert_eq!(starts(&v), want);
        v.remove(5);
        want.remove(5);
        v.truncate(2 * CHUNK);
        want.truncate(2 * CHUNK);
        assert_eq!(starts(&v), want);
        assert_eq!(v.len(), want.len());
        v[CHUNK].start = 7;
        assert_eq!(v.get(CHUNK).map(|t| t.start), Some(7));
        assert!(v.get(v.len()).is_none());
    }
}
