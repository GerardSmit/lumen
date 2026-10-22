//! Token kinds produced by the [`crate::lexer`].

use std::fmt;
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::rc::Rc;

const LINE_BITS: u32 = 28;
const LINE_MASK: u32 = (1 << LINE_BITS) - 1;
const NL_BEFORE: u32 = 1 << 31;
const LEGACY_OCTAL: u32 = 1 << 30;
const ESCAPED: u32 = 1 << 29;
const LONE_SURROGATE: u32 = 1 << 28;

/// One lexical token plus the source bookkeeping the parser needs: the 1-based line (for error
/// messages) and whether a line terminator appeared before this token (for Automatic Semicolon
/// Insertion and the handful of "[no LineTerminator here]" grammar rules).
#[derive(Debug, Clone)]
pub struct Token {
    pub kind: Tok,
    /// The line in the low [`LINE_BITS`] bits and the four flags in the top bits.
    line_flags: u32,
    /// Byte offsets of this token in the source (for function `toString` source slices).
    pub start: u32,
    pub end: u32,
}

impl Token {
    pub fn new(kind: Tok, line: u32, start: u32, end: u32, nl_before: bool) -> Token {
        Token {
            kind,
            line_flags: line.min(LINE_MASK) | if nl_before { NL_BEFORE } else { 0 },
            start,
            end,
        }
    }
    #[inline]
    pub fn line(&self) -> u32 {
        self.line_flags & LINE_MASK
    }
    pub fn set_line(&mut self, line: u32) {
        self.line_flags = (self.line_flags & !LINE_MASK) | line.min(LINE_MASK);
    }
    #[inline]
    fn flag(&self, bit: u32) -> bool {
        self.line_flags & bit != 0
    }
    fn set_flag(&mut self, bit: u32, on: bool) {
        if on {
            self.line_flags |= bit;
        } else {
            self.line_flags &= !bit;
        }
    }
    /// A line terminator precedes this token.
    #[inline]
    pub fn nl_before(&self) -> bool {
        self.flag(NL_BEFORE)
    }
    pub fn set_nl_before(&mut self, on: bool) {
        self.set_flag(NL_BEFORE, on);
    }
    /// A legacy-octal number (`010`) or a string with a legacy octal/`\8`/`\9` escape — a
    /// SyntaxError in strict mode.
    #[inline]
    pub fn legacy_octal(&self) -> bool {
        self.flag(LEGACY_OCTAL)
    }
    pub fn set_legacy_octal(&mut self, on: bool) {
        self.set_flag(LEGACY_OCTAL, on);
    }
    /// The identifier contained a `\u` escape — so it can't be recognized as a contextual keyword
    /// (`async`/`get`/`set`/`of`/`static`/…).
    #[inline]
    pub fn escaped(&self) -> bool {
        self.flag(ESCAPED)
    }
    pub fn set_escaped(&mut self, on: bool) {
        self.set_flag(ESCAPED, on);
    }
    /// A string literal that contains a lone (unpaired) surrogate code point — well-formed enough to
    /// be a JS string, but not a valid ModuleExportName.
    #[inline]
    pub fn lone_surrogate(&self) -> bool {
        self.flag(LONE_SURROGATE)
    }
    pub fn set_lone_surrogate(&mut self, on: bool) {
        self.set_flag(LONE_SURROGATE, on);
    }
}

/// A single-word owning pointer (`Box<T>`, `Rc<T>`, ...) stored as two `u32`s so that it has
/// alignment 4: a [`Tok`] holding one is 12 bytes instead of 16.
pub struct Packed<P> {
    bits: [u32; 2],
    owner: PhantomData<P>,
}

impl<P> Packed<P> {
    pub fn new(p: P) -> Self {
        const { assert!(size_of::<P>() <= size_of::<[u32; 2]>()) };
        let mut bits = [0u32; 2];
        // SAFETY: `P` fits in the eight bytes (asserted above; a pointer is four bytes on 32-bit
        // targets) and is moved in, so the bits carry ownership into `Packed`.
        unsafe { std::ptr::write_unaligned(bits.as_mut_ptr().cast::<P>(), p) };
        Packed {
            bits,
            owner: PhantomData,
        }
    }
    /// A bitwise copy of the pointer that must not be dropped.
    fn load(&self) -> ManuallyDrop<P> {
        // SAFETY: `bits` holds a valid `P` written by `new`; `read_unaligned` copes with the
        // 4-byte alignment and `ManuallyDrop` keeps the copy from releasing the pointee.
        ManuallyDrop::new(unsafe { std::ptr::read_unaligned(self.bits.as_ptr().cast::<P>()) })
    }
    pub fn into_inner(self) -> P {
        let this = ManuallyDrop::new(self);
        ManuallyDrop::into_inner(this.load())
    }
}

impl<P> Drop for Packed<P> {
    fn drop(&mut self) {
        drop(ManuallyDrop::into_inner(self.load()));
    }
}

impl<P: Clone> Clone for Packed<P> {
    fn clone(&self) -> Self {
        Packed::new(P::clone(&self.load()))
    }
}

impl<P: Deref> Deref for Packed<P> {
    type Target = P::Target;
    fn deref(&self) -> &P::Target {
        let p = self.load();
        let target: *const P::Target = &**p;
        // SAFETY: the pointee lives on the heap for as long as `self` owns the pointer, so the
        // reference outlives the local copy it was derived from.
        unsafe { &*target }
    }
}

impl<P: Deref> fmt::Debug for Packed<P>
where
    P::Target: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<P: Deref> PartialEq for Packed<P>
where
    P::Target: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

/// A number literal's value, stored as two `u32`s (see [`Packed`]).
#[derive(Clone, Copy)]
pub struct Num([u32; 2]);

impl Num {
    pub fn new(n: f64) -> Num {
        let bits = n.to_bits();
        Num([bits as u32, (bits >> 32) as u32])
    }
    #[inline]
    pub fn get(self) -> f64 {
        f64::from_bits(u64::from(self.0[0]) | (u64::from(self.0[1]) << 32))
    }
}
impl fmt::Debug for Num {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.get(), f)
    }
}
impl PartialEq for Num {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

/// A payload is at most one pointer, so a token vector for a multi-megabyte bundle stays at
/// 24 bytes per token.
#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Num(Num),
    /// A BigInt literal (`123n`).
    BigInt(Packed<Box<crate::bigint::JsBigInt>>),
    /// A string literal's cooked value.
    Str(Name),
    /// A template literal, split into cooked string chunks and `${...}` substitution tokens.
    /// `` `a${x}b` `` lexes to `[Str("a"), Sub([x, Eof]), Str("b")]`. The parser desugars it to a
    /// string concatenation, sub-parsing each `Sub`.
    Template(Packed<Box<Vec<TplPart>>>),
    /// Native JSX syntax, with expression containers tokenized in the enclosing source.
    Jsx(Packed<Rc<crate::ast::JsxElement>>),
    /// An identifier, interned per lexed source: every occurrence of a name shares one string.
    Ident(Name),
    /// A reserved word.
    Keyword(Keyword),
    /// A punctuator.
    Punct(Punct),
    /// A regular-expression literal: `/body/flags`.
    Regex(Packed<Box<RegexTok>>),
    /// Stands for the tokens between the `{` and `}` of a `function` body that was lexed but not
    /// kept (see `LexOpts::skim_min_depth`): it spans both braces, and always sits between them.
    Skim,
    Eof,
}

/// A shared, immutable string behind a single (4-aligned) pointer. Identifiers are interned per
/// lexed source, so every occurrence of a name shares one allocation.
#[derive(Clone, PartialEq)]
pub struct Name(Packed<Rc<Box<str>>>);

impl Name {
    pub fn new(s: &str) -> Name {
        Name(Packed::new(Rc::new(Box::<str>::from(s))))
    }
    pub fn as_str(&self) -> &str {
        let boxed: &Box<str> = &self.0;
        boxed
    }
}
impl Eq for Name {}
impl std::hash::Hash for Name {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}
impl Deref for Name {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}
impl std::borrow::Borrow<str> for Name {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}
impl fmt::Debug for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}
impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl PartialEq<str> for Name {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}
impl PartialEq<&str> for Name {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}
impl PartialEq<String> for Name {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}
impl From<Name> for String {
    fn from(n: Name) -> String {
        n.as_str().to_string()
    }
}
impl From<&Name> for Rc<str> {
    fn from(n: &Name) -> Rc<str> {
        Rc::from(n.as_str())
    }
}

/// A punctuator (see [`PUNCTUATORS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Punct {
    UShrAssign,
    Ellipsis,
    StrictEq,
    StrictNe,
    PowAssign,
    ShlAssign,
    ShrAssign,
    UShr,
    AndAssign,
    OrAssign,
    NullishAssign,
    Arrow,
    EqEq,
    NotEq,
    LtEq,
    GtEq,
    AndAnd,
    OrOr,
    Nullish,
    OptChain,
    Inc,
    Dec,
    AddAssign,
    SubAssign,
    MulAssign,
    DivAssign,
    RemAssign,
    BitAndAssign,
    BitOrAssign,
    BitXorAssign,
    Pow,
    Shl,
    Shr,
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Dot,
    Semi,
    Comma,
    Lt,
    Gt,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    Pipe,
    Caret,
    Bang,
    Tilde,
    Question,
    Colon,
    Assign,
    At,
}

impl Punct {
    pub const fn as_str(self) -> &'static str {
        match self {
            Punct::UShrAssign => ">>>=",
            Punct::Ellipsis => "...",
            Punct::StrictEq => "===",
            Punct::StrictNe => "!==",
            Punct::PowAssign => "**=",
            Punct::ShlAssign => "<<=",
            Punct::ShrAssign => ">>=",
            Punct::UShr => ">>>",
            Punct::AndAssign => "&&=",
            Punct::OrAssign => "||=",
            Punct::NullishAssign => "??=",
            Punct::Arrow => "=>",
            Punct::EqEq => "==",
            Punct::NotEq => "!=",
            Punct::LtEq => "<=",
            Punct::GtEq => ">=",
            Punct::AndAnd => "&&",
            Punct::OrOr => "||",
            Punct::Nullish => "??",
            Punct::OptChain => "?.",
            Punct::Inc => "++",
            Punct::Dec => "--",
            Punct::AddAssign => "+=",
            Punct::SubAssign => "-=",
            Punct::MulAssign => "*=",
            Punct::DivAssign => "/=",
            Punct::RemAssign => "%=",
            Punct::BitAndAssign => "&=",
            Punct::BitOrAssign => "|=",
            Punct::BitXorAssign => "^=",
            Punct::Pow => "**",
            Punct::Shl => "<<",
            Punct::Shr => ">>",
            Punct::LBrace => "{",
            Punct::RBrace => "}",
            Punct::LParen => "(",
            Punct::RParen => ")",
            Punct::LBracket => "[",
            Punct::RBracket => "]",
            Punct::Dot => ".",
            Punct::Semi => ";",
            Punct::Comma => ",",
            Punct::Lt => "<",
            Punct::Gt => ">",
            Punct::Plus => "+",
            Punct::Minus => "-",
            Punct::Star => "*",
            Punct::Slash => "/",
            Punct::Percent => "%",
            Punct::Amp => "&",
            Punct::Pipe => "|",
            Punct::Caret => "^",
            Punct::Bang => "!",
            Punct::Tilde => "~",
            Punct::Question => "?",
            Punct::Colon => ":",
            Punct::Assign => "=",
            Punct::At => "@",
        }
    }
}

impl fmt::Display for Punct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A reserved word (see [`KEYWORDS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Keyword {
    Break,
    Case,
    Catch,
    Class,
    Const,
    Continue,
    Debugger,
    Default,
    Delete,
    Do,
    Else,
    Enum,
    Export,
    Extends,
    False,
    Finally,
    For,
    Function,
    If,
    Import,
    In,
    Instanceof,
    New,
    Null,
    Return,
    Super,
    Switch,
    This,
    Throw,
    True,
    Try,
    Typeof,
    Var,
    Void,
    While,
    With,
}

impl Keyword {
    pub const fn as_str(self) -> &'static str {
        match self {
            Keyword::Break => "break",
            Keyword::Case => "case",
            Keyword::Catch => "catch",
            Keyword::Class => "class",
            Keyword::Const => "const",
            Keyword::Continue => "continue",
            Keyword::Debugger => "debugger",
            Keyword::Default => "default",
            Keyword::Delete => "delete",
            Keyword::Do => "do",
            Keyword::Else => "else",
            Keyword::Enum => "enum",
            Keyword::Export => "export",
            Keyword::Extends => "extends",
            Keyword::False => "false",
            Keyword::Finally => "finally",
            Keyword::For => "for",
            Keyword::Function => "function",
            Keyword::If => "if",
            Keyword::Import => "import",
            Keyword::In => "in",
            Keyword::Instanceof => "instanceof",
            Keyword::New => "new",
            Keyword::Null => "null",
            Keyword::Return => "return",
            Keyword::Super => "super",
            Keyword::Switch => "switch",
            Keyword::This => "this",
            Keyword::Throw => "throw",
            Keyword::True => "true",
            Keyword::Try => "try",
            Keyword::Typeof => "typeof",
            Keyword::Var => "var",
            Keyword::Void => "void",
            Keyword::While => "while",
            Keyword::With => "with",
        }
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
    /// An empty list whose first chunk is the (cleared) allocation of an earlier list, see
    /// [`TokVec::into_first_chunk`].
    pub fn from_chunk(chunk: Vec<Token>) -> Self {
        debug_assert!(chunk.is_empty());
        TokVec {
            chunks: vec![chunk],
            len: 0,
        }
    }
    /// The first chunk, emptied but keeping its capacity, for [`TokVec::from_chunk`].
    pub fn into_first_chunk(self) -> Option<Vec<Token>> {
        let mut chunk = self.chunks.into_iter().next()?;
        chunk.clear();
        Some(chunk)
    }
    /// The chunks in order, as slices.
    pub fn chunk_slices(&self) -> impl Iterator<Item = &[Token]> {
        self.chunks.iter().map(Vec::as_slice)
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
const _: () = assert!(std::mem::size_of::<Token>() <= 24);

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(n: usize) -> Token {
        Token::new(Tok::Num(Num::new(n as f64)), 1, n as u32, n as u32, false)
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
