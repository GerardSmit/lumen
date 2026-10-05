//! The parser: input decoding, the prolog / DTD / content / epilog processors, namespaces,
//! entities and the amplification limit.

use super::chars::*;
use super::dtd::*;
use super::scan::{self, Ref, Scan};
use super::{err, Account, Attribute, DefaultMode, Flow, Handler, Options, Position, Shared, XmlError};
use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS_NS: &str = "http://www.w3.org/2000/xmlns/";
const MAX_NESTING: usize = 1000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParserKind {
    /// A whole document.
    Document,
    /// The text of an external general entity (parsed as content).
    ExternalEntity,
    /// An external DTD subset or external parameter entity.
    ExternalSubset,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Prolog,
    AfterDoctype,
    Subset,
    Content,
    Epilog,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DeclState {
    Unknown,
    Pending,
    Done,
}

enum Step {
    Go,
    Stuck,
}

struct Tag {
    raw: String,
    out: String,
    declared: Vec<Option<String>>,
    ns_mark: usize,
}

struct Doctype {
    sysid: Option<String>,
    pubid: Option<String>,
}

type Res<T> = Result<T, u32>;

macro_rules! emit {
    ($h:ident, $raw:expr, $call:expr) => {{
        match $call {
            Flow::Abort => return Err(err::ABORTED),
            Flow::Unset => {
                if $h.default_mode() != DefaultMode::None && $h.default_text($raw) == Flow::Abort {
                    return Err(err::ABORTED);
                }
            }
            _ => {}
        }
    }};
}

macro_rules! quiet {
    ($call:expr) => {{
        if $call == Flow::Abort {
            return Err(err::ABORTED);
        }
    }};
}

fn off(t: &str, sub: &str) -> usize {
    (sub.as_ptr() as usize).wrapping_sub(t.as_ptr() as usize)
}

fn predefined(name: &str) -> Option<char> {
    match name {
        "lt" => Some('<'),
        "gt" => Some('>'),
        "amp" => Some('&'),
        "apos" => Some('\''),
        "quot" => Some('"'),
        _ => None,
    }
}

/// `\r\n` and lone `\r` become `\n`.
fn normalize_newlines(s: &str) -> String {
    if !s.contains('\r') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\r' {
            if it.peek() == Some(&'\n') {
                it.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

pub struct Parser {
    shared: Rc<Shared>,
    kind: ParserKind,
    phase: Phase,
    raw: Vec<u8>,
    rpos: usize,
    enc: Enc,
    enc_decided: bool,
    enc_override: bool,
    enc_name: Option<String>,
    decl: DeclState,
    resolved: bool,
    partial_char: bool,
    at_start: bool,
    buf: String,
    line: u64,
    col: u64,
    byte: u64,
    after_cr: bool,
    tags: Vec<Tag>,
    tag_floor: usize,
    ns: Vec<(String, String)>,
    in_cdata: bool,
    cond_depth: usize,
    seen_doctype: bool,
    doctype: Option<Doctype>,
    nested: usize,
    err_off: Option<usize>,
    error: Option<u32>,
    finished: bool,
    first_chunk: bool,
}

impl Parser {
    /// A parser for a whole document.
    pub fn new(opts: Options) -> Parser {
        let shared = Shared::new(Rc::new(RefCell::new(Dtd::new())), Rc::new(Account::new()), opts, false);
        Parser::build(Rc::new(shared), ParserKind::Document)
    }

    /// `XML_ExternalEntityParserCreate`: a parser for an external entity of `parent`, sharing its
    /// DTD. A `context` (from [`Handler::external_entity_ref`]) makes it parse a general entity; none
    /// makes it parse an external DTD subset or parameter entity.
    pub fn new_external(parent: &Rc<Shared>, context: Option<&str>, encoding: Option<&str>) -> Parser {
        let mut opts = parent.opts.clone();
        opts.encoding = encoding.map(str::to_string).or(None);
        let shared = Shared::new(parent.dtd.clone(), parent.account.clone(), opts, true);
        shared.base.replace(parent.base.borrow().clone());
        shared.param_entity_parsing.set(parent.param_entity_parsing.get());
        let kind = if context.is_some() { ParserKind::ExternalEntity } else { ParserKind::ExternalSubset };
        let mut p = Parser::build(Rc::new(shared), kind);
        if let Some(ctx) = context {
            for item in ctx.split('\x0C') {
                if let Some((prefix, uri)) = item.split_once('=') {
                    p.ns.push((prefix.to_string(), uri.to_string()));
                }
            }
        }
        p
    }

    fn build(shared: Rc<Shared>, kind: ParserKind) -> Parser {
        let enc_name = shared.opts.encoding.clone();
        let over = enc_name.is_some();
        let (enc, decided) = (Enc::Utf8, false);
        Parser {
            shared,
            kind,
            phase: match kind {
                ParserKind::Document => Phase::Prolog,
                ParserKind::ExternalEntity => Phase::Content,
                ParserKind::ExternalSubset => Phase::Subset,
            },
            raw: Vec::new(),
            rpos: 0,
            enc,
            enc_decided: decided,
            enc_override: over,
            enc_name,
            decl: DeclState::Unknown,
            resolved: false,
            partial_char: false,
            at_start: true,
            buf: String::new(),
            line: 1,
            col: 0,
            byte: 0,
            after_cr: false,
            tags: Vec::new(),
            tag_floor: 0,
            ns: Vec::new(),
            in_cdata: false,
            cond_depth: 0,
            seen_doctype: false,
            doctype: None,
            nested: 0,
            err_off: None,
            error: None,
            finished: false,
            first_chunk: true,
        }
    }

    pub fn shared(&self) -> Rc<Shared> {
        self.shared.clone()
    }

    pub fn kind(&self) -> ParserKind {
        self.kind
    }

    /// `XML_SetEncoding`: overrides the input's encoding; refused once parsing has begun.
    pub fn set_encoding(&mut self, name: &str) -> bool {
        if self.shared.started.get() {
            return false;
        }
        self.enc_name = Some(name.to_string());
        self.enc_override = true;
        true
    }

    /// Feeds `data`; `is_final` marks the end of the input. After an error every later call
    /// reports the same error; after a successful final call, `FINISHED`.
    pub fn parse(&mut self, h: &mut dyn Handler, data: &[u8], is_final: bool) -> Result<(), XmlError> {
        if let Some(code) = self.error {
            return Err(XmlError { code });
        }
        if self.finished {
            return Err(XmlError { code: err::FINISHED });
        }
        self.shared.started.set(true);
        if self.first_chunk {
            self.first_chunk = false;
            if self.kind == ParserKind::ExternalSubset {
                self.shared.dtd.borrow_mut().param_entity_read = true;
            }
        }
        self.raw.extend_from_slice(data);
        match self.parse_inner(h, is_final) {
            Ok(()) => {
                if is_final {
                    self.finished = true;
                }
                Ok(())
            }
            Err(code) => {
                self.error = Some(code);
                self.shared.error_code.set(code);
                self.set_pos();
                self.shared.error_pos.set(self.shared.pos.get());
                Err(XmlError { code })
            }
        }
    }

    fn parse_inner(&mut self, h: &mut dyn Handler, fin: bool) -> Res<()> {
        loop {
            let grew = self.decode(h, fin)?;
            self.tokenize(h, fin)?;
            if self.resolved {
                self.resolved = false;
                continue;
            }
            if self.decl == DeclState::Pending && grew {
                continue;
            }
            break;
        }
        Ok(())
    }

    fn set_pos(&self) {
        self.shared.pos.set(Position { line: self.line, column: self.col, byte: self.byte });
    }

    /// Moves the position over `s`.
    fn advance(&mut self, s: &str) {
        if self.nested > 0 {
            return;
        }
        match self.kind {
            ParserKind::Document => self.shared.account.direct.set(self.shared.account.direct.get() + s.len() as u64),
            _ => self.shared.account.indirect.set(self.shared.account.indirect.get() + s.len() as u64),
        }
        for c in s.chars() {
            self.byte += self.enc.width(c);
            match c {
                '\n' => {
                    if !self.after_cr {
                        self.line += 1;
                    }
                    self.col = 0;
                    self.after_cr = false;
                }
                '\r' => {
                    self.line += 1;
                    self.col = 0;
                    self.after_cr = true;
                }
                _ => {
                    self.col += 1;
                    self.after_cr = false;
                }
            }
        }
    }

    fn consume(&mut self, t: &str, i: &mut usize, end: usize) {
        self.advance(&t[*i..end]);
        *i = end;
        self.at_start = false;
    }

    fn bad<T>(&mut self, code: u32, at: usize) -> Res<T> {
        if self.nested == 0 {
            self.err_off = Some(at);
        }
        Err(code)
    }

    fn need(&mut self, fin: bool) -> Res<Step> {
        if self.nested == 0 && (!fin || (self.decl == DeclState::Pending && self.rpos < self.raw.len())) {
            return Ok(Step::Stuck);
        }
        Err(if self.partial_char {
            err::PARTIAL_CHAR
        } else if self.in_cdata {
            err::UNCLOSED_CDATA_SECTION
        } else {
            err::UNCLOSED_TOKEN
        })
    }

    fn account_indirect(&mut self, n: usize) -> Res<()> {
        let a = &self.shared.account;
        a.indirect.set(a.indirect.get().saturating_add(n as u64));
        let direct = a.direct.get();
        let indirect = a.indirect.get();
        let total = direct.saturating_add(indirect);
        let amp = if direct > 0 { total as f32 / direct as f32 } else { (22 + indirect) as f32 / 22.0 };
        if total >= a.activation_threshold.get() && amp > a.max_amplification.get() {
            return Err(err::AMPLIFICATION_LIMIT_BREACH);
        }
        Ok(())
    }

    // ---- input decoding -------------------------------------------------------------------

    fn push_one(&mut self) -> bool {
        match next_char(&self.enc, &self.raw[self.rpos..]) {
            Step2::Char(c, n) => {
                self.buf.push(c);
                self.rpos += n;
                true
            }
            Step2::Bad(n) => {
                self.buf.push(MALFORMED);
                self.rpos += n;
                true
            }
            Step2::Incomplete => false,
        }
    }

    fn set_encoding_by_name(&mut self, h: &mut dyn Handler, name: &str) -> Res<Enc> {
        match Enc::from_name(name) {
            Some(e) => Ok(e),
            None => match h.unknown_encoding(name) {
                Some(table) if table.len() >= 256 => Ok(Enc::Table(table)),
                _ => Err(err::UNKNOWN_ENCODING),
            },
        }
    }

    fn detect(&mut self, h: &mut dyn Handler, fin: bool) -> Res<bool> {
        let head: Vec<u8> = self.raw[self.rpos..].iter().take(4).copied().collect();
        let r = &head[..];
        if r.is_empty() {
            if fin {
                if self.enc_override {
                    if let Some(n) = self.enc_name.clone() {
                        self.enc = self.set_encoding_by_name(h, &n)?;
                    }
                }
                self.enc_decided = true;
                return Ok(true);
            }
            return Ok(false);
        }
        if self.enc_override {
            if let Some(n) = self.enc_name.clone() {
                self.enc = self.set_encoding_by_name(h, &n)?;
            }
            let skip = match (&self.enc, r) {
                (Enc::Utf8, [0xEF, 0xBB, 0xBF, ..]) => 3,
                (Enc::Utf16Be, [0xFE, 0xFF, ..]) | (Enc::Utf16Le, [0xFF, 0xFE, ..]) => 2,
                _ => 0,
            };
            self.rpos += skip;
            self.byte += skip as u64;
            self.enc_decided = true;
            return Ok(true);
        }
        let (enc, skip) = match r {
            [0xEF, 0xBB, 0xBF, ..] => (Enc::Utf8, 3),
            [0xFE, 0xFF, ..] => (Enc::Utf16Be, 2),
            [0xFF, 0xFE, ..] => (Enc::Utf16Le, 2),
            [0x00, 0x3C, ..] => (Enc::Utf16Be, 0),
            [0x3C, 0x00, ..] => (Enc::Utf16Le, 0),
            [0xEF] | [0xEF, 0xBB] | [0xFE] | [0xFF] | [0x00] | [0x3C] if !fin => return Ok(false),
            _ => (Enc::Utf8, 0),
        };
        self.enc = enc;
        self.rpos += skip;
        self.byte += skip as u64;
        self.enc_decided = true;
        Ok(true)
    }

    /// Decodes input into `buf`; returns whether anything was added.
    fn decode(&mut self, h: &mut dyn Handler, fin: bool) -> Res<bool> {
        if !self.enc_decided && !self.detect(h, fin)? {
            return Ok(false);
        }
        let mut grew = false;
        if self.decl == DeclState::Unknown {
            while self.buf.chars().count() < 6 {
                if !self.push_one() {
                    break;
                }
                grew = true;
            }
            if self.buf.chars().count() < 6 {
                if !fin {
                    return Ok(grew);
                }
                self.decl = DeclState::Done;
            } else {
                let b = self.buf.as_bytes();
                self.decl = if b.starts_with(b"<?xml") && matches!(b[5], b' ' | b'\t' | b'\r' | b'\n') {
                    DeclState::Pending
                } else {
                    DeclState::Done
                };
            }
        }
        if self.decl == DeclState::Pending {
            while self.push_one() {
                grew = true;
                if self.buf.ends_with('>') {
                    break;
                }
            }
            self.compact();
            return Ok(grew);
        }
        let used = decode_into(&self.enc, &self.raw[self.rpos..], &mut self.buf);
        self.rpos += used;
        grew |= used > 0;
        self.partial_char = fin && self.rpos < self.raw.len();
        self.compact();
        Ok(grew)
    }

    fn compact(&mut self) {
        if self.rpos > 0 {
            self.raw.drain(..self.rpos);
            self.rpos = 0;
        }
    }

    // ---- the token loop -------------------------------------------------------------------

    fn tokenize(&mut self, h: &mut dyn Handler, fin: bool) -> Res<()> {
        let t = std::mem::take(&mut self.buf);
        let mut i = 0usize;
        let r = self.run(h, &t, &mut i, fin);
        if r.is_err() {
            if let Some(off) = self.err_off.take() {
                if off >= i && off <= t.len() {
                    self.advance(&t[i..off]);
                    i = off;
                }
            }
        }
        self.buf = t[i..].to_string();
        r
    }

    fn run(&mut self, h: &mut dyn Handler, t: &str, i: &mut usize, fin: bool) -> Res<()> {
        while *i < t.len() {
            let step = match self.phase {
                Phase::Prolog | Phase::AfterDoctype => self.prolog_step(h, t, i, fin)?,
                Phase::Subset => self.subset_step(h, t, i, fin, false)?,
                Phase::Content => self.content_step(h, t, i, fin)?,
                Phase::Epilog => self.epilog_step(h, t, i, fin)?,
            };
            if let Step::Stuck = step {
                return Ok(());
            }
        }
        if fin && self.decl == DeclState::Done && (self.rpos >= self.raw.len() || self.partial_char) {
            return self.at_end();
        }
        Ok(())
    }

    fn at_end(&mut self) -> Res<()> {
        if self.partial_char {
            return Err(err::PARTIAL_CHAR);
        }
        if self.in_cdata {
            return Err(err::UNCLOSED_CDATA_SECTION);
        }
        match self.kind {
            ParserKind::Document => match self.phase {
                Phase::Epilog => Ok(()),
                _ => Err(err::NO_ELEMENTS),
            },
            ParserKind::ExternalEntity => {
                if self.tags.is_empty() {
                    Ok(())
                } else {
                    Err(err::ASYNC_ENTITY)
                }
            }
            ParserKind::ExternalSubset => {
                if self.cond_depth > 0 {
                    Err(err::INCOMPLETE_PE)
                } else {
                    Ok(())
                }
            }
        }
    }

    fn dflt(&mut self, h: &mut dyn Handler, raw: &str) -> Res<()> {
        if h.default_mode() != DefaultMode::None && h.default_text(raw) == Flow::Abort {
            return Err(err::ABORTED);
        }
        Ok(())
    }

    // ---- prolog and epilog ----------------------------------------------------------------

    fn prolog_step(&mut self, h: &mut dyn Handler, t: &str, i: &mut usize, fin: bool) -> Res<Step> {
        let start = *i;
        let rest = &t[start..];
        let c = rest.chars().next().unwrap_or(' ');
        if is_space(c) {
            let e = scan::skip_space(t, start);
            self.set_pos();
            self.dflt(h, &t[start..e])?;
            self.consume(t, i, e);
            return Ok(Step::Go);
        }
        if c != '<' {
            return if !is_xml_char(c) { self.bad(err::INVALID_TOKEN, start) } else { self.bad(err::SYNTAX, start) };
        }
        match rest[1..].chars().next() {
            None => self.need(fin),
            Some('?') => match scan::pi(t, start) {
                Scan::Tok(pi, e) => {
                    self.pi_token(h, t, i, &pi, e)?;
                    Ok(Step::Go)
                }
                Scan::Need => self.need(fin),
                Scan::Bad(p) => self.bad(err::INVALID_TOKEN, p),
            },
            Some('!') => {
                if rest.starts_with("<!--") {
                    return self.comment_token(h, t, i, fin);
                }
                if rest.starts_with("<!DOCTYPE") {
                    if self.phase != Phase::Prolog {
                        return self.bad(err::SYNTAX, start);
                    }
                    return match scan::doctype_head(t, start) {
                        Scan::Tok(head, e) => {
                            self.start_doctype(h, t, start, &head)?;
                            self.consume(t, i, e);
                            if !head.has_subset {
                                self.finish_doctype(h)?;
                            }
                            Ok(Step::Go)
                        }
                        Scan::Need => self.need(fin),
                        Scan::Bad(p) => self.bad(err::INVALID_TOKEN, p),
                    };
                }
                if scan::partial_prefix("<!--", rest) || scan::partial_prefix("<!DOCTYPE", rest) {
                    return self.need(fin);
                }
                if ["<!ELEMENT", "<!ATTLIST", "<!ENTITY", "<!NOTATION"].iter().any(|k| rest.starts_with(k)) {
                    return self.bad(err::SYNTAX, start);
                }
                self.bad(err::INVALID_TOKEN, start + 2)
            }
            Some('/') => self.bad(err::SYNTAX, start),
            Some(c2) if is_name_start(c2) => {
                self.root_start(h)?;
                self.content_step(h, t, i, fin)
            }
            Some(_) => self.bad(err::INVALID_TOKEN, start + 1),
        }
    }

    fn epilog_step(&mut self, h: &mut dyn Handler, t: &str, i: &mut usize, fin: bool) -> Res<Step> {
        let start = *i;
        let rest = &t[start..];
        let c = rest.chars().next().unwrap_or(' ');
        if is_space(c) {
            let e = scan::skip_space(t, start);
            self.set_pos();
            self.dflt(h, &t[start..e])?;
            self.consume(t, i, e);
            return Ok(Step::Go);
        }
        if c == '<' {
            match rest[1..].chars().next() {
                None => return self.need(fin),
                Some('?') => {
                    return match scan::pi(t, start) {
                        Scan::Tok(pi, e) => {
                            self.pi_token(h, t, i, &pi, e)?;
                            Ok(Step::Go)
                        }
                        Scan::Need => self.need(fin),
                        Scan::Bad(p) => self.bad(err::INVALID_TOKEN, p),
                    };
                }
                Some('!') => {
                    if rest.starts_with("<!--") {
                        return self.comment_token(h, t, i, fin);
                    }
                    if scan::partial_prefix("<!--", rest) {
                        return self.need(fin);
                    }
                    return self.bad(err::INVALID_TOKEN, start + 2);
                }
                Some(c2) if c2 == '/' || is_name_start(c2) => {}
                Some(_) => return self.bad(err::INVALID_TOKEN, start + 1),
            }
        } else if !is_xml_char(c) {
            return self.bad(err::INVALID_TOKEN, start);
        }
        self.bad(err::JUNK_AFTER_DOC_ELEMENT, start)
    }

    fn comment_token(&mut self, h: &mut dyn Handler, t: &str, i: &mut usize, fin: bool) -> Res<Step> {
        let start = *i;
        match scan::comment(t, start) {
            Scan::Tok(body, e) => {
                self.set_pos();
                let text = normalize_newlines(body);
                emit!(h, &t[start..e], h.comment(&text));
                self.consume(t, i, e);
                Ok(Step::Go)
            }
            Scan::Need => self.need(fin),
            Scan::Bad(p) => self.bad(err::INVALID_TOKEN, p),
        }
    }

    fn pi_token(&mut self, h: &mut dyn Handler, t: &str, i: &mut usize, pi: &scan::Pi, e: usize) -> Res<()> {
        let start = *i;
        if pi.target.eq_ignore_ascii_case("xml") {
            if pi.target == "xml" && self.at_start && self.decl == DeclState::Pending && self.nested == 0 {
                self.set_pos();
                self.process_xml_decl(h, &t[start..e])?;
                self.consume(t, i, e);
                self.decl = DeclState::Done;
                self.resolved = true;
                return Ok(());
            }
            return self.bad(err::MISPLACED_XML_PI, start);
        }
        self.set_pos();
        let data = normalize_newlines(pi.data);
        emit!(h, &t[start..e], h.processing_instruction(pi.target, &data));
        self.consume(t, i, e);
        Ok(())
    }

    // ---- XML and text declarations --------------------------------------------------------

    /// Returns (version, encoding, standalone) from `<?xml ... ?>`.
    fn parse_xml_decl(&self, text: &str, is_text: bool) -> Res<(Option<String>, Option<String>, i32)> {
        let code = if is_text { err::TEXT_DECL } else { err::XML_DECL };
        let inner = &text[5..text.len() - 2];
        let mut version = None;
        let mut encoding = None;
        let mut standalone = -1;
        let mut j = 0;
        let mut order = 0;
        loop {
            let sp = scan::skip_space(inner, j);
            if sp >= inner.len() {
                break;
            }
            if sp == j {
                return Err(code);
            }
            j = sp;
            let ne = scan::name_end(inner, j).ok_or(code)?;
            let name = &inner[j..ne];
            j = scan::skip_space(inner, ne);
            if !inner[j..].starts_with('=') {
                return Err(code);
            }
            j = scan::skip_space(inner, j + 1);
            let value = match scan::literal(inner, j) {
                Scan::Tok(v, e) => {
                    j = e;
                    v
                }
                _ => return Err(code),
            };
            let rank = match name {
                "version" => 1,
                "encoding" => 2,
                "standalone" if !is_text => 3,
                _ => return Err(code),
            };
            if rank <= order {
                return Err(code);
            }
            order = rank;
            match rank {
                1 => {
                    if value.is_empty() || !value.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-')) {
                        return Err(code);
                    }
                    version = Some(value.to_string());
                }
                2 => {
                    let mut it = value.chars();
                    if !matches!(it.next(), Some(c) if c.is_ascii_alphabetic()) || !it.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
                        return Err(code);
                    }
                    encoding = Some(value.to_string());
                }
                _ => {
                    standalone = match value {
                        "yes" => 1,
                        "no" => 0,
                        _ => return Err(code),
                    }
                }
            }
        }
        if is_text {
            if encoding.is_none() {
                return Err(code);
            }
        } else if version.is_none() {
            return Err(code);
        }
        Ok((version, encoding, standalone))
    }

    fn process_xml_decl(&mut self, h: &mut dyn Handler, text: &str) -> Res<()> {
        let is_text = self.kind != ParserKind::Document;
        let (version, encoding, standalone) = self.parse_xml_decl(text, is_text)?;
        if standalone == 1 {
            self.shared.dtd.borrow_mut().standalone = true;
        }
        emit!(h, text, h.xml_decl(version.as_deref(), encoding.as_deref(), standalone));
        if let (Some(name), false) = (&encoding, self.enc_override) {
            if name.eq_ignore_ascii_case("UTF-16") {
                if !self.enc.is_utf16() {
                    return Err(err::INCORRECT_ENCODING);
                }
            } else {
                let new = self.set_encoding_by_name(h, name)?;
                if self.enc.is_utf16() {
                    if new != self.enc {
                        return Err(err::INCORRECT_ENCODING);
                    }
                } else if new.is_utf16() {
                    return Err(err::INCORRECT_ENCODING);
                } else {
                    self.enc = new;
                }
            }
        }
        Ok(())
    }

    // ---- DOCTYPE and the DTD --------------------------------------------------------------

    fn start_doctype(&mut self, h: &mut dyn Handler, t: &str, start: usize, head: &scan::DoctypeHead) -> Res<()> {
        let pubid = match head.pubid {
            Some(p) => match check_pubid(p) {
                Ok(n) => Some(n),
                Err(c) => return self.bad(c, off(t, p)),
            },
            None => None,
        };
        let _ = start;
        if head.sysid.is_some() {
            self.shared.dtd.borrow_mut().has_param_entity_refs = true;
        }
        self.seen_doctype = true;
        self.doctype = Some(Doctype { sysid: head.sysid.map(str::to_string), pubid: pubid.clone() });
        self.phase = if head.has_subset { Phase::Subset } else { Phase::AfterDoctype };
        self.set_pos();
        match h.start_doctype(head.name, head.sysid, pubid.as_deref(), head.has_subset) {
            Flow::Abort => return Err(err::ABORTED),
            Flow::Unset => {
                if h.default_mode() != DefaultMode::None {
                    for &(a, b) in &head.tokens {
                        if h.default_text(&t[a..b]) == Flow::Abort {
                            return Err(err::ABORTED);
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn load_external_subset(&mut self, h: &mut dyn Handler, sysid: Option<&str>, pubid: Option<&str>) -> Res<()> {
        let base = self.shared.base();
        if self.shared.param_entity_parsing.get() != 0 {
            match h.external_entity_ref(None, base.as_deref(), sysid, pubid) {
                Flow::Abort => return Err(err::ABORTED),
                Flow::Fail => return Err(err::EXTERNAL_ENTITY_HANDLING),
                _ => {}
            }
        }
        let (read, standalone) = {
            let d = self.shared.dtd.borrow();
            (d.param_entity_read, d.standalone)
        };
        if read {
            if !standalone {
                match h.not_standalone() {
                    Flow::Abort => return Err(err::ABORTED),
                    Flow::Fail => return Err(err::NOT_STANDALONE),
                    _ => {}
                }
            }
        } else {
            self.shared.dtd.borrow_mut().keep_processing = standalone;
        }
        Ok(())
    }

    fn finish_doctype(&mut self, h: &mut dyn Handler) -> Res<()> {
        self.phase = Phase::AfterDoctype;
        let (sysid, pubid) = match &self.doctype {
            Some(d) => (d.sysid.clone(), d.pubid.clone()),
            None => (None, None),
        };
        if sysid.is_some() || self.shared.use_foreign_dtd.get() {
            self.shared.dtd.borrow_mut().has_param_entity_refs = true;
            self.load_external_subset(h, sysid.as_deref(), pubid.as_deref())?;
        }
        quiet!(h.end_doctype());
        Ok(())
    }

    /// The point where the root element begins.
    fn root_start(&mut self, h: &mut dyn Handler) -> Res<()> {
        if !self.seen_doctype && self.shared.use_foreign_dtd.get() {
            self.shared.dtd.borrow_mut().has_param_entity_refs = true;
            self.load_external_subset(h, None, None)?;
        }
        Ok(())
    }

    fn subset_step(&mut self, h: &mut dyn Handler, t: &str, i: &mut usize, fin: bool, in_pe: bool) -> Res<Step> {
        let start = *i;
        let rest = &t[start..];
        let c = rest.chars().next().unwrap_or(' ');
        if is_space(c) {
            let e = scan::skip_space(t, start);
            self.set_pos();
            self.dflt(h, &t[start..e])?;
            self.consume(t, i, e);
            return Ok(Step::Go);
        }
        let external = self.kind == ParserKind::ExternalSubset;
        match c {
            '<' => match rest[1..].chars().next() {
                None => self.need(fin),
                Some('?') => match scan::pi(t, start) {
                    Scan::Tok(pi, e) => {
                        self.pi_token(h, t, i, &pi, e)?;
                        Ok(Step::Go)
                    }
                    Scan::Need => self.need(fin),
                    Scan::Bad(p) => self.bad(err::INVALID_TOKEN, p),
                },
                Some('!') => {
                    if rest.starts_with("<!--") {
                        return self.comment_token(h, t, i, fin);
                    }
                    if rest.starts_with("<![") {
                        if !external {
                            return self.bad(err::SYNTAX, start);
                        }
                        return self.conditional(t, i, fin);
                    }
                    if ["<!ELEMENT", "<!ATTLIST", "<!ENTITY", "<!NOTATION"].iter().any(|k| rest.starts_with(k)) {
                        return match scan::declaration(t, start) {
                            Scan::Tok((), e) => {
                                self.set_pos();
                                self.handle_decl(h, &t[start..e])?;
                                self.consume(t, i, e);
                                Ok(Step::Go)
                            }
                            Scan::Need => self.need(fin),
                            Scan::Bad(p) => self.bad(err::INVALID_TOKEN, p),
                        };
                    }
                    if ["<!--", "<![", "<!ELEMENT", "<!ATTLIST", "<!ENTITY", "<!NOTATION"].iter().any(|k| scan::partial_prefix(k, rest)) {
                        return self.need(fin);
                    }
                    self.bad(err::SYNTAX, start)
                }
                Some(_) => self.bad(err::SYNTAX, start),
            },
            '%' => match scan::pe_reference(t, start) {
                Scan::Tok(name, e) => {
                    self.set_pos();
                    self.pe_ref(h, name, &t[start..e])?;
                    self.consume(t, i, e);
                    Ok(Step::Go)
                }
                Scan::Need => self.need(fin),
                Scan::Bad(p) => self.bad(err::INVALID_TOKEN, p),
            },
            ']' => {
                if self.cond_depth > 0 {
                    if rest.starts_with("]]>") {
                        self.cond_depth -= 1;
                        self.consume(t, i, start + 3);
                        return Ok(Step::Go);
                    }
                    if scan::partial_prefix("]]>", rest) {
                        return self.need(fin);
                    }
                    return self.bad(err::SYNTAX, start);
                }
                if external || in_pe {
                    return self.bad(err::SYNTAX, start);
                }
                let j = scan::skip_space(t, start + 1);
                match t[j..].chars().next() {
                    None => self.need(fin),
                    Some('>') => {
                        self.consume(t, i, j + 1);
                        self.finish_doctype(h)?;
                        Ok(Step::Go)
                    }
                    Some(_) => self.bad(err::SYNTAX, j),
                }
            }
            c if !is_xml_char(c) => self.bad(err::INVALID_TOKEN, start),
            _ => self.bad(err::SYNTAX, start),
        }
    }

    /// `<![INCLUDE[` and `<![IGNORE[`.
    fn conditional(&mut self, t: &str, i: &mut usize, fin: bool) -> Res<Step> {
        let start = *i;
        let open = match t[start + 3..].find('[') {
            Some(n) => start + 3 + n,
            None => return self.need(fin),
        };
        let keyword = {
            let raw = t[start + 3..open].to_string();
            self.expand_pes(&raw, 0)?
        };
        match keyword.trim() {
            "INCLUDE" => {
                self.cond_depth += 1;
                self.consume(t, i, open + 1);
                Ok(Step::Go)
            }
            "IGNORE" => {
                let mut depth = 1usize;
                let mut j = open + 1;
                while depth > 0 {
                    let r = &t[j..];
                    let a = r.find("<![");
                    let b = r.find("]]>");
                    match (a, b) {
                        (Some(a), Some(b)) if a < b => {
                            depth += 1;
                            j += a + 3;
                        }
                        (_, Some(b)) => {
                            depth -= 1;
                            j += b + 3;
                        }
                        (Some(a), None) => {
                            depth += 1;
                            j += a + 3;
                        }
                        (None, None) => return self.need(fin),
                    }
                }
                self.consume(t, i, j);
                Ok(Step::Go)
            }
            _ => self.bad(err::SYNTAX, start),
        }
    }

    /// Replaces `%name;` outside quoted literals with the parameter entity's text.
    fn expand_pes(&mut self, text: &str, depth: usize) -> Res<String> {
        if !text.contains('%') {
            return Ok(text.to_string());
        }
        if self.kind != ParserKind::ExternalSubset {
            let mut q: Option<char> = None;
            let mut it = text.char_indices().peekable();
            while let Some((_, c)) = it.next() {
                match q {
                    Some(qc) if c == qc => q = None,
                    Some(_) => {}
                    None => {
                        if c == '"' || c == '\'' {
                            q = Some(c);
                        } else if c == '%' && it.peek().is_some_and(|(_, n)| is_name_start(*n)) {
                            return Err(err::PARAM_ENTITY_REF);
                        }
                    }
                }
            }
            return Ok(text.to_string());
        }
        let mut out = String::new();
        let mut q: Option<char> = None;
        let mut j = 0;
        while j < text.len() {
            let c = text[j..].chars().next().unwrap_or(' ');
            match q {
                Some(qc) if c == qc => q = None,
                Some(_) => {}
                None => {
                    if c == '"' || c == '\'' {
                        q = Some(c);
                    } else if c == '%' && text[j + 1..].chars().next().is_some_and(is_name_start) {
                        if let Scan::Tok(name, e) = scan::pe_reference(text, j) {
                            if depth > 64 {
                                return Err(err::RECURSIVE_ENTITY_REF);
                            }
                            let ent = self.shared.dtd.borrow().param.get(name).cloned();
                            if let Some(ent) = ent {
                                if ent.open {
                                    return Err(err::RECURSIVE_ENTITY_REF);
                                }
                                if let Some(body) = ent.text {
                                    self.account_indirect(body.len())?;
                                    self.set_param_open(name, true);
                                    let expanded = self.expand_pes(&body, depth + 1);
                                    self.set_param_open(name, false);
                                    out.push(' ');
                                    out.push_str(&expanded?);
                                    out.push(' ');
                                }
                            }
                            j = e;
                            continue;
                        }
                    }
                }
            }
            out.push(c);
            j += c.len_utf8();
        }
        Ok(out)
    }

    fn set_param_open(&mut self, name: &str, open: bool) {
        if let Some(e) = self.shared.dtd.borrow_mut().param.get_mut(name) {
            e.open = open;
        }
    }

    fn set_general_open(&mut self, name: &str, open: bool) {
        if let Some(e) = self.shared.dtd.borrow_mut().general.get_mut(name) {
            e.open = open;
        }
    }

    /// An entity value literal: character references and (in external subsets) parameter entity
    /// references are replaced; general entity references stay.
    fn entity_value(&mut self, raw: &str) -> Res<String> {
        let mut out = String::new();
        let mut j = 0;
        while j < raw.len() {
            let c = raw[j..].chars().next().unwrap_or(' ');
            match c {
                '&' => match scan::reference(raw, j) {
                    Scan::Tok(Ref::Char(Some(ch)), e) => {
                        out.push(ch);
                        j = e;
                    }
                    Scan::Tok(Ref::Char(None), _) => return Err(err::BAD_CHAR_REF),
                    Scan::Tok(Ref::Entity(_), e) => {
                        out.push_str(&raw[j..e]);
                        j = e;
                    }
                    _ => return Err(err::INVALID_TOKEN),
                },
                '%' => {
                    if self.kind != ParserKind::ExternalSubset {
                        return Err(err::PARAM_ENTITY_REF);
                    }
                    match scan::pe_reference(raw, j) {
                        Scan::Tok(name, e) => {
                            let ent = self.shared.dtd.borrow().param.get(name).cloned();
                            if let Some(ent) = ent {
                                if ent.open {
                                    return Err(err::RECURSIVE_ENTITY_REF);
                                }
                                if let Some(body) = ent.text {
                                    self.account_indirect(body.len())?;
                                    out.push_str(&body);
                                }
                            }
                            j = e;
                        }
                        _ => return Err(err::INVALID_TOKEN),
                    }
                }
                '\r' => {
                    out.push('\n');
                    j += if raw[j..].starts_with("\r\n") { 2 } else { 1 };
                }
                c => {
                    out.push(c);
                    j += c.len_utf8();
                }
            }
        }
        Ok(out)
    }

    fn handle_decl(&mut self, h: &mut dyn Handler, text: &str) -> Res<()> {
        let expanded = self.expand_pes(text, 0)?;
        let decl = parse_decl(&expanded)?;
        let keep = self.shared.dtd.borrow().keep_processing;
        match decl {
            Decl::Element { name, model } => {
                emit!(h, text, h.element_decl(&name, &model));
            }
            Decl::Attlist { element, atts } => {
                let mut any_handled = false;
                for a in atts {
                    if !keep {
                        break;
                    }
                    let is_cdata = a.atype == "CDATA";
                    let (value, required) = match &a.default {
                        AttDefault::Required => (None, true),
                        AttDefault::Implied => (None, false),
                        AttDefault::Fixed(v) => (Some(self.default_value(v, is_cdata)?), true),
                        AttDefault::Value(v) => (Some(self.default_value(v, is_cdata)?), false),
                    };
                    {
                        let mut d = self.shared.dtd.borrow_mut();
                        let list = d.attlists.entry(element.clone()).or_default();
                        if !list.iter().any(|x| x.name == a.name) {
                            list.push(DefAtt { name: a.name.clone(), is_cdata, value: value.clone() });
                        }
                    }
                    match h.attlist_decl(&element, &a.name, &a.atype, value.as_deref(), required) {
                        Flow::Abort => return Err(err::ABORTED),
                        Flow::Unset => {}
                        _ => any_handled = true,
                    }
                }
                if !any_handled {
                    self.dflt(h, text)?;
                }
            }
            Decl::Entity { param, name, def } => {
                if !keep {
                    return self.dflt(h, text);
                }
                let exists = {
                    let d = self.shared.dtd.borrow();
                    if param {
                        d.param.contains_key(&name)
                    } else {
                        d.general.contains_key(&name)
                    }
                };
                if exists {
                    return self.dflt(h, text);
                }
                let base = self.shared.base();
                let mut ent = Entity { name: name.clone(), text: None, base: None, sysid: None, pubid: None, notation: None, open: false };
                let flow = match def {
                    EntityDef::Internal(raw) => {
                        let value = self.entity_value(&raw)?;
                        let f = h.entity_decl(&name, param, Some(&value), None, None, None, None);
                        ent.text = Some(Rc::from(value.as_str()));
                        f
                    }
                    EntityDef::External { pubid, sysid, notation } => {
                        ent.base = base.clone();
                        ent.sysid = Some(sysid.clone());
                        ent.pubid = pubid.clone();
                        ent.notation = notation.clone();
                        match &notation {
                            Some(n) => {
                                let f = h.unparsed_entity_decl(&name, base.as_deref(), Some(&sysid), pubid.as_deref(), n);
                                if f == Flow::Unset {
                                    h.entity_decl(&name, param, None, base.as_deref(), Some(&sysid), pubid.as_deref(), Some(n))
                                } else {
                                    f
                                }
                            }
                            None => h.entity_decl(&name, param, None, base.as_deref(), Some(&sysid), pubid.as_deref(), None),
                        }
                    }
                };
                {
                    let mut d = self.shared.dtd.borrow_mut();
                    if param {
                        d.param.insert(name, ent);
                    } else {
                        d.general.insert(name, ent);
                    }
                }
                match flow {
                    Flow::Abort => return Err(err::ABORTED),
                    Flow::Unset => self.dflt(h, text)?,
                    _ => {}
                }
            }
            Decl::Notation { name, pubid, sysid } => {
                let base = self.shared.base();
                emit!(h, text, h.notation_decl(&name, base.as_deref(), sysid.as_deref(), pubid.as_deref()));
            }
        }
        Ok(())
    }

    fn default_value(&mut self, raw: &str, is_cdata: bool) -> Res<String> {
        let mut out = String::new();
        self.attr_value(raw, is_cdata, &mut out, 0)?;
        if !is_cdata && out.ends_with(' ') {
            out.pop();
        }
        Ok(out)
    }

    fn pe_ref(&mut self, h: &mut dyn Handler, name: &str, raw: &str) -> Res<()> {
        let standalone = {
            let mut d = self.shared.dtd.borrow_mut();
            d.has_param_entity_refs = true;
            d.standalone
        };
        let mode = self.shared.param_entity_parsing.get();
        let skip = |p: &mut Parser, h: &mut dyn Handler| -> Res<()> {
            if !standalone {
                p.shared.dtd.borrow_mut().keep_processing = false;
            }
            emit!(h, raw, h.skipped_entity(name, true));
            Ok(())
        };
        if mode == 0 || (mode == 1 && standalone) {
            return skip(self, h);
        }
        let ent = self.shared.dtd.borrow().param.get(name).cloned();
        let ent = match ent {
            Some(e) => e,
            None => {
                self.shared.dtd.borrow_mut().keep_processing = standalone;
                emit!(h, raw, h.skipped_entity(name, true));
                return Ok(());
            }
        };
        if ent.open {
            return Err(err::RECURSIVE_ENTITY_REF);
        }
        match ent.text {
            Some(body) => {
                self.account_indirect(body.len())?;
                self.set_param_open(name, true);
                let r = self.run_nested(h, &body, true);
                self.set_param_open(name, false);
                r
            }
            None => {
                self.set_param_open(name, true);
                let f = h.external_entity_ref(None, ent.base.as_deref(), ent.sysid.as_deref(), ent.pubid.as_deref());
                self.set_param_open(name, false);
                match f {
                    Flow::Abort => Err(err::ABORTED),
                    Flow::Fail => Err(err::EXTERNAL_ENTITY_HANDLING),
                    Flow::Unset => {
                        emit!(h, raw, h.skipped_entity(name, true));
                        Ok(())
                    }
                    Flow::Continue => {
                        self.shared.dtd.borrow_mut().param_entity_read = true;
                        Ok(())
                    }
                }
            }
        }
    }

    /// Parses entity replacement text (`subset` selects markup declarations instead of content).
    fn run_nested(&mut self, h: &mut dyn Handler, text: &str, subset: bool) -> Res<()> {
        if self.nested >= MAX_NESTING {
            return Err(err::NO_MEMORY);
        }
        self.nested += 1;
        let saved_floor = self.tag_floor;
        let saved_cdata = self.in_cdata;
        let saved_cond = self.cond_depth;
        self.tag_floor = self.tags.len();
        self.in_cdata = false;
        let mut i = 0usize;
        let mut result = Ok(());
        while i < text.len() {
            let step = if subset { self.subset_step(h, text, &mut i, true, true) } else { self.content_step(h, text, &mut i, true) };
            match step {
                Ok(Step::Go) => {}
                Ok(Step::Stuck) => {
                    result = Err(err::UNCLOSED_TOKEN);
                    break;
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if result.is_ok() {
            if self.in_cdata {
                result = Err(err::UNCLOSED_CDATA_SECTION);
            } else if subset && self.cond_depth != saved_cond {
                result = Err(err::INCOMPLETE_PE);
            } else if !subset && self.tags.len() != self.tag_floor {
                result = Err(err::ASYNC_ENTITY);
            }
        }
        self.tag_floor = saved_floor;
        self.in_cdata = saved_cdata;
        self.nested -= 1;
        result
    }

    // ---- content --------------------------------------------------------------------------

    fn content_step(&mut self, h: &mut dyn Handler, t: &str, i: &mut usize, fin: bool) -> Res<Step> {
        if self.in_cdata {
            return self.cdata_step(h, t, i, fin);
        }
        let start = *i;
        let rest = &t[start..];
        let c = rest.chars().next().unwrap_or(' ');
        let more = !fin && self.nested == 0;
        match c {
            '<' => match rest[1..].chars().next() {
                None => self.need(fin),
                Some('/') => match scan::end_tag(t, start) {
                    Scan::Tok(name, e) => {
                        self.end_tag(h, t, i, name, e)?;
                        Ok(Step::Go)
                    }
                    Scan::Need => self.need(fin),
                    Scan::Bad(p) => self.bad(err::INVALID_TOKEN, p),
                },
                Some('!') => {
                    if rest.starts_with("<!--") {
                        return self.comment_token(h, t, i, fin);
                    }
                    if rest.starts_with("<![CDATA[") {
                        self.set_pos();
                        emit!(h, &t[start..start + 9], h.start_cdata());
                        self.in_cdata = true;
                        self.consume(t, i, start + 9);
                        return Ok(Step::Go);
                    }
                    if scan::partial_prefix("<!--", rest) || scan::partial_prefix("<![CDATA[", rest) {
                        return self.need(fin);
                    }
                    self.bad(err::INVALID_TOKEN, start + 2)
                }
                Some('?') => match scan::pi(t, start) {
                    Scan::Tok(pi, e) => {
                        self.pi_token(h, t, i, &pi, e)?;
                        Ok(Step::Go)
                    }
                    Scan::Need => self.need(fin),
                    Scan::Bad(p) => self.bad(err::INVALID_TOKEN, p),
                },
                Some(c2) if is_name_start(c2) => match scan::start_tag(t, start) {
                    Scan::Tok(st, e) => {
                        self.start_tag(h, t, i, st, e)?;
                        Ok(Step::Go)
                    }
                    Scan::Need => self.need(fin),
                    Scan::Bad(p) => self.bad(err::INVALID_TOKEN, p),
                },
                Some(_) => self.bad(err::INVALID_TOKEN, start + 1),
            },
            '&' => match scan::reference(t, start) {
                Scan::Tok(Ref::Char(Some(ch)), e) => {
                    self.set_pos();
                    let mut b = [0u8; 4];
                    emit!(h, &t[start..e], h.chardata(ch.encode_utf8(&mut b)));
                    self.consume(t, i, e);
                    Ok(Step::Go)
                }
                Scan::Tok(Ref::Char(None), _) => self.bad(err::BAD_CHAR_REF, start),
                Scan::Tok(Ref::Entity(name), e) => {
                    self.set_pos();
                    self.entity_ref(h, name, &t[start..e], start)?;
                    self.consume(t, i, e);
                    Ok(Step::Go)
                }
                Scan::Need => self.need(fin),
                Scan::Bad(p) => self.bad(err::INVALID_TOKEN, p),
            },
            '\r' | '\n' => {
                let len = if rest.starts_with("\r\n") {
                    2
                } else if c == '\r' && rest.len() == 1 && more {
                    return Ok(Step::Stuck);
                } else {
                    1
                };
                self.set_pos();
                emit!(h, &t[start..start + len], h.chardata("\n"));
                self.consume(t, i, start + len);
                Ok(Step::Go)
            }
            _ => match scan::text_run(t, start, more) {
                Ok(e) if e == start => self.need(fin),
                Ok(e) => {
                    self.set_pos();
                    emit!(h, &t[start..e], h.chardata(&t[start..e]));
                    self.consume(t, i, e);
                    Ok(Step::Go)
                }
                Err(p) => self.bad(err::INVALID_TOKEN, p),
            },
        }
    }

    fn cdata_step(&mut self, h: &mut dyn Handler, t: &str, i: &mut usize, fin: bool) -> Res<Step> {
        let start = *i;
        let rest = &t[start..];
        let more = !fin && self.nested == 0;
        if rest.starts_with("]]>") {
            self.set_pos();
            emit!(h, "]]>", h.end_cdata());
            self.in_cdata = false;
            self.consume(t, i, start + 3);
            return Ok(Step::Go);
        }
        let mut j = 0;
        for (n, c) in rest.char_indices() {
            j = n;
            match c {
                '\r' | '\n' => break,
                ']' => {
                    let tail = &rest[n..];
                    if tail.starts_with("]]>") || (more && scan::partial_prefix("]]>", tail)) {
                        break;
                    }
                }
                c if !is_xml_char(c) => return self.bad(err::INVALID_TOKEN, start + n),
                _ => {}
            }
            j = n + c.len_utf8();
        }
        if j == 0 {
            let c = rest.chars().next().unwrap_or(' ');
            if c == '\r' || c == '\n' {
                let len = if rest.starts_with("\r\n") {
                    2
                } else if c == '\r' && rest.len() == 1 && more {
                    return Ok(Step::Stuck);
                } else {
                    1
                };
                self.set_pos();
                emit!(h, &t[start..start + len], h.chardata("\n"));
                self.consume(t, i, start + len);
                return Ok(Step::Go);
            }
            return self.need(fin);
        }
        self.set_pos();
        emit!(h, &rest[..j], h.chardata(&rest[..j]));
        self.consume(t, i, start + j);
        Ok(Step::Go)
    }

    fn entity_ref(&mut self, h: &mut dyn Handler, name: &str, raw: &str, start: usize) -> Res<()> {
        if let Some(c) = predefined(name) {
            let mut b = [0u8; 4];
            emit!(h, raw, h.chardata(c.encode_utf8(&mut b)));
            return Ok(());
        }
        let ent = self.shared.dtd.borrow().general.get(name).cloned();
        let ent = match ent {
            Some(e) => e,
            None => {
                if let Some(text) = self.shared.entity_catalog.get().and_then(|lookup| lookup(name)) {
                    emit!(h, raw, h.chardata(text));
                    return Ok(());
                }
                let (hp, sa) = {
                    let d = self.shared.dtd.borrow();
                    (d.has_param_entity_refs, d.standalone)
                };
                if !hp || sa {
                    return self.bad(err::UNDEFINED_ENTITY, start);
                }
                self.shared.dtd.borrow_mut().keep_processing = sa;
                emit!(h, raw, h.skipped_entity(name, false));
                return Ok(());
            }
        };
        if ent.open {
            return self.bad(err::RECURSIVE_ENTITY_REF, start);
        }
        if ent.notation.is_some() {
            return self.bad(err::BINARY_ENTITY_REF, start);
        }
        match ent.text {
            Some(body) => {
                if h.default_mode() == DefaultMode::Raw {
                    return self.dflt(h, raw);
                }
                self.account_indirect(body.len())?;
                self.set_general_open(name, true);
                let r = self.run_nested(h, &body, false);
                self.set_general_open(name, false);
                r
            }
            None => {
                self.set_general_open(name, true);
                let context = self.get_context();
                let f = h.external_entity_ref(Some(&context), ent.base.as_deref(), ent.sysid.as_deref(), ent.pubid.as_deref());
                self.set_general_open(name, false);
                match f {
                    Flow::Abort => Err(err::ABORTED),
                    Flow::Fail => Err(err::EXTERNAL_ENTITY_HANDLING),
                    Flow::Continue => Ok(()),
                    Flow::Unset => {
                        emit!(h, raw, h.skipped_entity(name, false));
                        Ok(())
                    }
                }
            }
        }
    }

    /// The in-scope namespace bindings and open entities, separated by `\f`.
    pub fn get_context(&self) -> String {
        let mut out = String::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut entries: Vec<(&str, &str)> = Vec::new();
        for (p, u) in self.ns.iter().rev() {
            if seen.insert(p.as_str()) && !u.is_empty() {
                entries.push((p.as_str(), u.as_str()));
            }
        }
        entries.sort_by_key(|(p, _)| !p.is_empty());
        for (p, u) in entries {
            if !out.is_empty() {
                out.push('\x0C');
            }
            out.push_str(p);
            out.push('=');
            out.push_str(u);
        }
        let d = self.shared.dtd.borrow();
        let mut open: Vec<&str> = d.general.values().filter(|e| e.open).map(|e| e.name.as_str()).collect();
        open.sort_unstable();
        for n in open {
            if !out.is_empty() {
                out.push('\x0C');
            }
            out.push_str(n);
        }
        out
    }

    // ---- attribute values -----------------------------------------------------------------

    fn push_att_space(out: &mut String, cdata: bool) {
        if !cdata && (out.is_empty() || out.ends_with(' ')) {
            return;
        }
        out.push(' ');
    }

    fn attr_value(&mut self, raw: &str, cdata: bool, out: &mut String, depth: usize) -> Res<()> {
        if depth > MAX_NESTING {
            return Err(err::NO_MEMORY);
        }
        let mut j = 0;
        while j < raw.len() {
            let c = raw[j..].chars().next().unwrap_or(' ');
            match c {
                '&' => match scan::reference(raw, j) {
                    Scan::Tok(Ref::Char(Some(ch)), e) => {
                        if !(ch == ' ' && !cdata && (out.is_empty() || out.ends_with(' '))) {
                            out.push(ch);
                        }
                        j = e;
                    }
                    Scan::Tok(Ref::Char(None), _) => return Err(err::BAD_CHAR_REF),
                    Scan::Tok(Ref::Entity(name), e) => {
                        j = e;
                        if let Some(ch) = predefined(name) {
                            out.push(ch);
                            continue;
                        }
                        let ent = self.shared.dtd.borrow().general.get(name).cloned();
                        match ent {
                            None => {
                                if let Some(text) = self.shared.entity_catalog.get().and_then(|lookup| lookup(name)) {
                                    out.push_str(text);
                                    continue;
                                }
                                let (hp, sa) = {
                                    let d = self.shared.dtd.borrow();
                                    (d.has_param_entity_refs, d.standalone)
                                };
                                if !hp || sa {
                                    return Err(err::UNDEFINED_ENTITY);
                                }
                                self.shared.dtd.borrow_mut().keep_processing = sa;
                            }
                            Some(ent) => {
                                if ent.open {
                                    return Err(err::RECURSIVE_ENTITY_REF);
                                }
                                match ent.text {
                                    None => return Err(err::ATTRIBUTE_EXTERNAL_ENTITY_REF),
                                    Some(body) => {
                                        self.account_indirect(body.len())?;
                                        self.set_general_open(name, true);
                                        let r = self.attr_value(&body, cdata, out, depth + 1);
                                        self.set_general_open(name, false);
                                        r?;
                                    }
                                }
                            }
                        }
                    }
                    _ => return Err(err::INVALID_TOKEN),
                },
                '\r' => {
                    Self::push_att_space(out, cdata);
                    j += if raw[j..].starts_with("\r\n") { 2 } else { 1 };
                }
                '\n' | '\t' | ' ' => {
                    Self::push_att_space(out, cdata);
                    j += 1;
                }
                '<' => return Err(err::INVALID_TOKEN),
                c => {
                    out.push(c);
                    j += c.len_utf8();
                }
            }
        }
        Ok(())
    }

    // ---- elements -------------------------------------------------------------------------

    fn lookup_prefix(&self, prefix: &str) -> Option<&str> {
        if prefix == "xml" {
            return Some(XML_NS);
        }
        self.ns.iter().rev().find(|(p, _)| p == prefix).map(|(_, u)| u.as_str()).filter(|u| !u.is_empty())
    }

    /// `(uri-qualified name, key without the prefix)`.
    fn qualify(&self, name: &str, is_attr: bool) -> Res<(String, String)> {
        let sep = self.shared.opts.namespace_separator.unwrap_or(' ');
        let triplet = self.shared.triplet.get();
        if let Some(c) = name.find(':').filter(|&c| c > 0 && c + 1 < name.len()) {
            let (prefix, local) = (&name[..c], &name[c + 1..]);
            let uri = self.lookup_prefix(prefix).ok_or(err::UNBOUND_PREFIX)?;
            let key = format!("{uri}{sep}{local}");
            let full = if triplet { format!("{key}{sep}{prefix}") } else { key.clone() };
            return Ok((full, key));
        }
        if !is_attr {
            if let Some((_, uri)) = self.ns.iter().rev().find(|(p, _)| p.is_empty()) {
                if !uri.is_empty() {
                    let key = format!("{uri}{sep}{name}");
                    return Ok((key.clone(), key));
                }
            }
        }
        Ok((name.to_string(), name.to_string()))
    }

    fn start_tag(&mut self, h: &mut dyn Handler, t: &str, i: &mut usize, st: scan::StartTag, end: usize) -> Res<()> {
        let tag_start = *i;
        let ns_on = self.shared.opts.namespace_separator.is_some();
        let defaults: Vec<DefAtt> = self.shared.dtd.borrow().attlists.get(st.name).cloned().unwrap_or_default();

        let mut atts: Vec<(String, String, usize)> = Vec::with_capacity(st.attrs.len());
        let mut seen: HashSet<&str> = HashSet::new();
        for (n, v) in &st.attrs {
            let at = off(t, n);
            if !seen.insert(n) {
                return self.bad(err::DUPLICATE_ATTRIBUTE, at);
            }
            let cdata = defaults.iter().find(|d| d.name == *n).map_or(true, |d| d.is_cdata);
            let mut val = String::new();
            if let Err(c) = self.attr_value(v, cdata, &mut val, 0) {
                return self.bad(c, tag_start);
            }
            if !cdata && val.ends_with(' ') {
                val.pop();
            }
            atts.push((n.to_string(), val, at));
        }
        let specified_raw = atts.len();
        for d in &defaults {
            if let Some(v) = &d.value {
                if !seen.contains(d.name.as_str()) {
                    atts.push((d.name.clone(), v.clone(), tag_start));
                }
            }
        }

        self.set_pos();
        let ns_mark = self.ns.len();
        let mut declared: Vec<Option<String>> = Vec::new();
        let mut out_atts: Vec<Attribute> = Vec::new();
        let out_name;
        let mut specified = specified_raw;
        if ns_on {
            let mut rest: Vec<(String, String, usize, bool)> = Vec::new();
            for (idx, (n, v, at)) in atts.into_iter().enumerate() {
                let prefix = if n == "xmlns" {
                    Some(None)
                } else {
                    n.strip_prefix("xmlns:").map(|p| Some(p.to_string()))
                };
                match prefix {
                    None => rest.push((n, v, at, idx < specified_raw)),
                    Some(prefix) => {
                        if let Some(p) = &prefix {
                            if p == "xml" {
                                if v != XML_NS {
                                    return self.bad(err::RESERVED_PREFIX_XML, at);
                                }
                                continue;
                            }
                            if p == "xmlns" {
                                return self.bad(err::RESERVED_PREFIX_XMLNS, at);
                            }
                            if v.is_empty() {
                                return self.bad(err::UNDECLARING_PREFIX, at);
                            }
                        }
                        if v == XML_NS || v == XMLNS_NS {
                            return self.bad(err::RESERVED_NAMESPACE_URI, at);
                        }
                        self.ns.push((prefix.clone().unwrap_or_default(), v.clone()));
                        declared.push(prefix.clone());
                        let uri = if v.is_empty() { None } else { Some(v.as_str()) };
                        quiet!(h.start_namespace(prefix.as_deref(), uri));
                    }
                }
            }
            let (qname, _) = match self.qualify(st.name, false) {
                Ok(q) => q,
                Err(c) => return self.bad(c, tag_start),
            };
            out_name = qname;
            let mut keys: HashSet<String> = HashSet::new();
            specified = 0;
            for (n, v, at, spec) in rest {
                let (full, key) = match self.qualify(&n, true) {
                    Ok(q) => q,
                    Err(c) => return self.bad(c, at),
                };
                if !keys.insert(key) {
                    return self.bad(err::DUPLICATE_ATTRIBUTE, at);
                }
                if spec {
                    specified += 1;
                }
                out_atts.push(Attribute { name: full, value: v });
            }
        } else {
            out_name = st.name.to_string();
            out_atts = atts.into_iter().map(|(n, v, _)| Attribute { name: n, value: v }).collect();
        }

        let raw_text = &t[tag_start..end];
        let started = h.start_element(&out_name, &out_atts, specified);
        if started == Flow::Abort {
            return Err(err::ABORTED);
        }
        if st.empty {
            let ended = h.end_element(&out_name);
            if ended == Flow::Abort {
                return Err(err::ABORTED);
            }
            if started == Flow::Unset && ended == Flow::Unset {
                self.dflt(h, raw_text)?;
            }
            for p in declared.iter().rev() {
                quiet!(h.end_namespace(p.as_deref()));
            }
            self.ns.truncate(ns_mark);
        } else {
            if started == Flow::Unset {
                self.dflt(h, raw_text)?;
            }
            self.tags.push(Tag { raw: st.name.to_string(), out: out_name, declared, ns_mark });
        }
        self.consume(t, i, end);
        if self.kind == ParserKind::Document && self.nested == 0 {
            self.phase = if self.tags.is_empty() { Phase::Epilog } else { Phase::Content };
        }
        Ok(())
    }

    fn end_tag(&mut self, h: &mut dyn Handler, t: &str, i: &mut usize, name: &str, end: usize) -> Res<()> {
        let start = *i;
        if self.tags.len() <= self.tag_floor {
            return self.bad(err::ASYNC_ENTITY, start);
        }
        if self.tags.last().map_or(true, |tag| tag.raw != name) {
            return self.bad(err::TAG_MISMATCH, start + 2);
        }
        let tag = match self.tags.pop() {
            Some(tag) => tag,
            None => return Err(err::UNEXPECTED_STATE),
        };
        self.set_pos();
        emit!(h, &t[start..end], h.end_element(&tag.out));
        for p in tag.declared.iter().rev() {
            quiet!(h.end_namespace(p.as_deref()));
        }
        self.ns.truncate(tag.ns_mark);
        self.consume(t, i, end);
        if self.kind == ParserKind::Document && self.nested == 0 && self.tags.is_empty() {
            self.phase = Phase::Epilog;
        }
        Ok(())
    }
}

use super::chars::Step as Step2;
