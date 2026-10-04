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
            let decoded = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .and_then(|n| u32::from_str_radix(n, 16).ok())
                .or_else(|| entity.strip_prefix('#').and_then(|n| n.parse().ok()));
            if let Some(c) = decoded {
                if lumen_common::smuggle::push_code_point(&mut out, c) {
                    rest = &rest[end + 1..];
                    continue;
                }
                return Err("JSX numeric entity is outside Unicode range");
            }
            let numeric = entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit())
                })
                || entity.strip_prefix('#').is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
                });
            if numeric {
                return Err("JSX numeric entity is outside Unicode range");
            }
            if let Some(value) = named_entity(entity) {
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

// JSX uses the XHTML named character references, including apos.
fn named_entity(entity: &str) -> Option<&'static str> {
    match entity {
        "AElig" => Some("\u{c6}"),
        "Aacute" => Some("\u{c1}"),
        "Acirc" => Some("\u{c2}"),
        "Agrave" => Some("\u{c0}"),
        "Alpha" => Some("\u{391}"),
        "Aring" => Some("\u{c5}"),
        "Atilde" => Some("\u{c3}"),
        "Auml" => Some("\u{c4}"),
        "Beta" => Some("\u{392}"),
        "Ccedil" => Some("\u{c7}"),
        "Chi" => Some("\u{3a7}"),
        "Dagger" => Some("\u{2021}"),
        "Delta" => Some("\u{394}"),
        "ETH" => Some("\u{d0}"),
        "Eacute" => Some("\u{c9}"),
        "Ecirc" => Some("\u{ca}"),
        "Egrave" => Some("\u{c8}"),
        "Epsilon" => Some("\u{395}"),
        "Eta" => Some("\u{397}"),
        "Euml" => Some("\u{cb}"),
        "Gamma" => Some("\u{393}"),
        "Iacute" => Some("\u{cd}"),
        "Icirc" => Some("\u{ce}"),
        "Igrave" => Some("\u{cc}"),
        "Iota" => Some("\u{399}"),
        "Iuml" => Some("\u{cf}"),
        "Kappa" => Some("\u{39a}"),
        "Lambda" => Some("\u{39b}"),
        "Mu" => Some("\u{39c}"),
        "Ntilde" => Some("\u{d1}"),
        "Nu" => Some("\u{39d}"),
        "OElig" => Some("\u{152}"),
        "Oacute" => Some("\u{d3}"),
        "Ocirc" => Some("\u{d4}"),
        "Ograve" => Some("\u{d2}"),
        "Omega" => Some("\u{3a9}"),
        "Omicron" => Some("\u{39f}"),
        "Oslash" => Some("\u{d8}"),
        "Otilde" => Some("\u{d5}"),
        "Ouml" => Some("\u{d6}"),
        "Phi" => Some("\u{3a6}"),
        "Pi" => Some("\u{3a0}"),
        "Prime" => Some("\u{2033}"),
        "Psi" => Some("\u{3a8}"),
        "Rho" => Some("\u{3a1}"),
        "Scaron" => Some("\u{160}"),
        "Sigma" => Some("\u{3a3}"),
        "THORN" => Some("\u{de}"),
        "Tau" => Some("\u{3a4}"),
        "Theta" => Some("\u{398}"),
        "Uacute" => Some("\u{da}"),
        "Ucirc" => Some("\u{db}"),
        "Ugrave" => Some("\u{d9}"),
        "Upsilon" => Some("\u{3a5}"),
        "Uuml" => Some("\u{dc}"),
        "Xi" => Some("\u{39e}"),
        "Yacute" => Some("\u{dd}"),
        "Yuml" => Some("\u{178}"),
        "Zeta" => Some("\u{396}"),
        "aacute" => Some("\u{e1}"),
        "acirc" => Some("\u{e2}"),
        "acute" => Some("\u{b4}"),
        "aelig" => Some("\u{e6}"),
        "agrave" => Some("\u{e0}"),
        "alefsym" => Some("\u{2135}"),
        "alpha" => Some("\u{3b1}"),
        "amp" => Some("\u{26}"),
        "and" => Some("\u{2227}"),
        "ang" => Some("\u{2220}"),
        "apos" => Some("\u{27}"),
        "aring" => Some("\u{e5}"),
        "asymp" => Some("\u{2248}"),
        "atilde" => Some("\u{e3}"),
        "auml" => Some("\u{e4}"),
        "bdquo" => Some("\u{201e}"),
        "beta" => Some("\u{3b2}"),
        "brvbar" => Some("\u{a6}"),
        "bull" => Some("\u{2022}"),
        "cap" => Some("\u{2229}"),
        "ccedil" => Some("\u{e7}"),
        "cedil" => Some("\u{b8}"),
        "cent" => Some("\u{a2}"),
        "chi" => Some("\u{3c7}"),
        "circ" => Some("\u{2c6}"),
        "clubs" => Some("\u{2663}"),
        "cong" => Some("\u{2245}"),
        "copy" => Some("\u{a9}"),
        "crarr" => Some("\u{21b5}"),
        "cup" => Some("\u{222a}"),
        "curren" => Some("\u{a4}"),
        "dArr" => Some("\u{21d3}"),
        "dagger" => Some("\u{2020}"),
        "darr" => Some("\u{2193}"),
        "deg" => Some("\u{b0}"),
        "delta" => Some("\u{3b4}"),
        "diams" => Some("\u{2666}"),
        "divide" => Some("\u{f7}"),
        "eacute" => Some("\u{e9}"),
        "ecirc" => Some("\u{ea}"),
        "egrave" => Some("\u{e8}"),
        "empty" => Some("\u{2205}"),
        "emsp" => Some("\u{2003}"),
        "ensp" => Some("\u{2002}"),
        "epsilon" => Some("\u{3b5}"),
        "equiv" => Some("\u{2261}"),
        "eta" => Some("\u{3b7}"),
        "eth" => Some("\u{f0}"),
        "euml" => Some("\u{eb}"),
        "euro" => Some("\u{20ac}"),
        "exist" => Some("\u{2203}"),
        "fnof" => Some("\u{192}"),
        "forall" => Some("\u{2200}"),
        "frac12" => Some("\u{bd}"),
        "frac14" => Some("\u{bc}"),
        "frac34" => Some("\u{be}"),
        "frasl" => Some("\u{2044}"),
        "gamma" => Some("\u{3b3}"),
        "ge" => Some("\u{2265}"),
        "gt" => Some("\u{3e}"),
        "hArr" => Some("\u{21d4}"),
        "harr" => Some("\u{2194}"),
        "hearts" => Some("\u{2665}"),
        "hellip" => Some("\u{2026}"),
        "iacute" => Some("\u{ed}"),
        "icirc" => Some("\u{ee}"),
        "iexcl" => Some("\u{a1}"),
        "igrave" => Some("\u{ec}"),
        "image" => Some("\u{2111}"),
        "infin" => Some("\u{221e}"),
        "int" => Some("\u{222b}"),
        "iota" => Some("\u{3b9}"),
        "iquest" => Some("\u{bf}"),
        "isin" => Some("\u{2208}"),
        "iuml" => Some("\u{ef}"),
        "kappa" => Some("\u{3ba}"),
        "lArr" => Some("\u{21d0}"),
        "lambda" => Some("\u{3bb}"),
        "lang" => Some("\u{2329}"),
        "laquo" => Some("\u{ab}"),
        "larr" => Some("\u{2190}"),
        "lceil" => Some("\u{2308}"),
        "ldquo" => Some("\u{201c}"),
        "le" => Some("\u{2264}"),
        "lfloor" => Some("\u{230a}"),
        "lowast" => Some("\u{2217}"),
        "loz" => Some("\u{25ca}"),
        "lrm" => Some("\u{200e}"),
        "lsaquo" => Some("\u{2039}"),
        "lsquo" => Some("\u{2018}"),
        "lt" => Some("\u{3c}"),
        "macr" => Some("\u{af}"),
        "mdash" => Some("\u{2014}"),
        "micro" => Some("\u{b5}"),
        "middot" => Some("\u{b7}"),
        "minus" => Some("\u{2212}"),
        "mu" => Some("\u{3bc}"),
        "nabla" => Some("\u{2207}"),
        "nbsp" => Some("\u{a0}"),
        "ndash" => Some("\u{2013}"),
        "ne" => Some("\u{2260}"),
        "ni" => Some("\u{220b}"),
        "not" => Some("\u{ac}"),
        "notin" => Some("\u{2209}"),
        "nsub" => Some("\u{2284}"),
        "ntilde" => Some("\u{f1}"),
        "nu" => Some("\u{3bd}"),
        "oacute" => Some("\u{f3}"),
        "ocirc" => Some("\u{f4}"),
        "oelig" => Some("\u{153}"),
        "ograve" => Some("\u{f2}"),
        "oline" => Some("\u{203e}"),
        "omega" => Some("\u{3c9}"),
        "omicron" => Some("\u{3bf}"),
        "oplus" => Some("\u{2295}"),
        "or" => Some("\u{2228}"),
        "ordf" => Some("\u{aa}"),
        "ordm" => Some("\u{ba}"),
        "oslash" => Some("\u{f8}"),
        "otilde" => Some("\u{f5}"),
        "otimes" => Some("\u{2297}"),
        "ouml" => Some("\u{f6}"),
        "para" => Some("\u{b6}"),
        "part" => Some("\u{2202}"),
        "permil" => Some("\u{2030}"),
        "perp" => Some("\u{22a5}"),
        "phi" => Some("\u{3c6}"),
        "pi" => Some("\u{3c0}"),
        "piv" => Some("\u{3d6}"),
        "plusmn" => Some("\u{b1}"),
        "pound" => Some("\u{a3}"),
        "prime" => Some("\u{2032}"),
        "prod" => Some("\u{220f}"),
        "prop" => Some("\u{221d}"),
        "psi" => Some("\u{3c8}"),
        "quot" => Some("\u{22}"),
        "rArr" => Some("\u{21d2}"),
        "radic" => Some("\u{221a}"),
        "rang" => Some("\u{232a}"),
        "raquo" => Some("\u{bb}"),
        "rarr" => Some("\u{2192}"),
        "rceil" => Some("\u{2309}"),
        "rdquo" => Some("\u{201d}"),
        "real" => Some("\u{211c}"),
        "reg" => Some("\u{ae}"),
        "rfloor" => Some("\u{230b}"),
        "rho" => Some("\u{3c1}"),
        "rlm" => Some("\u{200f}"),
        "rsaquo" => Some("\u{203a}"),
        "rsquo" => Some("\u{2019}"),
        "sbquo" => Some("\u{201a}"),
        "scaron" => Some("\u{161}"),
        "sdot" => Some("\u{22c5}"),
        "sect" => Some("\u{a7}"),
        "shy" => Some("\u{ad}"),
        "sigma" => Some("\u{3c3}"),
        "sigmaf" => Some("\u{3c2}"),
        "sim" => Some("\u{223c}"),
        "spades" => Some("\u{2660}"),
        "sub" => Some("\u{2282}"),
        "sube" => Some("\u{2286}"),
        "sum" => Some("\u{2211}"),
        "sup" => Some("\u{2283}"),
        "sup1" => Some("\u{b9}"),
        "sup2" => Some("\u{b2}"),
        "sup3" => Some("\u{b3}"),
        "supe" => Some("\u{2287}"),
        "szlig" => Some("\u{df}"),
        "tau" => Some("\u{3c4}"),
        "there4" => Some("\u{2234}"),
        "theta" => Some("\u{3b8}"),
        "thetasym" => Some("\u{3d1}"),
        "thinsp" => Some("\u{2009}"),
        "thorn" => Some("\u{fe}"),
        "tilde" => Some("\u{2dc}"),
        "times" => Some("\u{d7}"),
        "trade" => Some("\u{2122}"),
        "uArr" => Some("\u{21d1}"),
        "uacute" => Some("\u{fa}"),
        "uarr" => Some("\u{2191}"),
        "ucirc" => Some("\u{fb}"),
        "ugrave" => Some("\u{f9}"),
        "uml" => Some("\u{a8}"),
        "upsih" => Some("\u{3d2}"),
        "upsilon" => Some("\u{3c5}"),
        "uuml" => Some("\u{fc}"),
        "weierp" => Some("\u{2118}"),
        "xi" => Some("\u{3be}"),
        "yacute" => Some("\u{fd}"),
        "yen" => Some("\u{a5}"),
        "yuml" => Some("\u{ff}"),
        "zeta" => Some("\u{3b6}"),
        "zwj" => Some("\u{200d}"),
        "zwnj" => Some("\u{200c}"),
        _ => None,
    }
}
