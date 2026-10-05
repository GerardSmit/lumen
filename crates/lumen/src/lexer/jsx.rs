use super::*;
use crate::ast::{JsxAttribute, JsxChild, JsxElement};

impl Lexer<'_> {
    pub(super) fn jsx_starts(&self) -> bool {
        let tail = &self.src[self.pos + 1..];
        if tail.starts_with('>') {
            return true;
        }
        // A JSX fragment may contain spaces and comments between the angle brackets.
        // This is distinct from `< expression`, which remains a relational operator.
        if jsx_fragment_start(tail) {
            return true;
        }
        if !tail.chars().next().is_some_and(is_ident_start) {
            return false;
        }
        if self.ts {
            let end = tail.find(|c: char| !is_ident_part(c)).unwrap_or(tail.len());
            let rest = tail[end..].trim_start();
            // TSX's unambiguous generic arrows keep the TypeScript token path.
            if rest.starts_with([',', '='])
                || rest
                    .strip_prefix("extends")
                    .is_some_and(|s| s.chars().next().is_some_and(|c| !is_ident_part(c)))
            {
                return false;
            }
        }
        true
    }

    fn jsx_error(&self, message: impl Into<String>) -> LexError {
        self.err(message)
    }

    fn jsx_space(&mut self) -> Result<(), LexError> {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.bump();
            }
            if self.peek() == Some('/') && self.peek2() == Some('*') {
                self.bump();
                self.bump();
                while !(self.peek() == Some('*') && self.peek2() == Some('/')) {
                    if self.bump().is_none() {
                        return Err(self.jsx_error("unterminated comment in JSX tag"));
                    }
                }
                self.bump();
                self.bump();
                continue;
            }
            if self.peek() == Some('/') && self.peek2() == Some('/') {
                self.bump();
                self.bump();
                while self.peek().is_some_and(|c| c != '\n' && c != '\r') {
                    self.bump();
                }
                continue;
            }
            return Ok(());
        }
    }

    fn jsx_identifier(&mut self) -> Result<String, LexError> {
        let start = self.pos;
        if !self.peek().is_some_and(is_ident_start) {
            return Err(self.jsx_error("expected JSX name"));
        }
        self.bump();
        while self.peek().is_some_and(|c| is_ident_part(c) || c == '-') {
            self.bump();
        }
        Ok(self.src[start..self.pos].to_string())
    }

    fn jsx_name(&mut self, member: bool) -> Result<String, LexError> {
        let mut name = self.jsx_identifier()?;
        self.jsx_space()?;
        if self.peek() == Some(':') {
            self.bump();
            self.jsx_space()?;
            name.push(':');
            name.push_str(&self.jsx_identifier()?);
        } else if member {
            while self.peek() == Some('.') {
                self.bump();
                self.jsx_space()?;
                name.push('.');
                name.push_str(&self.jsx_identifier()?);
                self.jsx_space()?;
            }
        }
        Ok(name)
    }

    fn jsx_container(&mut self) -> Result<SubToks, LexError> {
        self.bump(); // {
        match self.read_template_sub().map_err(|e| {
            if e.at_eof {
                self.jsx_error("unterminated JSX expression container")
            } else {
                e
            }
        })? {
            TplPart::Sub(tokens) => Ok(tokens),
            _ => unreachable!(),
        }
    }

    pub(super) fn read_jsx_element(&mut self) -> Result<JsxElement, LexError> {
        if crate::stack::exhausted() {
            return Err(self.jsx_error("JSX nesting too deep"));
        }
        let (start, line) = (self.pos as u32 + self.offset, self.line);
        self.bump(); // <
        self.jsx_space()?;
        let name = if self.peek() == Some('>') {
            None
        } else {
            Some(self.jsx_name(true)?)
        };
        if self.ts && name.is_some() && self.peek() == Some('<') {
            self.jsx_type_arguments()?;
        }
        let mut attributes = Vec::new();
        if name.is_some() {
            loop {
                self.jsx_space()?;
                match self.peek() {
                    Some('>') => break,
                    Some('/') if self.peek2() == Some('>') => {
                        self.bump();
                        self.bump();
                        return Ok(JsxElement {
                            name,
                            attributes,
                            children: Vec::new(),
                            start,
                            end: self.pos as u32 + self.offset,
                            line,
                        });
                    }
                    Some('{') => {
                        let tokens = self.jsx_container()?;
                        if !matches!(tokens.0.get(0).map(|t| &t.kind), Some(Tok::Punct("..."))) {
                            return Err(
                                self.jsx_error("JSX spread attribute must begin with '...'")
                            );
                        }
                        attributes.push(JsxAttribute::Spread(tokens));
                    }
                    None => return Err(self.jsx_error("unterminated JSX opening element")),
                    _ => {
                        let key = self.jsx_name(false)?;
                        self.jsx_space()?;
                        let value = if self.peek() == Some('=') {
                            self.bump();
                            self.jsx_space()?;
                            Some(match self.peek() {
                                Some(q @ ('\'' | '"')) => {
                                    self.bump();
                                    let from = self.pos;
                                    while self.peek().is_some_and(|c| c != q) {
                                        self.bump();
                                    }
                                    if self.peek().is_none() {
                                        return Err(
                                            self.jsx_error("unterminated JSX attribute string")
                                        );
                                    }
                                    let text = normalize_attribute(
                                        &decode_entities(&self.src[from..self.pos])
                                            .map_err(|message| self.jsx_error(message))?,
                                    );
                                    self.bump();
                                    JsxChild::Text(text)
                                }
                                Some('{') => {
                                    let tokens = self.jsx_container()?;
                                    if matches!(
                                        tokens.0.get(0).map(|token| &token.kind),
                                        Some(Tok::Eof)
                                    ) {
                                        return Err(self.jsx_error(
                                            "JSX attribute expression must not be empty",
                                        ));
                                    }
                                    JsxChild::Expression(tokens)
                                }
                                Some('<') => JsxChild::Element(Rc::new(self.read_jsx_element()?)),
                                _ => return Err(self.jsx_error(
                                    "JSX attribute value must be a string, expression, or element",
                                )),
                            })
                        } else {
                            None
                        };
                        attributes.push(JsxAttribute::Named(key, value));
                    }
                }
            }
        }
        self.bump(); // >
        let mut children = Vec::new();
        loop {
            match self.peek() {
                None => return Err(self.jsx_error("unterminated JSX element")),
                Some('<') if self.peek2() == Some('/') => {
                    self.bump();
                    self.bump();
                    self.jsx_space()?;
                    let closing = if self.peek() == Some('>') {
                        None
                    } else {
                        Some(self.jsx_name(true)?)
                    };
                    self.jsx_space()?;
                    if closing != name {
                        return Err(self.jsx_error(format!(
                            "JSX closing tag does not match {}",
                            name.as_deref().unwrap_or("fragment")
                        )));
                    }
                    if self.peek() != Some('>') {
                        return Err(self.jsx_error("expected '>' in JSX closing element"));
                    }
                    self.bump();
                    break;
                }
                Some('<') => children.push(JsxChild::Element(Rc::new(self.read_jsx_element()?))),
                Some('{') => {
                    let tokens = self.jsx_container()?;
                    let spread =
                        matches!(tokens.0.get(0).map(|t| &t.kind), Some(Tok::Punct("...")));
                    children.push(if spread {
                        JsxChild::Spread(tokens)
                    } else {
                        JsxChild::Expression(tokens)
                    });
                }
                _ => {
                    let from = self.pos;
                    while self.peek().is_some_and(|c| c != '<' && c != '{') {
                        if self.ts && matches!(self.peek(), Some('}' | '>')) {
                            return Err(self.jsx_error("unescaped '}' or '>' in JSX text"));
                        }
                        self.bump();
                    }
                    children.push(JsxChild::Text(
                        decode_entities(&self.src[from..self.pos])
                            .map_err(|message| self.jsx_error(message))?,
                    ));
                }
            }
        }
        Ok(JsxElement {
            name,
            attributes,
            children,
            start,
            end: self.pos as u32 + self.offset,
            line,
        })
    }

    /// TypeScript permits type arguments after a JSX component name. They carry no run-time
    /// value, so the lexer consumes their balanced source form while preserving the component
    /// name and attributes used by the runtime AST.
    fn jsx_type_arguments(&mut self) -> Result<(), LexError> {
        debug_assert!(self.ts && self.peek() == Some('<'));
        self.bump();
        let mut angles = 1usize;
        let mut delimiters = Vec::new();
        let mut quote = None;
        let mut escaped = false;
        let mut saw_type_content = false;
        while let Some(c) = self.peek() {
            if let Some(q) = quote {
                self.bump();
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
                continue;
            }
            if matches!(c, '\'' | '"' | '`') {
                quote = Some(c);
                self.bump();
                continue;
            }
            if c == '/' && self.peek2() == Some('/') {
                self.bump();
                self.bump();
                while self.peek().is_some_and(|d| d != '\n' && d != '\r') {
                    self.bump();
                }
                continue;
            }
            if c == '/' && self.peek2() == Some('*') {
                self.bump();
                self.bump();
                while !(self.peek() == Some('*') && self.peek2() == Some('/')) {
                    if self.bump().is_none() {
                        return Err(self.jsx_error("unterminated comment in JSX type arguments"));
                    }
                }
                self.bump();
                self.bump();
                continue;
            }
            match c {
                '(' | '[' | '{' => delimiters.push(match c {
                    '(' => ')',
                    '[' => ']',
                    _ => '}',
                }),
                ')' | ']' | '}' => {
                    if delimiters.pop() != Some(c) {
                        return Err(self.jsx_error("unbalanced JSX type arguments"));
                    }
                }
                '<' if delimiters.is_empty() => angles += 1,
                '>' if delimiters.is_empty() && !self.src[..self.pos].ends_with('=') => {
                    angles -= 1;
                    self.bump();
                    if angles == 0 {
                        if !saw_type_content {
                            return Err(
                                self.jsx_error("TypeScript JSX type argument list cannot be empty")
                            );
                        }
                        return Ok(());
                    }
                    continue;
                }
                _ => {}
            }
            if !c.is_whitespace() {
                saw_type_content = true;
            }
            self.bump();
        }
        Err(self.jsx_error("unterminated JSX type arguments"))
    }
}

fn jsx_fragment_start(tail: &str) -> bool {
    let bytes = tail.as_bytes();
    let mut i = 0;
    loop {
        while bytes.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if bytes.get(i..i + 2) == Some(b"/*") {
            let Some(end) = tail[i + 2..].find("*/") else {
                return false;
            };
            i += end + 4;
            continue;
        }
        if bytes.get(i..i + 2) == Some(b"//") {
            while bytes
                .get(i)
                .is_some_and(|byte| *byte != b'\n' && *byte != b'\r')
            {
                i += 1;
            }
            continue;
        }
        return bytes.get(i) == Some(&b'>');
    }
}

fn decode_entities(text: &str) -> Result<String, &'static str> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(end) = rest[1..]
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '#')
            .map(|end| end + 1)
            .filter(|end| rest.as_bytes()[*end] == b';')
        {
            let entity = &rest[1..end];
            if let Some(reference) =
                lumen_common::entities::char_ref(&rest[1..]).filter(|r| r.len == entity.len() + 1)
            {
                if lumen_common::smuggle::push_code_point(&mut out, reference.value) {
                    rest = &rest[end + 1..];
                    continue;
                }
                return Err("JSX numeric entity is outside Unicode range");
            }
            if let Some(value) = lumen_common::entities::xhtml(entity) {
                out.push_str(value);
                rest = &rest[end + 1..];
                continue;
            }
        }
        out.push('&');
        rest = &rest[1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn normalize_attribute(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n");
    let mut chars = normalized.chars().peekable();
    let mut out = String::with_capacity(text.len());
    while let Some(c) = chars.next() {
        if c == '\n' && chars.peek().is_some_and(|c| c.is_whitespace()) {
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}
