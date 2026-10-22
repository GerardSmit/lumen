//! Hand-written tokenizer over the UTF-8 source (token offsets are bytes). Produces the full
//! token list up front and resolves the classic `/`-is-it-a-regex-or-division ambiguity by
//! tracking whether the previously emitted token can end an expression.

use crate::token::{
    Keyword, Name, Num, Packed, Punct, RegexTok, SubToks, Tok, TokVec, Token, TplPart,
};
use std::rc::Rc;

mod jsx;

#[derive(Debug, Clone)]
pub struct LexError {
    pub message: String,
    pub line: u32,
    /// The lexer ran out of input mid-construct (unterminated template/comment/...). A REPL uses
    /// this to keep reading lines instead of reporting a SyntaxError.
    pub at_eof: bool,
}

/// Tokens of a skimmed body kept before its middle is dropped as the lexer goes, and the
/// trailing tokens always kept for the lexer's look-behind.
const SKIM_WINDOW: usize = 512;
const SKIM_TAIL: usize = 48;

#[cfg(test)]
thread_local! {
    /// The most tokens any lexer on this thread held at once.
    pub(crate) static PEAK_TOKENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The bodies [`Lexer::skip_body`] jumped over on this thread.
    pub(crate) static SKIPPED_BODIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The bodies [`Lexer::skip_body`] jumped over on this thread by a recorded hint.
    pub(crate) static HINTED_BODIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The body being skimmed: where its `{` sits in the output and the source.
#[derive(Clone, Copy)]
struct SkimMark {
    brace_depth: usize,
    out_len: usize,
    line: u32,
    start: u32,
}

/// What the previous token says about a following `/`, `{` or `function`.
#[derive(Clone, Copy, PartialEq)]
enum Prev {
    /// No token yet.
    Start,
    /// A literal, or the `Eof`/`Skim` stand-ins: an operand has ended.
    Operand,
    Ident,
    Await,
    Yield,
    Kw(Keyword),
    Punct(Punct),
}

impl Prev {
    fn of(kind: &Tok) -> Prev {
        match kind {
            Tok::Num(_)
            | Tok::BigInt(_)
            | Tok::Str(_)
            | Tok::Template(_)
            | Tok::Jsx(_)
            | Tok::Regex(_)
            | Tok::Skim
            | Tok::Eof => Prev::Operand,
            Tok::Ident(w) => Prev::of_ident(w),
            Tok::Keyword(k) => Prev::Kw(*k),
            Tok::Punct(p) => Prev::Punct(*p),
        }
    }

    fn of_ident(word: &str) -> Prev {
        match word {
            "await" => Prev::Await,
            "yield" => Prev::Yield,
            _ => Prev::Ident,
        }
    }

    /// Whether a `/` after this token begins a regex literal (we are at the start of an
    /// expression) rather than a division.
    fn regex_allowed(self, start_div: bool, br: &Brackets) -> bool {
        match self {
            Prev::Start => !start_div,
            Prev::Operand | Prev::Ident => false,
            // `await`/`yield` are contextual: when they are keywords (module top level, async or
            // generator bodies) they prefix an expression, so a following `/` starts a regex. They
            // are identifiers here since the lexer lacks that context; allow the regex form —
            // division right after a bare `await`/`yield` identifier is vanishingly rare.
            Prev::Await | Prev::Yield => true,
            Prev::Kw(k) => !matches!(
                k,
                Keyword::This | Keyword::Super | Keyword::True | Keyword::False | Keyword::Null
            ),
            // Calls/grouping and brackets produce values; a control header ends a statement
            // prefix. Braces distinguish a block (regex) from an object literal (division).
            Prev::Punct(Punct::RParen) => br.last_close_control,
            Prev::Punct(Punct::RBracket) => false,
            Prev::Punct(Punct::RBrace) => br.last_close_block,
            Prev::Punct(_) => true,
        }
    }

    /// Whether a `function`/`class` keyword after this token sits at statement position (a
    /// declaration, whose body's closing `}` ends a statement).
    fn at_statement_start(self, br: &Brackets) -> bool {
        match self {
            Prev::Start => true,
            Prev::Punct(Punct::Semi | Punct::LBrace) => true,
            Prev::Punct(Punct::RBrace) => br.last_close_block,
            Prev::Kw(k) => matches!(
                k,
                Keyword::Else
                    | Keyword::Do
                    | Keyword::Try
                    | Keyword::Finally
                    | Keyword::Export
                    | Keyword::Default
            ),
            _ => false,
        }
    }

    /// Whether a `{` after this token (other than a `)`) opens a block or control body rather
    /// than an object literal.
    fn block_follows(self) -> bool {
        match self {
            Prev::Start => true,
            Prev::Punct(p) => matches!(p, Punct::Semi | Punct::LBrace | Punct::RBrace | Punct::Arrow),
            Prev::Kw(k) => matches!(k, Keyword::Else | Keyword::Do | Keyword::Try | Keyword::Finally),
            _ => false,
        }
    }

    /// Whether a `(` after this token (and `before` it) opens a control-statement header.
    fn opens_control_paren(self, before: Prev) -> bool {
        match self {
            Prev::Kw(
                Keyword::If
                | Keyword::For
                | Keyword::While
                | Keyword::With
                | Keyword::Switch
                | Keyword::Catch,
            ) => !matches!(before, Prev::Punct(Punct::Dot | Punct::OptChain)),
            Prev::Await => before == Prev::Kw(Keyword::For),
            _ => false,
        }
    }
}

/// The open brackets as far as the `/` ambiguity needs them.
#[derive(Default)]
struct Brackets {
    /// One entry per open `{`: `true` if it opened a block/function body (statement position),
    /// `false` if an object literal (expression position). Used to disambiguate a `/` after `}`.
    braces: Vec<bool>,
    /// One entry per open `(`: it opens a control header. Call/grouping parentheses produce
    /// values; a following slash must tell those apart before scanning a regex body.
    parens: Vec<bool>,
    /// One entry per `function` keyword whose body has not opened yet: `true` if the function is
    /// an *expression* (so its body `}` is followed by division), `false` if a *declaration*
    /// (its `}` ends a statement, so a `/` after it is a regex).
    pending_fn: Vec<bool>,
    /// Set by a `class` keyword: the next `{` is the class body, classified by whether the class
    /// is a declaration (statement position) or an expression.
    pending_class: Option<bool>,
    /// A `/` after a block-closing `}` begins a regex; after an object-literal-closing `}` it is
    /// division.
    last_close_block: bool,
    last_close_control: bool,
}

/// Where [`Brackets`] stood, to rewind to.
struct BracketsMark {
    lens: [usize; 3],
    pending_class: Option<bool>,
    last_close: [bool; 2],
}

impl Brackets {
    fn mark(&self) -> BracketsMark {
        BracketsMark {
            lens: [self.braces.len(), self.parens.len(), self.pending_fn.len()],
            pending_class: self.pending_class,
            last_close: [self.last_close_block, self.last_close_control],
        }
    }

    /// Back to `mark`, provided nothing below it was popped since.
    fn rewind(&mut self, mark: &BracketsMark) {
        self.braces.truncate(mark.lens[0]);
        self.parens.truncate(mark.lens[1]);
        self.pending_fn.truncate(mark.lens[2]);
        self.pending_class = mark.pending_class;
        [self.last_close_block, self.last_close_control] = mark.last_close;
    }

    fn open_paren(&mut self, prev: Prev, before: Prev) {
        self.parens.push(prev.opens_control_paren(before));
    }

    fn close_paren(&mut self) {
        self.last_close_control = self.parens.pop().unwrap_or(false);
    }

    fn function_keyword(&mut self, prev: Prev) {
        let is_expr = !prev.at_statement_start(self);
        self.pending_fn.push(is_expr);
    }

    fn class_keyword(&mut self, prev: Prev) {
        self.pending_class = Some(prev.at_statement_start(self));
    }

    /// Classify an opening `{` (about to be pushed): does the matching `}` end at statement
    /// position (a block, control body, or function *declaration* body → a following `/` is a
    /// regex) or at value position (an object literal or function *expression* body → division)?
    /// Returns whether it opens a class body.
    fn open_brace(&mut self, prev: Prev) -> bool {
        let class_body = self.pending_class.is_some();
        let ends_stmt = match self.pending_class.take() {
            Some(is_decl) => is_decl,
            None if prev == Prev::Punct(Punct::RParen) => {
                // A body after `)` — either a function body (classified by `pending_fn`) or a
                // control block / method (statement position).
                self.pending_fn.pop().map_or(true, |is_expr| !is_expr)
            }
            None => prev.block_follows(),
        };
        self.braces.push(ends_stmt);
        class_body
    }

    fn close_brace(&mut self) {
        self.last_close_block = self.braces.pop().unwrap_or(false);
    }
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
    /// Byte offset where the token currently being scanned began.
    tok_start: usize,
    br: Brackets,
    /// Parallel to `br.parens`: the `(` opens the parameter list of a `function`.
    fn_paren_stack: Vec<bool>,
    last_close_fn_head: bool,
    /// Parallel to `br.braces`: the `{` opens a `function` body.
    fn_brace_stack: Vec<bool>,
    /// `function` bodies currently open.
    fn_depth: u32,
    /// Replace the tokens of each `function` body nested in at least this many others with
    /// [`Tok::Skim`] (see [`LexOpts::skim_min_depth`]).
    skim_min_depth: Option<u32>,
    skim_open: Option<SkimMark>,
    /// TypeScript source: a postfix non-null `!` (`x!`) ends an expression, so a `/` after it is
    /// a division.
    ts: bool,
    jsx: bool,
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
    /// Where the function bodies a skim jumped over end, from the lex that already scanned them.
    hints_in: Option<Rc<SkimHints>>,
    /// The function bodies the skim scans cross, recorded for later re-lexes of enclosing bodies.
    hints_out: Option<SkimHints>,
    hints_paused: bool,
    scan_heads: Vec<bool>,
    scan_fns: Vec<(u32, u32)>,
}

/// The `function` bodies seen while skimming, sorted by the offset of their `{`: with each, the
/// offset of its `}` shifted left once, with the low bit set if a line break precedes that `}`.
/// A body re-lexed later jumps to the recorded `}` instead of scanning to it again.
pub(crate) type SkimHints = Vec<(u32, u32)>;

const HINT_OPEN: u32 = u32::MAX;
const HINT_MIN_BYTES: u32 = 512;

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
    pub skim_hints: SkimHints,
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
    pub jsx: bool,
    /// Record the byte ranges of `/** */` comments.
    pub docs: bool,
    /// A leading `/` is a division.
    pub start_div: bool,
    /// Added to every token's byte offsets (the range starts this far into the parser's source).
    pub offset: u32,
    /// Keep no tokens for the body of a `function` nested in at least this many other function
    /// bodies: the lexer still lexes it (so its errors surface), then replaces its tokens with a
    /// single [`Tok::Skim`] spanning the braces. The parser re-lexes the range when it reaches the
    /// body, so a huge script never holds the tokens of all its function bodies at once. The
    /// tokens of a body being skimmed are dropped as they are lexed, so even one huge body is
    /// never held whole.
    pub skim_min_depth: Option<u32>,
    /// Record the bodies skimmed over (see [`SkimHints`]) in [`Lexed::skim_hints`].
    pub record_hints: bool,
}

/// [`tokenize_range`] with [`LexOpts`]; also returns the recorded JSDoc comment ranges.
pub(crate) fn tokenize_opts(
    src: &str,
    html_comments: bool,
    line: u32,
    opts: LexOpts,
) -> Result<Lexed, LexError> {
    tokenize_pieces(&[src], html_comments, line, opts)
}

/// Most identifier names a [`LexScratch`] keeps between lexes.
const SCRATCH_IDENTS: usize = 32 * 1024;

/// What a parser re-lexing many ranges of one source (the skimmed function bodies) carries from
/// one lex to the next, so each does not start from nothing: the interned identifier names, which
/// then are one allocation per name rather than per name per body, and the first token chunk.
#[derive(Default)]
pub(crate) struct LexScratch {
    idents: crate::fasthash::FastSet<Name>,
    chunk: Option<Vec<Token>>,
    hints: Option<Rc<SkimHints>>,
}

impl LexScratch {
    /// A scratch whose lexes jump over the bodies `hints` records.
    pub(crate) fn with_hints(hints: SkimHints) -> Self {
        LexScratch {
            hints: (!hints.is_empty()).then(|| Rc::new(hints)),
            ..Self::default()
        }
    }

    /// Keep the allocation of `tokens`, which the caller is done with, for the next lex.
    pub(crate) fn recycle(&mut self, tokens: TokVec) {
        if let Some(chunk) = tokens.into_first_chunk() {
            if self.chunk.as_ref().map_or(true, |c| c.capacity() < chunk.capacity()) {
                self.chunk = Some(chunk);
            }
        }
    }
}

pub(crate) fn tokenize_pieces<'a>(
    pieces: &[&'a str],
    html_comments: bool,
    line: u32,
    opts: LexOpts,
) -> Result<Lexed, LexError> {
    tokenize_pieces_with(pieces, html_comments, line, opts, &mut LexScratch::default())
}

pub(crate) fn tokenize_pieces_with<'a>(
    pieces: &[&'a str],
    html_comments: bool,
    line: u32,
    opts: LexOpts,
    scratch: &mut LexScratch,
) -> Result<Lexed, LexError> {
    let src = pieces.first().copied().unwrap_or("");
    if scratch.idents.len() > SCRATCH_IDENTS {
        scratch.idents.clear();
    }
    let out = match scratch.chunk.take() {
        Some(chunk) => TokVec::from_chunk(chunk),
        // About one token per six bytes of ordinary source.
        None => TokVec::with_capacity(pieces.iter().map(|p| p.len()).sum::<usize>() / 6 + 16),
    };
    let mut lx = Lexer {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        line,
        out,
        nl_pending: false,
        html_comments,
        pending_legacy: false,
        pending_lone_surrogate: false,
        br: Brackets::default(),
        tok_start: 0,
        fn_paren_stack: Vec::new(),
        last_close_fn_head: false,
        fn_brace_stack: Vec::new(),
        fn_depth: 0,
        skim_min_depth: opts.skim_min_depth,
        skim_open: None,
        ts: opts.ts,
        jsx: opts.jsx,
        docs: opts.docs.then(Vec::new),
        start_div: opts.start_div,
        idents: std::mem::take(&mut scratch.idents),
        offset: opts.offset,
        hints_in: scratch.hints.clone(),
        hints_out: opts.record_hints.then(Vec::new),
        hints_paused: false,
        scan_heads: Vec::new(),
        scan_fns: Vec::new(),
    };
    let ran = lx.run(pieces);
    scratch.idents = std::mem::take(&mut lx.idents);
    let soft_err = match ran {
        Ok(()) => None,
        Err(e) if lx.ts => {
            lx.tok_start = lx.pos.min(lx.src.len());
            lx.push(Tok::Eof);
            Some(e)
        }
        Err(e) => return Err(e),
    };
    Ok(Lexed {
        tokens: lx.out,
        docs: lx.docs.unwrap_or_default(),
        skim_hints: lx.hints_out.take().map_or_else(Vec::new, finish_hints),
        soft_err,
    })
}

fn finish_hints(mut hints: SkimHints) -> SkimHints {
    hints.retain(|&(open, close)| close != HINT_OPEN && (close >> 1) - open >= HINT_MIN_BYTES);
    hints.shrink_to_fit();
    hints
}

impl<'a> Lexer<'a> {
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
        let start = self.pos as u32 + self.offset;
        crate::parser::set_error_span(start, start + u32::from(self.pos < self.bytes.len()));
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
        let prev = self.prev(0);
        if self.ts && prev == Prev::Punct(Punct::Bang) {
            return !self.bang_is_postfix();
        }
        prev.regex_allowed(self.start_div, &self.br)
    }

    /// The class of the token `k` places before the last one pushed.
    fn prev(&self, k: usize) -> Prev {
        self.tok_back(k).map_or(Prev::Start, |t| Prev::of(&t.kind))
    }

    /// Certain declarations end at a line break before a JSX expression. In those cases
    /// `<tag>` starts the next statement even though the preceding token normally makes `/`
    /// a division operator (or `<` a relational operator).
    fn jsx_after_asi_declaration(&self) -> bool {
        let lexical_declaration = matches!(
            self.tok_back(1).map(|token| &token.kind),
            Some(Tok::Keyword(Keyword::Const | Keyword::Var))
        ) || matches!(
            self.tok_back(1).map(|token| &token.kind),
            Some(Tok::Ident(word)) if word == "let"
        );
        let import_declaration =
            matches!(self.tok_back(0).map(|token| &token.kind), Some(Tok::Str(_)))
                && matches!(
                    self.tok_back(1).map(|token| &token.kind),
                    Some(Tok::Ident(word)) if word == "from"
                )
                && (0..self.out.len().saturating_sub(2))
                    .rev()
                    .take(32)
                    .any(|index| {
                        self.out
                            .get(index)
                            .is_some_and(|token| matches!(&token.kind, Tok::Keyword(Keyword::Import)))
                    });
        let bare_binding = matches!(
            self.tok_back(0).map(|token| &token.kind),
            Some(Tok::Ident(_))
        ) && lexical_declaration;
        self.nl_pending && (bare_binding || import_declaration)
    }

    /// In TypeScript, whether the `!` just pushed is a non-null assertion (`x!`): it follows a
    /// token that ends an operand on the same line, where a prefix `!` could not stand.
    fn bang_is_postfix(&self) -> bool {
        let (Some(last), Some(before)) = (self.tok_back(0), self.tok_back(1)) else {
            return false;
        };
        if last.nl_before() {
            return false;
        }
        match &before.kind {
            Tok::Ident(_) | Tok::Num(_) | Tok::BigInt(_) | Tok::Str(_) | Tok::Template(_) => true,
            Tok::Keyword(k) => matches!(k.as_str(), "this" | "super" | "null" | "true" | "false"),
            Tok::Punct(p) => matches!(p.as_str(), ")" | "]" | "!"),
            _ => false,
        }
    }

    fn push(&mut self, kind: Tok) {
        let mut skim_here = false;
        match &kind {
            Tok::Punct(Punct::LParen) => {
                let (prev, before) = (self.prev(0), self.prev(1));
                self.br.open_paren(prev, before);
                if self.skim_min_depth.is_some() {
                    let head = self.paren_opens_function_params();
                    self.fn_paren_stack.push(head);
                }
            }
            Tok::Punct(Punct::RParen) => {
                self.br.close_paren();
                if self.skim_min_depth.is_some() {
                    self.last_close_fn_head = self.fn_paren_stack.pop().unwrap_or(false);
                }
            }
            Tok::Keyword(Keyword::Function) => {
                let prev = self.prev(0);
                self.br.function_keyword(prev);
            }
            Tok::Keyword(Keyword::Class) => {
                let prev = self.prev(0);
                self.br.class_keyword(prev);
            }
            Tok::Punct(Punct::LBrace) => {
                let prev = self.prev(0);
                let brace_depth = self.br.braces.len();
                let class_body = self.br.open_brace(prev);
                if let Some(min) = self.skim_min_depth {
                    let fn_body = !class_body
                        && self.last_close_fn_head
                        && prev == Prev::Punct(Punct::RParen);
                    self.fn_brace_stack.push(fn_body);
                    if fn_body {
                        if self.skim_open.is_none() && self.fn_depth >= min {
                            self.skim_open = Some(SkimMark {
                                brace_depth,
                                out_len: self.out.len() + 1,
                                line: self.line,
                                start: self.tok_start as u32 + self.offset,
                            });
                            skim_here = true;
                        }
                        self.fn_depth += 1;
                    }
                }
            }
            Tok::Punct(Punct::RBrace) => {
                self.br.close_brace();
                if self.skim_min_depth.is_some() && self.fn_brace_stack.pop().unwrap_or(false) {
                    self.fn_depth = self.fn_depth.saturating_sub(1);
                    if let Some(mark) = self.skim_open.filter(|m| m.brace_depth == self.br.braces.len()) {
                        self.skim_open = None;
                        self.out.truncate(mark.out_len);
                        self.out.push(Token::new(
                            Tok::Skim,
                            mark.line,
                            mark.start,
                            self.pos as u32 + self.offset,
                            false,
                        ));
                    }
                }
            }
            _ => {}
        }
        let nl = self.nl_pending;
        self.nl_pending = false;
        self.out.push(Token::new(
            kind,
            self.line,
            self.tok_start as u32 + self.offset,
            self.pos as u32 + self.offset,
            nl,
        ));
        #[cfg(test)]
        PEAK_TOKENS.with(|p| p.set(p.get().max(self.out.len())));
        if skim_here {
            self.skip_body();
        } else if let Some(mark) = self.skim_open {
            let len = self.out.len();
            if len - mark.out_len >= SKIM_WINDOW + SKIM_TAIL {
                self.out.splice(mark.out_len..len - SKIM_TAIL, None);
            }
        }
    }

    /// With the cursor just past the `{` of a body being skimmed, jump to its closing `}` (left
    /// for the ordinary lexer to produce the [`Tok::Skim`]) without building tokens. Anything the
    /// byte scan is not sure about (a construct that looks malformed, non-ASCII code, HTML-like
    /// comments, running out of input) leaves the cursor and bracket state as they were, so the
    /// body is lexed token by token and any lex error comes out as it always did.
    fn skip_body(&mut self) {
        let start = self.pos;
        if let Some((close, nl)) = self.hinted_close() {
            self.pos = close;
            self.line += self.bytes[start..close].iter().filter(|&&b| b == b'\n').count() as u32;
            self.nl_pending = nl;
            #[cfg(test)]
            {
                SKIPPED_BODIES.with(|n| n.set(n.get() + 1));
                HINTED_BODIES.with(|n| n.set(n.get() + 1));
            }
            return;
        }
        let mark = self.br.mark();
        let docs = self.docs.as_ref().map(Vec::len);
        let hints = self.hints_out.as_ref().map(Vec::len);
        let mut nl = false;
        let skipped = self.skip_to_close(Prev::Punct(Punct::LBrace), &mut nl);
        self.scan_heads.clear();
        self.scan_fns.clear();
        if skipped.is_none() {
            self.br.rewind(&mark);
            if let (Some(found), Some(len)) = (self.docs.as_mut(), docs) {
                found.truncate(len);
            }
            if let (Some(found), Some(len)) = (self.hints_out.as_mut(), hints) {
                found.truncate(len);
            }
            self.pos = start;
            return;
        }
        self.line += self.bytes[start..self.pos].iter().filter(|&&b| b == b'\n').count() as u32;
        self.nl_pending = nl;
        #[cfg(test)]
        SKIPPED_BODIES.with(|n| n.set(n.get() + 1));
    }

    /// The `}` (and whether a line break precedes it) that a previous scan found closing the body
    /// whose `{` was just lexed.
    fn hinted_close(&self) -> Option<(usize, bool)> {
        let hints = self.hints_in.as_ref().filter(|_| self.docs.is_none())?;
        let open = self.tok_start as u32 + self.offset;
        let at = hints.binary_search_by_key(&open, |h| h.0).ok()?;
        let packed = hints[at].1;
        let close = ((packed >> 1).checked_sub(self.offset)?) as usize;
        (close >= self.pos && self.bytes.get(close) == Some(&b'}')).then_some((close, packed & 1 == 1))
    }

    /// Advance to the `}` that closes the brace level the lexer is at (not consuming it),
    /// tracking just enough of the token stream to tell a regex from a division. `nl` ends up
    /// whether a line terminator separates the last token from that `}`. Line numbers are left
    /// to the caller.
    #[allow(unused_assignments)]
    fn skip_to_close(&mut self, start: Prev, nl: &mut bool) -> Option<()> {
        let (bytes, src) = (self.bytes, self.src);
        let (base_braces, base_parens, base_fns) =
            (self.br.braces.len(), self.br.parens.len(), self.br.pending_fn.len());
        let (mut prev, mut before) = (start, Prev::Start);
        let mut pos = self.pos;
        let rec = self.hints_out.is_some()
            && !self.hints_paused
            && self.offset as usize + bytes.len() < 1 << 30;
        // After `function`, `function *`, `function name` / `function * name`: 1, 2, 3.
        let mut fn_head = 0u8;
        let mut last_head = false;
        macro_rules! token {
            ($class:expr) => {{
                before = prev;
                prev = $class;
                *nl = false;
                fn_head = 0;
            }};
        }
        loop {
            let b = *bytes.get(pos)?;
            match SCAN_CLASS[b as usize] {
                SC_SPACE => {
                    pos += 1;
                    continue;
                }
                SC_NEWLINE => {
                    *nl = true;
                    pos += 1;
                    continue;
                }
                SC_QUOTE => {
                    pos = string_end(bytes, pos)?;
                    token!(Prev::Operand);
                    continue;
                }
                SC_TEMPLATE => {
                    self.pos = pos + 1;
                    self.skip_template()?;
                    pos = self.pos;
                    token!(Prev::Operand);
                    continue;
                }
                SC_DIGIT => {
                    pos = number_end(bytes, pos);
                    token!(Prev::Operand);
                    continue;
                }
                SC_IDENT | SC_HASH => {
                    let from = pos + usize::from(b == b'#');
                    if !bytes.get(from).is_some_and(|&c| IDENT_START_BYTE[c as usize]) {
                        return None;
                    }
                    let end = ident_end(bytes, from);
                    if bytes.get(end).is_some_and(|&c| c == b'\\' || c >= 0x80) {
                        return None;
                    }
                    let word = &src[pos..end];
                    let head = fn_head;
                    let mut next_head = 0;
                    let class = match (end - pos > 1 && b != b'#').then(|| keyword(word)).flatten() {
                        Some(kw) => {
                            match kw {
                                Keyword::Function => {
                                    self.br.function_keyword(prev);
                                    next_head = u8::from(!matches!(
                                        prev,
                                        Prev::Punct(Punct::Dot | Punct::OptChain)
                                    ));
                                }
                                Keyword::Class => self.br.class_keyword(prev),
                                _ => {}
                            }
                            Prev::Kw(kw)
                        }
                        None => {
                            next_head = if matches!(head, 1 | 2) { 3 } else { 0 };
                            if end - pos == 5 { Prev::of_ident(word) } else { Prev::Ident }
                        }
                    };
                    pos = end;
                    token!(class);
                    fn_head = next_head;
                    continue;
                }
                SC_DOT if bytes.get(pos + 1).is_some_and(u8::is_ascii_digit) => {
                    pos = number_end(bytes, pos + 1);
                    token!(Prev::Operand);
                    continue;
                }
                SC_SLASH => {
                    match bytes.get(pos + 1) {
                        Some(b'/') => {
                            pos = line_comment_end(bytes, pos);
                            continue;
                        }
                        Some(b'*') => {
                            let (end, has_nl) = block_comment_end(bytes, pos)?;
                            if let Some(docs) = &mut self.docs {
                                if bytes.get(pos + 2) == Some(&b'*') && end - pos > 4 {
                                    docs.push((pos as u32, end as u32));
                                }
                            }
                            *nl |= has_nl;
                            pos = end;
                            continue;
                        }
                        _ => {}
                    }
                    if prev.regex_allowed(false, &self.br) {
                        pos = regex_end(bytes, pos)?;
                        token!(Prev::Operand);
                        continue;
                    }
                }
                SC_LT if self.html_comments && bytes[pos..].starts_with(b"<!--") => return None,
                SC_MINUS
                    if self.html_comments
                        && bytes[pos..].starts_with(b"-->")
                        && (*nl || prev == Prev::Start) =>
                {
                    return None
                }
                SC_E2 => {
                    if !is_ls_ps(bytes, pos) {
                        return None;
                    }
                    *nl = true;
                    pos += 3;
                    continue;
                }
                SC_BAIL => return None,
                _ => {}
            }
            let p = scan_punct(&bytes[pos..])?;
            let head = fn_head;
            match p {
                Punct::LParen => {
                    self.br.open_paren(prev, before);
                    if rec {
                        self.scan_heads.push(head != 0);
                    }
                }
                Punct::RParen => {
                    if self.br.parens.len() <= base_parens {
                        return None;
                    }
                    self.br.close_paren();
                    if rec {
                        last_head = self.scan_heads.pop().unwrap_or(false);
                    }
                }
                Punct::LBrace => {
                    let pops_outer_fn = prev == Prev::Punct(Punct::RParen)
                        && self.br.pending_class.is_none()
                        && base_fns > 0
                        && self.br.pending_fn.len() <= base_fns;
                    if pops_outer_fn {
                        return None;
                    }
                    let class_body = self.br.open_brace(prev);
                    if rec {
                        let fn_body = !class_body && last_head && prev == Prev::Punct(Punct::RParen);
                        let slot = match (fn_body, self.hints_out.as_mut()) {
                            (true, Some(found)) => {
                                found.push((pos as u32 + self.offset, HINT_OPEN));
                                found.len() as u32 - 1
                            }
                            _ => HINT_OPEN,
                        };
                        self.scan_fns.push((slot, self.br.parens.len() as u32));
                    }
                }
                Punct::RBrace => {
                    if self.br.braces.len() == base_braces {
                        self.pos = pos;
                        return (self.br.parens.len() == base_parens).then_some(());
                    }
                    self.br.close_brace();
                    if rec {
                        if let Some((slot, parens)) = self.scan_fns.pop() {
                            if slot != HINT_OPEN && parens as usize == self.br.parens.len() {
                                if let Some(found) = self.hints_out.as_mut() {
                                    let close = (pos as u32 + self.offset) << 1 | u32::from(*nl);
                                    found[slot as usize].1 = close;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            pos += p.len();
            token!(Prev::Punct(p));
            if p == Punct::Star && head == 1 {
                fn_head = 2;
            }
        }
    }

    /// Skip a template literal's body, the cursor just past its opening backtick. Each `${ }`
    /// hole is a lexing context of its own, as in [`Lexer::read_template_sub`].
    fn skip_template(&mut self) -> Option<()> {
        loop {
            let at = self
                .bytes
                .get(self.pos..)?
                .iter()
                .position(|&b| matches!(b, b'\\' | b'`' | b'$'))?
                + self.pos;
            match self.bytes[at] {
                b'\\' => self.pos = at + 2,
                b'`' => {
                    self.pos = at + 1;
                    return Some(());
                }
                _ if self.bytes.get(at + 1) == Some(&b'{') => {
                    if crate::stack::exhausted() {
                        return None;
                    }
                    self.pos = at + 2;
                    let outer = std::mem::take(&mut self.br);
                    let paused = std::mem::replace(&mut self.hints_paused, true);
                    let closed = self.skip_to_close(Prev::Start, &mut false);
                    self.hints_paused = paused;
                    self.br = outer;
                    closed?;
                    self.pos += 1;
                }
                _ => self.pos = at + 1,
            }
        }
    }

    /// Whether the `(` about to be pushed opens the parameter list of a `function` (`function (`,
    /// `function name(`, `function* name(`), not a property named `function`.
    fn paren_opens_function_params(&self) -> bool {
        let is_function = |back: usize| {
            matches!(self.tok_back(back).map(|t| &t.kind), Some(Tok::Keyword(Keyword::Function)))
                && !matches!(
                    self.tok_back(back + 1).map(|t| &t.kind),
                    Some(Tok::Punct(Punct::Dot | Punct::OptChain))
                )
        };
        let is_star = |back: usize| {
            matches!(self.tok_back(back).map(|t| &t.kind), Some(Tok::Punct(Punct::Star)))
        };
        let is_ident = matches!(self.tok_back(0).map(|t| &t.kind), Some(Tok::Ident(_)));
        is_function(0)
            || (is_star(0) && is_function(1))
            || (is_ident && (is_function(1) || (is_star(1) && is_function(2))))
    }
    /// Flag the most recently pushed token as having contained a `\u` escape.
    fn mark_escaped(&mut self) {
        if let Some(t) = self.out.last_mut() {
            t.set_escaped(true);
        }
    }
    /// Flag the most recently pushed token as containing a lone surrogate.
    fn mark_lone_surrogate(&mut self) {
        if let Some(t) = self.out.last_mut() {
            t.set_lone_surrogate(true);
        }
    }
    /// Flag the most recently pushed token as a legacy-octal construct.
    fn mark_legacy_octal(&mut self) {
        if let Some(t) = self.out.last_mut() {
            t.set_legacy_octal(true);
        }
    }

    fn run(&mut self, pieces: &[&'a str]) -> Result<(), LexError> {
        for (k, piece) in pieces.iter().enumerate() {
            if k > 0 {
                self.offset += self.src.len() as u32;
                self.src = piece;
                self.bytes = piece.as_bytes();
                self.pos = 0;
            }
            // Hashbang comment: `#!...` only at the very start of the source.
            if k == 0 && self.peek() == Some('#') && self.peek2() == Some('!') {
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
        }
        self.tok_start = self.pos;
        self.push(Tok::Eof);
        Ok(())
    }

    /// Scan one token (or skip one whitespace char / comment) starting with `c`.
    #[inline]
    fn step(&mut self, c: char) -> Result<(), LexError> {
        self.tok_start = self.pos;
        if c.is_ascii() {
            match ASCII_CLASS[c as usize] {
                CLASS_IDENT => return self.read_ident(),
                CLASS_SPACE => {
                    self.pos += 1;
                    return Ok(());
                }
                CLASS_NEWLINE => {
                    self.nl_pending = true;
                    self.pos += 1;
                    self.line += 1;
                    return Ok(());
                }
                CLASS_PUNCT => return self.read_punct(),
                CLASS_QUOTE => return self.read_string(c),
                CLASS_DIGIT => return self.read_number(),
                CLASS_DOT => {
                    return if self.bytes.get(self.pos + 1).is_some_and(u8::is_ascii_digit) {
                        self.read_number()
                    } else {
                        self.read_punct()
                    };
                }
                CLASS_ANGLE => {
                    let next = self.bytes.get(self.pos + 1).copied();
                    let special = if c == '<' {
                        self.jsx || (self.html_comments && next == Some(b'!'))
                    } else {
                        self.html_comments && next == Some(b'-')
                    };
                    if !special {
                        return self.read_punct();
                    }
                }
                _ => {}
            }
        }
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
        } else if c == '<'
            && self.jsx
            && (self.regex_allowed() || self.jsx_after_asi_declaration())
            && self.jsx_starts()
        {
            let (start, nl) = (self.tok_start, std::mem::take(&mut self.nl_pending));
            let element = self.read_jsx_element()?;
            self.tok_start = start;
            self.nl_pending = nl;
            self.push(Tok::Jsx(Packed::new(Rc::new(element))));
            if let Some(token) = self.out.last_mut() {
                if let Tok::Jsx(element) = &token.kind {
                    token.set_line(element.line);
                }
            }
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
        let from = self.pos;
        if self.bytes.get(from).is_some_and(|&b| IDENT_START_BYTE[b as usize]) {
            let end = ident_end(self.bytes, from + 1);
            if !self.bytes.get(end).is_some_and(|&b| b == b'\\' || b >= 0x80) {
                self.pos = end;
                let text = &self.src[from..end];
                match keyword(text) {
                    Some(kw) => self.push(Tok::Keyword(kw)),
                    None => {
                        let name = self.intern(text);
                        self.push(Tok::Ident(name));
                    }
                }
                return Ok(());
            }
        }
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
        if self
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        {
            let from = self.pos;
            let mut end = from;
            while let Some(&c) = self.bytes.get(end) {
                if IDENT_BYTE[c as usize] {
                    end += 1;
                } else {
                    break;
                }
            }
            self.pos = end;
            if !self
                .bytes
                .get(end)
                .is_some_and(|&b| b == b'\\' || b >= 0x80)
            {
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
        let n = Name::new(name);
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
        self.pending_legacy = false;
        self.pending_lone_surrogate = false;
        let from = self.pos;
        let end = self.bytes[from..]
            .iter()
            .position(|&b| matches!(b, b'\\' | b'\n' | b'\r') || b == quote as u8)
            .map(|n| from + n);
        // No escape or line break before the closing quote: the literal is a plain source slice.
        // The quote is ASCII, so `from` and `end` lie on char boundaries.
        if let Some(end) = end.filter(|&e| self.bytes[e] == quote as u8) {
            let text = &self.src[from..end];
            let value = if text.len() <= 24 {
                self.intern(text)
            } else {
                Name::new(text)
            };
            self.pos = end + 1;
            self.push(Tok::Str(value));
            return Ok(());
        }
        let mut s = String::new();
        if let Some(end) = end {
            s.push_str(&self.src[from..end]);
            self.pos = end;
        }
        let mut had_escape = false;
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
                Some(c) => {
                    s.push(c);
                    let rest = &self.bytes[self.pos..];
                    let run = rest
                        .iter()
                        .position(|&b| matches!(b, b'\\' | b'\n' | b'\r') || b == quote as u8)
                        .unwrap_or(rest.len());
                    s.push_str(&self.src[self.pos..self.pos + run]);
                    self.pos += run;
                }
            }
        }
        self.push(Tok::Str(Name::new(&s)));
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
        self.push(Tok::Template(Packed::new(Box::new(parts))));
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
            std::mem::take(&mut self.br),
            std::mem::take(&mut self.start_div),
            std::mem::take(&mut self.fn_paren_stack),
            std::mem::take(&mut self.fn_brace_stack),
            std::mem::take(&mut self.fn_depth),
            self.skim_min_depth.take(),
            self.skim_open.take(),
            self.last_close_fn_head,
        );
        let r = loop {
            match self.peek() {
                None => break Err(self.err("unterminated template substitution")),
                Some('}') if self.br.braces.is_empty() => {
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
        self.br = saved.1;
        self.start_div = saved.2;
        self.fn_paren_stack = saved.3;
        self.fn_brace_stack = saved.4;
        self.fn_depth = saved.5;
        self.skim_min_depth = saved.6;
        self.skim_open = saved.7;
        self.last_close_fn_head = saved.8;
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

    /// Four hex digits at the cursor, consumed.
    fn take_hex4(&mut self) -> Option<u32> {
        let digits = self.bytes.get(self.pos..self.pos + 4)?;
        let mut n = 0;
        for &d in digits {
            n = n << 4 | (d as char).to_digit(16)?;
        }
        self.pos += 4;
        Some(n)
    }

    fn read_unicode_escape(&mut self, out: &mut String) -> Result<(), LexError> {
        let n = match self.take_hex4() {
            Some(n) => n,
            None => self.read_unicode_escape_digits()?,
        };
        self.finish_unicode_escape(n, out)
    }

    fn read_unicode_escape_digits(&mut self) -> Result<u32, LexError> {
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
        Ok(n)
    }

    fn finish_unicode_escape(&mut self, n: u32, out: &mut String) -> Result<(), LexError> {
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
        let start = self.pos;
        let mut end = start;
        let mut value = 0u64;
        while let Some(d) = self.bytes.get(end).filter(|d| d.is_ascii_digit()) {
            value = value * 10 + u64::from(d - b'0');
            end += 1;
            if end - start > 15 {
                break;
            }
        }
        let plain = end > start
            && end - start <= 15
            && (end - start == 1 || self.bytes[start] != b'0')
            && !self.bytes.get(end).is_some_and(|&c| {
                IDENT_BYTE[c as usize] || matches!(c, b'.' | b'\\') || c >= 0x80
            });
        if plain {
            self.pos = end;
            self.push(Tok::Num(Num::new(value as f64)));
            return Ok(());
        }
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
                self.push(Tok::BigInt(Packed::new(Box::new(n))));
                return Ok(());
            }
            let n = u64::from_str_radix(&digits, radix)
                .map_err(|_| self.err("invalid numeric literal"))?;
            self.push(Tok::Num(Num::new(n as f64)));
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
                self.push(Tok::Num(Num::new(n as f64)));
                self.mark_legacy_octal();
                return Ok(());
            } else if text.chars().all(|c| c.is_ascii_digit()) {
                let n: f64 = text.parse().unwrap_or(0.0);
                self.push(Tok::Num(Num::new(n)));
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
            self.push(Tok::BigInt(Packed::new(Box::new(n))));
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
        let raw = &self.src[start..self.pos];
        let parsed = if raw.contains('_') {
            raw.chars().filter(|c| *c != '_').collect::<String>().parse()
        } else {
            raw.parse()
        };
        let n: f64 = parsed.map_err(|_| self.err("invalid numeric literal"))?;
        self.push(Tok::Num(Num::new(n)));
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
        self.push(Tok::Regex(Packed::new(Box::new(RegexTok { body, flags }))));
        Ok(())
    }

    fn read_punct(&mut self) -> Result<(), LexError> {
        let Some(p) = scan_punct(&self.bytes[self.pos..]) else {
            return Err(self.err(format!(
                "unexpected character {:?}",
                self.peek().unwrap_or('\0')
            )));
        };
        self.pos += p.len();
        self.push(Tok::Punct(p));
        Ok(())
    }
}

/// The longest punctuator at the start of `b` (maximal munch, the same as scanning `PUNCTUATORS`,
/// which lists longer spellings first). `?.` followed by a digit is `?` then `.` (a conditional
/// like `x ? .5 : .3`), not optional chaining.
#[inline]
fn scan_punct(b: &[u8]) -> Option<Punct> {
    let c0 = *b.first()?;
    if let Some(Some(p)) = SINGLE_PUNCT.get(c0 as usize) {
        return Some(*p);
    }
    let at = |k: usize| b.get(k).copied().unwrap_or(0);
    let (c1, c2) = (at(1), at(2));
    Some(match c0 {
        b'{' => Punct::LBrace,
        b'}' => Punct::RBrace,
        b'(' => Punct::LParen,
        b')' => Punct::RParen,
        b'[' => Punct::LBracket,
        b']' => Punct::RBracket,
        b';' => Punct::Semi,
        b',' => Punct::Comma,
        b'~' => Punct::Tilde,
        b':' => Punct::Colon,
        b'@' => Punct::At,
        b'.' if c1 == b'.' && c2 == b'.' => Punct::Ellipsis,
        b'.' => Punct::Dot,
        b'=' => match (c1, c2) {
            (b'=', b'=') => Punct::StrictEq,
            (b'>', _) => Punct::Arrow,
            (b'=', _) => Punct::EqEq,
            _ => Punct::Assign,
        },
        b'!' => match (c1, c2) {
            (b'=', b'=') => Punct::StrictNe,
            (b'=', _) => Punct::NotEq,
            _ => Punct::Bang,
        },
        b'*' => match (c1, c2) {
            (b'*', b'=') => Punct::PowAssign,
            (b'*', _) => Punct::Pow,
            (b'=', _) => Punct::MulAssign,
            _ => Punct::Star,
        },
        b'<' => match (c1, c2) {
            (b'<', b'=') => Punct::ShlAssign,
            (b'<', _) => Punct::Shl,
            (b'=', _) => Punct::LtEq,
            _ => Punct::Lt,
        },
        b'>' => match (c1, c2) {
            (b'>', b'>') if at(3) == b'=' => Punct::UShrAssign,
            (b'>', b'>') => Punct::UShr,
            (b'>', b'=') => Punct::ShrAssign,
            (b'>', _) => Punct::Shr,
            (b'=', _) => Punct::GtEq,
            _ => Punct::Gt,
        },
        b'&' => match (c1, c2) {
            (b'&', b'=') => Punct::AndAssign,
            (b'&', _) => Punct::AndAnd,
            (b'=', _) => Punct::BitAndAssign,
            _ => Punct::Amp,
        },
        b'|' => match (c1, c2) {
            (b'|', b'=') => Punct::OrAssign,
            (b'|', _) => Punct::OrOr,
            (b'=', _) => Punct::BitOrAssign,
            _ => Punct::Pipe,
        },
        b'?' => match (c1, c2) {
            (b'.', d) if d.is_ascii_digit() => Punct::Question,
            (b'?', b'=') => Punct::NullishAssign,
            (b'?', _) => Punct::Nullish,
            (b'.', _) => Punct::OptChain,
            _ => Punct::Question,
        },
        b'+' => match c1 {
            b'+' => Punct::Inc,
            b'=' => Punct::AddAssign,
            _ => Punct::Plus,
        },
        b'-' => match c1 {
            b'-' => Punct::Dec,
            b'=' => Punct::SubAssign,
            _ => Punct::Minus,
        },
        b'/' => match c1 {
            b'=' => Punct::DivAssign,
            _ => Punct::Slash,
        },
        b'%' => match c1 {
            b'=' => Punct::RemAssign,
            _ => Punct::Percent,
        },
        b'^' => match c1 {
            b'=' => Punct::BitXorAssign,
            _ => Punct::Caret,
        },
        _ => return None,
    })
}

/// The punctuators that no longer spelling starts with.
static SINGLE_PUNCT: [Option<Punct>; 128] = {
    let mut t = [None; 128];
    t[b'{' as usize] = Some(Punct::LBrace);
    t[b'}' as usize] = Some(Punct::RBrace);
    t[b'(' as usize] = Some(Punct::LParen);
    t[b')' as usize] = Some(Punct::RParen);
    t[b'[' as usize] = Some(Punct::LBracket);
    t[b']' as usize] = Some(Punct::RBracket);
    t[b';' as usize] = Some(Punct::Semi);
    t[b',' as usize] = Some(Punct::Comma);
    t[b'~' as usize] = Some(Punct::Tilde);
    t[b':' as usize] = Some(Punct::Colon);
    t[b'@' as usize] = Some(Punct::At);
    t
};

/// Whether `bytes[at]` and the two bytes after it are U+2028 or U+2029.
fn is_ls_ps(bytes: &[u8], at: usize) -> bool {
    bytes[at] == 0xE2 && bytes.get(at + 1) == Some(&0x80) && matches!(bytes.get(at + 2), Some(0xA8 | 0xA9))
}

/// The end of the ASCII identifier run starting at `from`.
fn ident_end(bytes: &[u8], from: usize) -> usize {
    from + bytes[from..].iter().take_while(|&&c| IDENT_BYTE[c as usize]).count()
}

/// The end of the number literal starting at `from`: a run of digits, letters (hex digits,
/// exponent, `n`), `_` and `.`. Not validated; the token-by-token lexer reports a malformed one.
fn number_end(bytes: &[u8], from: usize) -> usize {
    from + bytes[from..]
        .iter()
        .take_while(|&&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'.')
        .count()
}

/// Just past the string literal starting at `at`; `None` if it hits a line break or the end.
fn string_end(bytes: &[u8], at: usize) -> Option<usize> {
    let quote = bytes[at];
    let mut i = at + 1;
    loop {
        let rest = bytes.get(i..)?;
        i += rest
            .iter()
            .position(|&b| matches!(b, b'\\' | b'\n' | b'\r') || b == quote)?;
        match bytes[i] {
            b'\\' => i += if bytes[i + 1..].starts_with(b"\r\n") { 3 } else { 2 },
            b'\n' | b'\r' => return None,
            _ => return Some(i + 1),
        }
    }
}

/// The line terminator (or end) that ends the `//` comment starting at `at`.
fn line_comment_end(bytes: &[u8], at: usize) -> usize {
    let mut i = at;
    loop {
        i += bytes[i..]
            .iter()
            .position(|&b| matches!(b, b'\n' | b'\r' | 0xE2))
            .unwrap_or(bytes.len() - i);
        if i >= bytes.len() || bytes[i] != 0xE2 || is_ls_ps(bytes, i) {
            return i;
        }
        i += 1;
    }
}

/// Just past the `/* */` comment starting at `at`, and whether it holds a line terminator.
fn block_comment_end(bytes: &[u8], at: usize) -> Option<(usize, bool)> {
    let mut i = at + 2;
    let mut nl = false;
    loop {
        match *bytes.get(i)? {
            b'*' if bytes.get(i + 1) == Some(&b'/') => return Some((i + 2, nl)),
            b'\n' | b'\r' => nl = true,
            0xE2 => nl |= is_ls_ps(bytes, i),
            _ => {}
        }
        i += 1;
    }
}

/// Just past the regex literal (and its flags) starting at `at`; `None` if it hits a line
/// terminator, the end, or flags the byte scan cannot vouch for.
fn regex_end(bytes: &[u8], at: usize) -> Option<usize> {
    let mut i = at + 1;
    let mut in_class = false;
    loop {
        match *bytes.get(i)? {
            b'\n' | b'\r' => return None,
            0xE2 if is_ls_ps(bytes, i) => return None,
            b'\\' => {
                i += 1;
                if matches!(*bytes.get(i)?, b'\n' | b'\r') || is_ls_ps(bytes, i) {
                    return None;
                }
            }
            b'[' => in_class = true,
            b']' => in_class = false,
            b'/' if !in_class => break,
            _ => {}
        }
        i += 1;
    }
    let end = ident_end(bytes, i + 1);
    (!bytes.get(end).is_some_and(|&c| c == b'\\' || c >= 0x80)).then_some(end)
}

/// ASCII IdentifierPart bytes (letters, digits, `_`, `$`).
static IDENT_BYTE: [bool; 256] = {
    let mut t = [false; 256];
    let mut i = 0;
    while i < 128 {
        let b = i as u8;
        t[i] = b.is_ascii_alphanumeric() || b == b'_' || b == b'$';
        i += 1;
    }
    t
};

/// ASCII IdentifierStart bytes (letters, `_`, `$`).
static IDENT_START_BYTE: [bool; 256] = {
    let mut t = [false; 256];
    let mut i = 0;
    while i < 128 {
        let b = i as u8;
        t[i] = b.is_ascii_alphabetic() || b == b'_' || b == b'$';
        i += 1;
    }
    t
};

const SC_BAIL: u8 = 0;
const SC_SPACE: u8 = 1;
const SC_NEWLINE: u8 = 2;
const SC_QUOTE: u8 = 3;
const SC_TEMPLATE: u8 = 4;
const SC_DIGIT: u8 = 5;
const SC_IDENT: u8 = 6;
const SC_HASH: u8 = 7;
const SC_DOT: u8 = 8;
const SC_SLASH: u8 = 9;
const SC_LT: u8 = 10;
const SC_MINUS: u8 = 11;
const SC_E2: u8 = 12;
const SC_PUNCT: u8 = 13;

/// What [`Lexer::skip_to_close`] does with each byte; `SC_BAIL` bytes (non-ASCII text,
/// backslashes, control characters) hand the body back to the token-by-token lexer.
static SCAN_CLASS: [u8; 256] = {
    let mut t = [SC_BAIL; 256];
    let mut i = 0;
    while i < 128 {
        let b = i as u8;
        t[i] = match b {
            b' ' | b'\t' | 0x0B | 0x0C => SC_SPACE,
            b'\n' | b'\r' => SC_NEWLINE,
            b'"' | b'\'' => SC_QUOTE,
            b'`' => SC_TEMPLATE,
            b'0'..=b'9' => SC_DIGIT,
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' => SC_IDENT,
            b'#' => SC_HASH,
            b'.' => SC_DOT,
            b'/' => SC_SLASH,
            b'<' => SC_LT,
            b'-' => SC_MINUS,
            b'{' | b'}' | b'(' | b')' | b'[' | b']' | b';' | b',' | b':' | b'~' | b'@' | b'='
            | b'!' | b'*' | b'%' | b'^' | b'&' | b'|' | b'+' | b'>' | b'?' => SC_PUNCT,
            _ => SC_BAIL,
        };
        i += 1;
    }
    t[0xE2] = SC_E2;
    t
};

const CLASS_OTHER: u8 = 0;
const CLASS_IDENT: u8 = 1;
const CLASS_SPACE: u8 = 2;
const CLASS_NEWLINE: u8 = 3;
/// A punctuator that can never start a comment, regex, number, string, template or JSX element.
const CLASS_PUNCT: u8 = 4;
const CLASS_QUOTE: u8 = 5;
const CLASS_DIGIT: u8 = 6;
const CLASS_DOT: u8 = 7;
/// `-` and `<`, which can start an HTML-like comment (or `<` a JSX element).
const CLASS_ANGLE: u8 = 8;

/// What [`Lexer::step`] can decide from an ASCII byte alone; everything else takes the general path.
const ASCII_CLASS: [u8; 128] = {
    let mut t = [CLASS_OTHER; 128];
    let mut i = 0;
    while i < 128 {
        let b = i as u8;
        t[i] = match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'$' | b'#' => CLASS_IDENT,
            b'"' | b'\'' => CLASS_QUOTE,
            b'0'..=b'9' => CLASS_DIGIT,
            b'.' => CLASS_DOT,
            b'-' | b'<' => CLASS_ANGLE,
            b' ' | b'\t' => CLASS_SPACE,
            b'\n' => CLASS_NEWLINE,
            b'{' | b'}' | b'(' | b')' | b'[' | b']' | b';' | b',' | b':' | b'~' | b'?' | b'='
            | b'!' | b'*' | b'%' | b'^' | b'&' | b'|' | b'+' | b'>' | b'@' => CLASS_PUNCT,
            _ => CLASS_OTHER,
        };
        i += 1;
    }
    t
};

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
/// Per first byte, a bit for each length (up to 14) of a reserved word starting with it.
static KEYWORD_LENS: [u16; 128] = {
    let mut t = [0u16; 128];
    let mut i = 0;
    while i < crate::token::KEYWORDS.len() {
        let w = crate::token::KEYWORDS[i].as_bytes();
        t[w[0] as usize] |= 1 << w.len();
        i += 1;
    }
    t
};
/// The reserved word spelled `s` (one of `token::KEYWORDS`).
fn keyword(s: &str) -> Option<Keyword> {
    let first = *s.as_bytes().first()?;
    if first >= 0x80 || KEYWORD_LENS[first as usize] & (1 << s.len().min(15)) == 0 {
        return None;
    }
    Some(match s {
        "break" => Keyword::Break,
        "case" => Keyword::Case,
        "catch" => Keyword::Catch,
        "class" => Keyword::Class,
        "const" => Keyword::Const,
        "continue" => Keyword::Continue,
        "debugger" => Keyword::Debugger,
        "default" => Keyword::Default,
        "delete" => Keyword::Delete,
        "do" => Keyword::Do,
        "else" => Keyword::Else,
        "enum" => Keyword::Enum,
        "export" => Keyword::Export,
        "extends" => Keyword::Extends,
        "false" => Keyword::False,
        "finally" => Keyword::Finally,
        "for" => Keyword::For,
        "function" => Keyword::Function,
        "if" => Keyword::If,
        "import" => Keyword::Import,
        "in" => Keyword::In,
        "instanceof" => Keyword::Instanceof,
        "new" => Keyword::New,
        "null" => Keyword::Null,
        "return" => Keyword::Return,
        "super" => Keyword::Super,
        "switch" => Keyword::Switch,
        "this" => Keyword::This,
        "throw" => Keyword::Throw,
        "true" => Keyword::True,
        "try" => Keyword::Try,
        "typeof" => Keyword::Typeof,
        "var" => Keyword::Var,
        "void" => Keyword::Void,
        "while" => Keyword::While,
        "with" => Keyword::With,
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
            assert_eq!(keyword(k).map(|w| w.as_str()), Some(*k));
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
            assert!(matches!(toks[1].kind, Tok::Punct(q) if q.as_str() == *p), "{p}");
        }
        let toks = tokenize("a>>>=b?.5:.3").unwrap();
        let puncts: Vec<&str> = toks
            .iter()
            .filter_map(|t| match t.kind {
                Tok::Punct(p) => Some(p.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(puncts, [">>>=", "?", ":"]);
    }

    fn skim_kinds(src: &str, min_depth: u32) -> Vec<String> {
        let opts = LexOpts {
            skim_min_depth: Some(min_depth),
            ..LexOpts::default()
        };
        let lexed = tokenize_opts(src, true, 1, opts).unwrap();
        lexed
            .tokens
            .iter_from(0)
            .map(|t| match &t.kind {
                Tok::Skim => format!("skim:{}..{}", t.start, t.end),
                Tok::Punct(p) => p.as_str().to_string(),
                Tok::Keyword(k) => k.as_str().to_string(),
                Tok::Ident(n) => n.to_string(),
                Tok::Eof => "eof".to_string(),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn skim_replaces_function_bodies_only() {
        let src = "function f(a) { if (a) { g(); } return /x/.test(a); } if (z) { y(); } o = { function() { 1 } };";
        let kinds = skim_kinds(src, 0);
        let skims = kinds.iter().filter(|k| k.starts_with("skim:")).count();
        assert_eq!(skims, 2, "{kinds:?}");
        assert_eq!(&kinds[..7], ["function", "f", "(", "a", ")", "{", "skim:14..53"], "{kinds:?}");
        assert!(kinds.windows(3).any(|w| w == ["if", "(", "z"]));
        assert!(kinds.contains(&"y".to_string()));
    }

    #[test]
    fn skim_keeps_the_outermost_body_at_depth_one() {
        let src = "function outer() { function inner() { return 1; } return inner; }";
        let kept = skim_kinds(src, 1);
        assert!(kept.contains(&"inner".to_string()));
        assert_eq!(kept.iter().filter(|k| k.starts_with("skim:")).count(), 1, "{kept:?}");
        let all = skim_kinds(src, 0);
        assert_eq!(all.iter().filter(|k| k.starts_with("skim:")).count(), 1, "{all:?}");
        assert!(!all.contains(&"inner".to_string()));
    }

    #[test]
    fn skim_ignores_property_named_function_and_regex_after_body() {
        let kinds = skim_kinds("a.function(1); if (b) { c(); }\nfunction f() {}\n/re/.test(d);", 0);
        assert_eq!(kinds.iter().filter(|k| k.starts_with("skim:")).count(), 1, "{kinds:?}");
        assert!(kinds.contains(&"c".to_string()));
    }

    fn describe(t: &Token) -> (String, u32, bool) {
        let text = match &t.kind {
            Tok::Punct(p) => p.as_str().to_string(),
            Tok::Keyword(k) => k.as_str().to_string(),
            Tok::Ident(n) => n.to_string(),
            Tok::Eof => "eof".to_string(),
            other => format!("{other:?}"),
        };
        (text, t.line(), t.nl_before())
    }

    /// The tokens from the first body's closing `}` on, lexed with and without skimming; asserts
    /// the byte scan (not the token-by-token fallback) skipped `skipped` bodies.
    fn tails_after_first_body(src: &str, skipped: usize) -> (Vec<(String, u32, bool)>, Vec<(String, u32, bool)>) {
        SKIPPED_BODIES.with(|n| n.set(0));
        let opts = LexOpts {
            skim_min_depth: Some(0),
            ..LexOpts::default()
        };
        let skimmed = tokenize_opts(src, true, 1, opts).unwrap().tokens;
        assert_eq!(SKIPPED_BODIES.with(|n| n.get()), skipped, "{src:?}");
        let skim = skimmed.iter_from(0).find(|t| matches!(t.kind, Tok::Skim)).expect("a skim");
        assert_eq!(skim.line(), 1);
        let from = skim.end - 1;
        let tail = |toks: &TokVec| -> Vec<_> {
            toks.iter_from(0).filter(|t| t.start >= from).map(describe).collect()
        };
        let full = tokenize_goal(src, true).unwrap();
        (tail(&skimmed), tail(&full))
    }

    fn assert_skips_like_full_lex(body: &str) {
        let src = format!("function f() {{{body}}}\nafter(1);\n/re/.test(2);\n");
        let (skimmed, full) = tails_after_first_body(&src, 1);
        assert_eq!(skimmed, full, "{body:?}");
        assert!(skimmed.len() > 8, "{skimmed:?}");
    }

    #[test]
    fn skipper_handles_regexes_with_braces_and_slashes() {
        assert_skips_like_full_lex(" var r = /[/}]+\\}\\//g; var s = a / b / c; ");
        assert_skips_like_full_lex(" var r = /[\\]}]/; ");
        assert_skips_like_full_lex(" x = a ? /}/ : 1; ");
    }

    #[test]
    fn skipper_tells_division_from_regex() {
        assert_skips_like_full_lex(" var d = (a) / 2 / g; var e = x / y; ");
        assert_skips_like_full_lex(" if (a) /}/.test(b); while (c) /{/.exec(d); ");
        assert_skips_like_full_lex(" return /}/.test(a); ");
        assert_skips_like_full_lex(" var v = function () { return 1 } / 2 / 3; ");
        assert_skips_like_full_lex(" function g() {} /}/.test(1); ");
        assert_skips_like_full_lex(" var o = {} / 2; if (z) {} /}/.test(1); ");
        assert_skips_like_full_lex(" a = b[0] / 2 / c; x = typeof /}/; y = this / 2; ");
        assert_skips_like_full_lex(" for await (x of y) /}/.test(x); ");
    }

    #[test]
    fn skipper_handles_templates_with_nested_braces() {
        assert_skips_like_full_lex(" var t = `a${ {a: 1}.a + \"}\" }b${`${ '}' }`}`; ");
        assert_skips_like_full_lex(" var t = `\\${ } \\` ${ /}/.source } `; ");
        assert_skips_like_full_lex(" var t = `${ function () { return `}` }() }`; ");
    }

    #[test]
    fn skipper_handles_comments_and_strings() {
        assert_skips_like_full_lex(" // }\n/* } { \n */ x(); // {\n");
        assert_skips_like_full_lex(" var q = \"\\\"}\" + '\\'{' + \"\\\\\"; ");
        assert_skips_like_full_lex(" var q = \"a\\\nb}\"; ");
        assert_skips_like_full_lex(" /** doc } */ x(); ");
    }

    #[test]
    fn skipper_counts_lines_like_the_lexer() {
        assert_skips_like_full_lex(" a();\r\nb();\r\n/* x\r\n y */ c(); ");
        assert_skips_like_full_lex(" a();\u{2028}b();\u{2029}// c\u{2028}d(); \"e\u{2028}f\"; ");
        assert_skips_like_full_lex(" a();\rb();\r");
        assert_skips_like_full_lex("\n a();\n\n`x\ny\n${\n1\n}`;\n");
        assert_skips_like_full_lex(" a();\n");
        assert_skips_like_full_lex(" a(); // c\n");
        assert_skips_like_full_lex(" a(); /* c */ ");
    }

    #[test]
    fn skipper_falls_back_on_what_it_cannot_vouch_for() {
        for body in [" var é = 1; ", " x = 1; <!-- c\n y(); ", " \u{a0} a(); "] {
            let src = format!("function f() {{{body}}}\nafter(1);\n");
            let (skimmed, full) = tails_after_first_body(&src, 0);
            assert_eq!(skimmed, full, "{body:?}");
        }
    }

    #[test]
    fn lex_error_in_a_skimmed_body_is_the_same() {
        for body in [" var s = \"abc;\n x(); ", " x = /abc\n/; ", " x = `abc ", " /* "] {
            let src = format!("function f() {{{body}}}\n");
            let opts = LexOpts {
                skim_min_depth: Some(0),
                ..LexOpts::default()
            };
            let skimmed = tokenize_opts(&src, true, 1, opts).err().expect("error");
            let full = tokenize_goal(&src, true).err().expect("error");
            assert_eq!((skimmed.message, skimmed.line, skimmed.at_eof), (full.message, full.line, full.at_eof), "{body:?}");
        }
    }

    #[test]
    fn skipper_regex_after_keywords_and_control_headers() {
        assert_skips_like_full_lex(" throw /}/; typeof /{/; void /}/; delete o[/}/]; x = a in /}/; ");
        assert_skips_like_full_lex(" if (a) x(); else /}/.test(1); do /{/.test(1); while (0); ");
        assert_skips_like_full_lex(" switch (a) { case /}/.source: break; default: /{/.test(a); } ");
        assert_skips_like_full_lex(" x = f(a) / 2; y = (a + b) / 2 / c; z = a ? b : /}/; ");
        assert_skips_like_full_lex(" for (;;) /}/.test(a); with (o) /{/.test(a); ");
        assert_skips_like_full_lex(" z = [1] / 2; w = a.b / 2 / c; ");
    }

    #[test]
    fn skipper_ignores_brackets_and_quotes_inside_comments() {
        assert_skips_like_full_lex(" /* ' \" ` { ( [ */ x(); // \" ' ` } ) ]\n y(); ");
        assert_skips_like_full_lex(" x = 1; /* } */ /* { */ y = 2; //}\n//{\nz = 3; ");
        assert_skips_like_full_lex(" /**/ x(); /***/ y(); /*/ } /*/ z(); ");
    }

    #[test]
    fn skipper_handles_script_end_tag_lookalikes() {
        assert_skips_like_full_lex(" var s = '</script>'; var t = `</script>${1}</script>`; var r = /<\\/script>/; ");
        assert_skips_like_full_lex(" x = a < b > c; y = a </ 2 /; ");
        assert_skips_like_full_lex(" x = '<!--'; y = `-->`; z = /-->/; ");
    }

    #[test]
    fn skipper_html_like_comments_follow_the_lexer() {
        assert_skips_like_full_lex(" x = a-->b; y = a --> b; ");
        let (skimmed, full) = tails_after_first_body("function f() { x();\n--> c }\n y(); }\nafter(1);\n", 0);
        assert_eq!(skimmed, full);
        let (skimmed, full) = tails_after_first_body("function f() { x = 1; <!-- }\n y(); }\nafter(1);\n", 0);
        assert_eq!(skimmed, full);
    }

    #[test]
    fn skipper_falls_back_on_escaped_identifiers() {
        for body in [" var \\u0061b = 1; ", " x = a\\u0062c; ", " x = \\u{62}c; ", " o.\\u0061 = 1; "] {
            let src = format!("function f() {{{body}}}\nafter(1);\n");
            let (skimmed, full) = tails_after_first_body(&src, 0);
            assert_eq!(skimmed, full, "{body:?}");
        }
        assert_skips_like_full_lex(" var s = '\\u0061'; var t = `\\u{62}`; var r = /\\u0063/; ");
    }

    #[test]
    fn skipper_handles_deeply_nested_templates() {
        assert_skips_like_full_lex(" `a${`b${`c${ {x: '}'}.x }${`d`}`}`}` + `${ [1, 2].map(function (v) { return `${v}`; }) }`; ");
        assert_skips_like_full_lex(" `${ a / 2 }` / 3; `${ /}/.test(1) }`; `$` + `$$`+ `\\${}`; ");
    }

    #[test]
    fn skipper_handles_classes_private_names_and_generators() {
        assert_skips_like_full_lex(" class A { #x = 1; m() { return this.#x / 2; } static { y(); } } /}/.test(1); ");
        assert_skips_like_full_lex(" var A = class { get x() { return 1; } }; var d = A / 2; ");
        assert_skips_like_full_lex(" function* g() { yield /}/; } /}/.test(g); ");
        assert_skips_like_full_lex(" x = async () => { await /}/; }; y = a => a / 2; ");
    }

    fn describe_full(t: &Token) -> (String, u32, bool, u32, u32) {
        let (text, line, nl) = describe(t);
        (text, line, nl, t.start, t.end)
    }

    /// Lex `src`'s outermost body with skim hints from a lex of the whole `src`, and without;
    /// the tokens must agree, and `hits` bodies must have been jumped over by a hint.
    fn assert_hints_replay(src: &str, hits: usize) {
        let open = src.find('{').unwrap();
        let body = &src[open..=src.rfind('}').unwrap()];
        let opts = LexOpts {
            skim_min_depth: Some(0),
            record_hints: true,
            ..LexOpts::default()
        };
        let hints = tokenize_opts(src, true, 1, opts).unwrap().skim_hints;
        assert!(hints.windows(2).all(|w| w[0].0 < w[1].0), "{hints:?}");
        let line = 1 + src[..open].matches('\n').count() as u32;
        let relex = |scratch: &mut LexScratch| {
            let opts = LexOpts {
                skim_min_depth: Some(0),
                offset: open as u32,
                ..LexOpts::default()
            };
            tokenize_pieces_with(&[body], true, line, opts, scratch).unwrap().tokens
        };
        HINTED_BODIES.with(|n| n.set(0));
        let plain = relex(&mut LexScratch::default());
        assert_eq!(HINTED_BODIES.with(|n| n.get()), 0);
        let hinted = relex(&mut LexScratch::with_hints(hints));
        assert_eq!(HINTED_BODIES.with(|n| n.get()), hits, "{src:?}");
        let tokens = |toks: &TokVec| -> Vec<_> { toks.iter_from(0).map(describe_full).collect() };
        assert_eq!(tokens(&hinted), tokens(&plain), "{src:?}");
    }

    fn pad() -> String {
        format!("/*{}*/", " ".repeat(HINT_MIN_BYTES as usize))
    }

    #[test]
    fn skim_hints_replay_nested_bodies() {
        let pad = pad();
        assert_hints_replay(
            &format!("function o() {{ function a(x) {{ {pad} return x / 2; }}\n function b() {{ {pad} }} a(1) }}"),
            2,
        );
        assert_hints_replay(
            &format!("function o() {{ var f = function* g() {{ {pad} yield /}}/; }}; var h = function () {{ {pad} \n}} }}"),
            2,
        );
        assert_hints_replay(
            &format!("function o() {{ if (a) {{ b(); }} for (;;) {{ c(); }} var x = function y(a, b) {{ {pad} if (a) {{ return `${{ function () {{ {pad} }} }}`; }} }} }}"),
            1,
        );
        assert_hints_replay(
            &format!("function o() {{ var k = {{ get a() {{ {pad} }}, b() {{ {pad} }}, function() {{ {pad} }} }}; var l = (m) => {{ {pad} }}; }}"),
            1,
        );
        assert_hints_replay(
            &format!("function o() {{ x = function () {{ {pad} }}\n/ 2 / 3; y = function () {{ {pad} }} / 2; }}"),
            2,
        );
    }

    #[test]
    fn skim_hints_skip_small_and_unbalanced_bodies() {
        let opts = LexOpts {
            skim_min_depth: Some(0),
            record_hints: true,
            ..LexOpts::default()
        };
        let src = "function o() { function small() { return 1; } var t = function () { ( }; ) }";
        let hints = tokenize_opts(src, true, 1, opts).unwrap().skim_hints;
        assert!(hints.is_empty(), "{hints:?}");
        let src = format!("function o() {{ function big() {{ {} (; }} ; )}} }}", pad());
        let hints = tokenize_opts(&src, true, 1, opts).map(|l| l.skim_hints).unwrap_or_default();
        assert!(hints.is_empty(), "{hints:?}");
    }

    #[test]
    fn skim_hints_record_body_end_and_line_break() {
        let opts = LexOpts {
            skim_min_depth: Some(0),
            record_hints: true,
            ..LexOpts::default()
        };
        let src = format!("function o() {{ function a() {{ {}\n}} function b() {{ {} }} }}", pad(), pad());
        let hints = tokenize_opts(&src, true, 1, opts).unwrap().skim_hints;
        assert_eq!(hints.len(), 2);
        let close_a = src.find("\n}").unwrap() + 1;
        assert_eq!(hints[0], (src.find("{ /*").unwrap() as u32 + 0, (close_a as u32) << 1 | 1));
        let close_b = src.rfind("} }").unwrap();
        assert_eq!(hints[1].1, (close_b as u32) << 1);
    }

    fn num_values(src: &str) -> Vec<f64> {
        tokenize(src)
            .unwrap()
            .iter()
            .filter_map(|t| match t.kind {
                Tok::Num(n) => Some(n.get()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_integer_literals_take_the_fast_path_and_agree_with_the_slow_one() {
        assert_eq!(num_values("0 1 42 999999999999999 1000000000000000"), [0.0, 1.0, 42.0, 999999999999999.0, 1e15]);
        assert_eq!(num_values("9007199254740993 12345678901234567890"), [9007199254740992.0, 12345678901234567890.0]);
        assert_eq!(num_values("1.5 .5 1e3 1E-2 0x1F 0b11 0o17 1_000 010 08 1.e2"), [1.5, 0.5, 1000.0, 0.01, 31.0, 3.0, 15.0, 1000.0, 8.0, 8.0, 100.0]);
        assert_eq!(num_values("a[1]+2;3,4)5"), [1.0, 2.0, 3.0, 4.0, 5.0]);
        for bad in ["12abc", "1$", "0_1", "1\\u0061", "3in x", "5é"] {
            assert!(tokenize(bad).is_err(), "{bad:?}");
        }
        assert!(tokenize("1n 0n 12345678901234567890n").is_ok());
        assert!(tokenize("1 in x").is_ok());
    }

    fn str_values(src: &str) -> Vec<String> {
        tokenize(src)
            .unwrap()
            .iter()
            .filter_map(|t| match &t.kind {
                Tok::Str(s) => Some(s.to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn strings_with_escapes_copy_plain_runs_between_them() {
        assert_eq!(
            str_values(r#" "ab\ncd" 'x\'y' "\u0041\u{1F600}z" "é\tü" "a\
b" "\ud83d\ude00!" "\x41\101" "tail\\" "#),
            ["ab\ncd", "x'y", "A\u{1F600}z", "é\tü", "ab", "\u{1F600}!", "AA", "tail\\"]
        );
        assert_eq!(str_values("'\u{2028}a\\nb\u{2029}'"), ["\u{2028}a\nb\u{2029}"]);
        let long = "x".repeat(200);
        assert_eq!(str_values(&format!("\"{long}\\n{long}\"")), [format!("{long}\n{long}")]);
        for bad in ["\"abc\\u12\"", "\"abc\\u12zz\"", "'a\nb'", "'abc\\", "\"\\u{110000}\""] {
            assert!(tokenize(bad).is_err(), "{bad:?}");
        }
        let toks = tokenize("'a\\\nb'\nx").unwrap();
        assert_eq!(toks[1].line(), 3);
    }

    #[test]
    fn keyword_table_matches_the_word_list() {
        for k in KEYWORDS {
            for near in [format!("{k}x"), k[..k.len() - 1].to_string(), k.to_uppercase(), format!("_{k}")] {
                assert!(keyword(&near).is_none() || KEYWORDS.contains(&near.as_str()), "{near}");
            }
        }
        for word in ["a", "z", "of", "async", "let", "static", "yield", "await", "get", "set"] {
            assert_eq!(keyword(word), None, "{word}");
        }
    }

    #[test]
    fn punct_len_is_the_spelling_length() {
        for p in PUNCTUATORS {
            let toks = tokenize(&format!("x {p} ")).unwrap();
            let Tok::Punct(q) = toks[1].kind else { panic!("{p}") };
            assert_eq!(q.len(), q.as_str().len(), "{p}");
        }
    }

    #[test]
    fn dot_minus_and_angle_tokens_dispatch_like_the_general_path() {
        let kinds = |src: &str, html: bool| -> Vec<String> {
            tokenize_goal(src, html)
                .unwrap()
                .iter_from(0)
                .map(|t| describe(t).0)
                .collect()
        };
        assert_eq!(kinds("a.b ...c .5 a--b a-b a<b a<=b", true), ["a", ".", "b", "...", "c", "Num(0.5)", "a", "--", "b", "a", "-", "b", "a", "<", "b", "a", "<=", "b", "eof"]);
        assert_eq!(kinds("a<!--b\nc", true), ["a", "c", "eof"]);
        assert_eq!(kinds("a\n--> b\nc", true), ["a", "c", "eof"]);
        assert_eq!(kinds("a<!--b", false), ["a", "<", "!", "--", "b", "eof"]);
        assert_eq!(kinds("a\n--> b", false), ["a", "--", ">", "b", "eof"]);
    }
}
