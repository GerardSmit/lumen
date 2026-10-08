//! Source-preserving CSS Syntax token boundaries shared by parser entry points.
use super::*;
use alloc::borrow::Cow;

pub(super) const MAX_COMPONENT_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TokenKind {
    Open(u8), Close(u8), String { quote: u8, closed: bool },
    Comment { closed: bool }, Url { closed: bool }, BadString, BadUrl,
    Escape, Other,
}
#[derive(Clone, Copy, Debug)]
pub(super) struct Token { pub start: usize, pub end: usize, pub kind: TokenKind }

/// No token vector or decoded identifier allocation is needed for ordinary input.
pub(super) struct Cursor<'a> { pub input: &'a str, pub position: usize }
impl<'a> Cursor<'a> {
    pub fn new(input: &'a str, position: usize) -> Result<Self, CssError> {
        if input.len() > MAX_CSS_BYTES { return Err(error(position, "CSS input too large")); }
        Ok(Self { input, position })
    }
    pub fn next(&mut self) -> Option<Token> {
        let bytes = self.input.as_bytes();
        let start = self.position;
        let byte = *bytes.get(start)?;
        let kind = match byte {
            b'/' if bytes.get(start + 1) == Some(&b'*') => {
                let end = self.input[start + 2..].find("*/");
                self.position = end.map_or(bytes.len(), |end| start + end + 4);
                TokenKind::Comment { closed: end.is_some() }
            }
            quote @ (b'\'' | b'"') => {
                self.position += 1;
                loop {
                    match bytes.get(self.position).copied() {
                        None => break TokenKind::String { quote, closed: false },
                        Some(byte) if byte == quote => {
                            self.position += 1;
                            break TokenKind::String { quote, closed: true };
                        }
                        Some(b'\n' | b'\r' | b'\x0c') => break TokenKind::BadString,
                        Some(b'\\') => {
                            self.position += 1;
                            if matches!(bytes.get(self.position), Some(b'\n' | b'\r' | b'\x0c')) {
                                let cr = bytes[self.position] == b'\r';
                                self.position += 1;
                                if cr && bytes.get(self.position) == Some(&b'\n') { self.position += 1; }
                            } else if self.position < bytes.len() {
                                self.position += self.input[self.position..].chars().next().unwrap().len_utf8();
                            }
                        }
                        Some(_) => self.position += self.input[self.position..].chars().next().unwrap().len_utf8(),
                    }
                }
            }
            b'(' | b'[' | b'{' => { self.position += 1; TokenKind::Open(byte) }
            b')' | b']' | b'}' => { self.position += 1; TokenKind::Close(byte) }
            b'\\' if !valid_css_escape_at_start(&self.input[start..]) => {
                // A backslash at EOF is a valid escape with replacement value.
                if selector_escape(self.input, &mut self.position).is_none() { self.position = start + 1; }
                TokenKind::Escape
            }
            b'#' | b'@' if self.input.get(start+1..).is_some_and(|rest| {
                if byte == b'@' { starts_identifier(rest) } else {
                    rest.chars().next().is_some_and(|ch|ch == '\0' || selector_name_character(ch)) || valid_css_escape_at_start(rest)
                }
            }) => {
                self.position += 1;
                self.consume_name();
                TokenKind::Other
            }
            _ if starts_number(&bytes[start..]) => {
                self.position = number_end(bytes, start);
                if starts_identifier(&self.input[self.position..]) {
                    self.consume_name();
                } else if bytes.get(self.position) == Some(&b'%') { self.position += 1; }
                TokenKind::Other
            }
            _ if starts_identifier(&self.input[start..]) => {
                let mut escaped = false;
                while let Some(character) = self.input[self.position..].chars().next() {
                    if character == '\0' || selector_name_character(character) { self.position += character.len_utf8(); }
                    else if character == '\\' && valid_css_escape_at_start(&self.input[self.position..]) {
                        escaped = true;
                        selector_escape(self.input, &mut self.position)?;
                    } else { break; }
                }
                let end = self.position;
                let url = if escaped {
                    let mut at = start;
                    consume_selector_identifier(self.input, &mut at).is_some_and(|name| name.eq_ignore_ascii_case("url"))
                } else { self.input[start..end].eq_ignore_ascii_case("url") };
                if url && bytes.get(end) == Some(&b'(') {
                    let mut at = end + 1;
                    while bytes.get(at).copied().is_some_and(is_css_whitespace) { at += 1; }
                    if !matches!(bytes.get(at), Some(b'\'' | b'"')) {
                        self.position = at;
                        self.url_token()
                    } else { TokenKind::Other }
                } else { TokenKind::Other }
            }
            _ => { self.position += self.input[start..].chars().next().unwrap().len_utf8(); TokenKind::Other }
        };
        Some(Token { start, end: self.position, kind })
    }
    fn consume_name(&mut self) {
        while let Some(character) = self.input[self.position..].chars().next() {
            if character == '\0' || selector_name_character(character) { self.position += character.len_utf8(); }
            else if character == '\\' && valid_css_escape_at_start(&self.input[self.position..]) {
                if selector_escape(self.input, &mut self.position).is_none() { break; }
            } else { break; }
        }
    }
    fn url_token(&mut self) -> TokenKind {
        let bytes = self.input.as_bytes();
        let mut bad = false;
        loop {
            let Some(byte) = bytes.get(self.position).copied() else {
                return if bad { TokenKind::BadUrl } else { TokenKind::Url { closed: false } };
            };
            if byte == b')' {
                self.position += 1;
                return if bad { TokenKind::BadUrl } else { TokenKind::Url { closed: true } };
            }
            if byte == b'\\' {
                if selector_escape(self.input, &mut self.position).is_none() {
                    bad = true;
                    self.position += 1;
                }
                continue;
            }
            if is_css_whitespace(byte) {
                while bytes.get(self.position).copied().is_some_and(is_css_whitespace) { self.position += 1; }
                if bytes.get(self.position).is_some_and(|byte| *byte != b')') { bad = true; }
                continue;
            }
            bad |= matches!(byte, b'\'' | b'"' | b'(' | 1..=8 | 11 | 14..=31 | 127);
            self.position += self.input[self.position..].chars().next().unwrap().len_utf8();
        }
    }
}

fn starts_identifier(input: &str) -> bool {
    input.starts_with('\0') || input.starts_with("-\0") || would_start_css_identifier(input)
}

// CSS Syntax's consume-number boundary needs no numeric conversion or allocation.
fn starts_number(bytes: &[u8]) -> bool {
    let bytes = if matches!(bytes.first(), Some(b'+' | b'-')) { &bytes[1..] } else { bytes };
    bytes.first().is_some_and(u8::is_ascii_digit)
        || bytes.first() == Some(&b'.') && bytes.get(1).is_some_and(u8::is_ascii_digit)
}
fn number_end(bytes: &[u8], mut at: usize) -> usize {
    if matches!(bytes.get(at), Some(b'+' | b'-')) { at += 1; }
    while bytes.get(at).is_some_and(u8::is_ascii_digit) { at += 1; }
    if bytes.get(at) == Some(&b'.') && bytes.get(at+1).is_some_and(u8::is_ascii_digit) {
        at += 1;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) { at += 1; }
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        let exponent = at+1;
        let digits = exponent + usize::from(matches!(bytes.get(exponent), Some(b'+' | b'-')));
        if bytes.get(digits).is_some_and(u8::is_ascii_digit) {
            at = digits+1;
            while bytes.get(at).is_some_and(u8::is_ascii_digit) { at += 1; }
        }
    }
    at
}

pub(super) fn closing(open: u8) -> u8 { match open { b'(' => b')', b'[' => b']', _ => b'}' } }
pub(super) fn error(offset: usize, message: &'static str) -> CssError { CssError { offset, message } }

#[derive(Clone, Copy)]
pub(super) struct BlockEnd {
    pub after: usize, pub content_end: usize, pub closed: bool,
}
pub(super) fn block(input: &str, open: usize) -> Result<BlockEnd, CssError> {
    let first = *input.as_bytes().get(open).ok_or_else(||error(open,"expected CSS block"))?;
    if !matches!(first,b'('|b'['|b'{') { return Err(error(open,"expected CSS block")); }
    let mut cursor = Cursor::new(input, open + 1)?;
    let mut stack = [0; MAX_COMPONENT_DEPTH];
    stack[0] = closing(first);
    let mut depth = 1;
    while let Some(token) = cursor.next() {
        match token.kind {
            TokenKind::Open(open) => {
                if depth == stack.len() { return Err(error(token.start,"CSS component nesting limit")); }
                stack[depth] = closing(open); depth += 1;
            }
            TokenKind::Close(close) if stack[depth-1] == close => {
                depth -= 1;
                if depth == 0 { return Ok(BlockEnd { after: token.end, content_end: token.start, closed: true }); }
            }
            // A different closing token is an ordinary component token. It
            // cannot consume a matching delimiter owned by another block.
            _ => {}
        }
    }
    Ok(BlockEnd { after: input.len(), content_end: input.len(), closed: false })
}

pub(super) fn boundary(input: &str) -> Result<Option<RuleBoundary>, CssError> {
    let at_rule = input.strip_prefix('@').is_some_and(would_start_css_identifier);
    let mut cursor = Cursor::new(input,0)?;
    while let Some(token) = cursor.next() {
        match token.kind {
            TokenKind::Open(b'{') => return Ok(Some(RuleBoundary::Block(token.start))),
            TokenKind::Open(_) => cursor.position = block(input,token.start)?.after,
            TokenKind::Close(b'}') => return Ok(Some(RuleBoundary::Discard(token.end))),
            TokenKind::Other if input.as_bytes()[token.start] == b';' && at_rule => return Ok(Some(RuleBoundary::Statement(token.start))),
            _ => {}
        }
    }
    Ok(at_rule.then_some(RuleBoundary::Statement(input.len())))
}

/// Complete component token serialization only at EOF. Ordinary input borrows
/// the original bytes; error recovery/source tree parsing never uses this copy.
pub(super) fn complete(input: &str) -> Result<Option<Cow<'_,str>>,CssError> {
    let mut cursor = Cursor::new(input,0)?;
    let mut stack = [0;MAX_COMPONENT_DEPTH];
    let mut depth = 0;
    let mut output: Option<String> = None;
    let mut copied = 0;
    while let Some(token) = cursor.next() {
        let replacement = match token.kind {
            TokenKind::Open(open) => {
                if depth == stack.len() { return Err(error(token.start,"CSS component nesting limit")); }
                stack[depth] = closing(open); depth += 1; None
            }
            TokenKind::Close(close) => {
                if depth == 0 { return Ok(None); }
                if stack[depth-1] == close { depth -= 1; }
                None
            }
            TokenKind::BadString | TokenKind::BadUrl => return Ok(None),
            TokenKind::Comment { closed:false } => Some((token.start, "/**/")),
            TokenKind::Escape if token.end == input.len() && trailing_escape(input) => Some((token.start,"\u{fffd}")),
            TokenKind::Other if token.end == input.len() && trailing_escape(input) => Some((input.len()-1,"\u{fffd}")),
            TokenKind::String { quote, closed:false } => {
                let value = output.get_or_insert_with(String::new);
                // The final backslash in a string contributes no code point.
                let end = if trailing_escape(input) { input.len()-1 } else { input.len() };
                value.push_str(&input[copied..end]); value.push(quote as char);
                copied = token.end; None
            }
            TokenKind::Url { closed:false } => {
                let value = output.get_or_insert_with(String::new);
                let end = if trailing_escape(input) { input.len()-1 } else { input.len() };
                value.push_str(&input[copied..end]);
                if end != input.len() { value.push('\u{fffd}'); }
                value.push(')'); copied=token.end; None
            }
            _ => None,
        };
        if let Some((end,replacement)) = replacement {
            let value = output.get_or_insert_with(String::new);
            value.push_str(&input[copied..end]); value.push_str(replacement); copied=token.end;
        }
    }
    if depth != 0 || output.is_some() {
        let value = output.get_or_insert_with(String::new);
        value.push_str(&input[copied..]);
        for &close in stack[..depth].iter().rev() { value.push(close as char); }
        if value.len() > MAX_CSS_BYTES { return Err(error(input.len(),"CSS input too large")); }
    }
    let completed=output.map_or(Cow::Borrowed(input),Cow::Owned);
    if completed.contains('\0') {
        if completed.len().checked_add(completed.bytes().filter(|&byte|byte==0).count().saturating_mul(2)).is_none_or(|size|size>MAX_CSS_BYTES) {
            return Err(error(input.len(),"CSS input too large"));
        }
        return Ok(Some(Cow::Owned(completed.replace('\0',"\u{fffd}"))));
    }
    Ok(Some(completed))
}

fn trailing_escape(input:&str)->bool {
    input.as_bytes().iter().rev().take_while(|&&byte|byte==b'\\').count()%2!=0
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_css_component_eof_and_bad_token_recovery_preserve_source_ranges() {
        for (source,expected) in [
            ("foo([x", "foo([x])"),
            ("'text", "'text'"),
            ("\"text\\", "\"text\""),
            ("url(foo", "url(foo)"),
            ("url(foo\\", "url(foo\u{fffd})"),
            ("name\\", "name\u{fffd}"),
            ("foo(1/* never closed", "foo(1/**/)"),
            ("foo\0bar", "foo\u{fffd}bar"),
        ] { assert_eq!(complete(source).unwrap().unwrap(),expected,"{source:?}"); }
        assert!(matches!(complete("foo('()') [x]").unwrap(),Some(Cow::Borrowed(_))));
        for source in ["'bad\nnext'", "url(foo bar)", "url(foo\"bar)", "]x"] {
            assert!(complete(source).unwrap().is_none(),"{source:?}");
        }
        assert_eq!(complete("foo(]x)").unwrap().unwrap(), "foo(]x)", "mismatched closes inside a component block remain ordinary tokens");
        for source in ["1url(foo bar)", ".1url(foo bar)", "1e2url(foo bar)", "#url(foo bar)", "@url(foo bar)"] {
            assert_eq!(complete(source).unwrap().unwrap(),source,"a dimension/hash/at-keyword cannot become a URL token");
        }
        for source in ["name\\", "#name\\", "1name\\", "@name\\"] {
            assert!(complete(source).unwrap().unwrap().ends_with('\u{fffd}'),"{source}");
        }
        assert!(complete("\\75 rl(foo bar)").unwrap().is_none(), "a leading escape participates in the ident-like URL token");
        assert_eq!(complete("\\6e ame\\").unwrap().unwrap(), "\\6e ame\u{fffd}", "an escaped leading identifier retains its spelling and EOF escape replacement");
        let source="{ a(]x) } after";
        let end=block(source,0).unwrap();
        assert_eq!(&source[end.after..]," after","an unrelated closing delimiter does not terminate another block");
        assert_eq!(&source[1..end.content_end]," a(]x) ");
        assert!(block(&"(".repeat(MAX_COMPONENT_DEPTH+1),0).is_err());
        assert!(complete(&"(".repeat(MAX_COMPONENT_DEPTH+1)).is_err());
    }
}
