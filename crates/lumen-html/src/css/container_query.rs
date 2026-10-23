//! `@container` preludes: `<container-condition>#` where a condition is an
//! optional container name followed by an optional container query. Parsing
//! validates the grammar and serializes the canonical text used by the
//! CSSContainerRule interface; features this module does not know are kept as
//! general-enclosed terms.
use alloc::string::String;
use alloc::vec::Vec;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Condition {
    pub name: String,
    pub query: String,
}

impl Condition {
    pub fn serialize(&self) -> String {
        match (self.name.is_empty(), self.query.is_empty()) {
            (_, true) => self.name.clone(),
            (true, false) => self.query.clone(),
            (false, false) => alloc::format!("{} {}", self.name, self.query),
        }
    }
}

/// Conditions of `@container <prelude> {}`; `prelude` is the text after the
/// at-keyword. `None` when the prelude is invalid.
pub fn parse_prelude(prelude: &str) -> Option<Vec<Condition>> {
    let segments = super::top_level_split(prelude, b',', usize::MAX)?;
    let mut conditions = Vec::with_capacity(segments.len());
    for segment in segments {
        conditions.push(parse_condition(segment)?);
    }
    (!conditions.is_empty()).then_some(conditions)
}

/// Canonical `conditionText` of a `@container` prelude.
pub fn serialize_prelude(prelude: &str) -> Option<String> {
    let conditions = parse_prelude(prelude)?;
    Some(conditions.iter().map(Condition::serialize).collect::<Vec<_>>().join(", "))
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Ident,
    Paren,
    Function,
    Other,
}

#[derive(Clone, Debug)]
pub(super) struct Token<'a> {
    pub(super) kind: Kind,
    pub(super) text: &'a str,
}

fn is_ident_start(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '_' || !ch.is_ascii()
}

fn ident_end(text: &str, start: usize) -> Option<usize> {
    let mut chars = text[start..].char_indices().peekable();
    let (_, first) = chars.next()?;
    let mut end;
    match first {
        '\\' => {
            let (offset, escaped) = chars.next()?;
            if escaped == '\n' {
                return None;
            }
            end = start + offset + escaped.len_utf8();
        }
        '-' => {
            let (offset, second) = chars.next()?;
            if second == '-' {
                end = start + offset + 1;
            } else if is_ident_start(second) {
                end = start + offset + second.len_utf8();
            } else if second == '\\' {
                let (escaped_offset, escaped) = chars.next()?;
                if escaped == '\n' {
                    return None;
                }
                end = start + escaped_offset + escaped.len_utf8();
            } else {
                return None;
            }
        }
        ch if is_ident_start(ch) => end = start + ch.len_utf8(),
        _ => return None,
    }
    while let Some((offset, ch)) = chars.next() {
        if super::syntax::is_name_char(ch) {
            end = start + offset + ch.len_utf8();
        } else if ch == '\\' {
            match chars.next() {
                Some((escaped_offset, escaped)) if escaped != '\n' => {
                    end = start + escaped_offset + escaped.len_utf8();
                }
                _ => break,
            }
        } else {
            break;
        }
    }
    Some(end)
}

fn group_end(text: &str, open: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut position = open;
    while position < bytes.len() {
        match bytes[position] {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(position + 1);
                }
            }
            b'"' | b'\'' => {
                let quote = bytes[position];
                position += 1;
                while position < bytes.len() && bytes[position] != quote {
                    if bytes[position] == b'\\' {
                        position += 1;
                    }
                    position += 1;
                }
            }
            b'\\' => position += 1,
            _ => {}
        }
        position += 1;
    }
    None
}

pub(super) fn tokenize(text: &str) -> Option<Vec<Token<'_>>> {
    let mut tokens = Vec::new();
    let mut position = 0;
    while position < text.len() {
        let rest = &text[position..];
        let ch = rest.chars().next()?;
        if ch.is_whitespace() {
            position += ch.len_utf8();
            continue;
        }
        if ch == '(' {
            let end = group_end(text, position)?;
            tokens.push(Token { kind: Kind::Paren, text: &text[position..end] });
            position = end;
            continue;
        }
        if let Some(end) = ident_end(text, position) {
            if text[end..].starts_with('(') {
                let close = group_end(text, end)?;
                tokens.push(Token { kind: Kind::Function, text: &text[position..close] });
                position = close;
            } else {
                tokens.push(Token { kind: Kind::Ident, text: &text[position..end] });
                position = end;
            }
            continue;
        }
        let start = position;
        while position < text.len() {
            let ch = text[position..].chars().next()?;
            if ch.is_whitespace() || ch == '(' {
                break;
            }
            position += ch.len_utf8();
        }
        if position == start {
            return None;
        }
        tokens.push(Token { kind: Kind::Other, text: &text[start..position] });
    }
    Some(tokens)
}

pub(super) fn keyword(token: &Token<'_>, expected: &str) -> bool {
    token.kind == Kind::Ident && token.text.eq_ignore_ascii_case(expected)
}

fn valid_container_name(text: &str) -> bool {
    let reserved = [
        "none", "and", "or", "not", "default", "inherit", "initial", "unset", "revert", "revert-layer",
    ];
    !reserved.iter().any(|word| text.eq_ignore_ascii_case(word))
}

fn parse_condition(segment: &str) -> Option<Condition> {
    let tokens = tokenize(segment)?;
    let first = tokens.first()?;
    let mut name = String::new();
    let mut rest = &tokens[..];
    if first.kind == Kind::Ident && !keyword(first, "not") {
        if !valid_container_name(first.text) {
            return None;
        }
        name = first.text.into();
        rest = &tokens[1..];
    }
    let query = if rest.is_empty() {
        if name.is_empty() {
            return None;
        }
        String::new()
    } else {
        serialize_query(rest, false)?
    };
    Some(Condition { name, query })
}

fn serialize_query(tokens: &[Token<'_>], style: bool) -> Option<String> {
    let first = tokens.first()?;
    if keyword(first, "not") {
        if tokens.len() != 2 {
            return None;
        }
        return Some(alloc::format!("not {}", serialize_in_parens(&tokens[1], style)?));
    }
    let mut output = serialize_in_parens(first, style)?;
    if tokens.len() == 1 {
        return Some(output);
    }
    if tokens.len() % 2 == 0 {
        return None;
    }
    let connective = if keyword(&tokens[1], "and") {
        "and"
    } else if keyword(&tokens[1], "or") {
        "or"
    } else {
        return None;
    };
    for pair in tokens[1..].chunks_exact(2) {
        if !keyword(&pair[0], connective) {
            return None;
        }
        output.push(' ');
        output.push_str(connective);
        output.push(' ');
        output.push_str(&serialize_in_parens(&pair[1], style)?);
    }
    Some(output)
}

pub(super) fn inner(token: &Token<'_>, prefix: usize) -> String {
    token.text[prefix + 1..token.text.len() - 1].trim().into()
}

fn serialize_in_parens(token: &Token<'_>, style: bool) -> Option<String> {
    match token.kind {
        Kind::Paren => {
            let content = if style { token.text[1..token.text.len() - 1].into() } else { inner(token, 0) };
            Some(alloc::format!("({})", serialize_feature_or_query(&content, style)))
        }
        Kind::Function => {
            let open = token.text.find('(')?;
            let name = &token.text[..open];
            let content = inner(token, open);
            if name.eq_ignore_ascii_case("style") {
                Some(alloc::format!("style({})", serialize_feature_or_query(&content, true)))
            } else if name.eq_ignore_ascii_case("scroll-state") || name.eq_ignore_ascii_case("anchored") {
                let name = name.to_ascii_lowercase();
                Some(alloc::format!("{name}({})", serialize_feature_or_query(&content, false)))
            } else {
                Some(collapse_whitespace(token.text))
            }
        }
        _ => None,
    }
}

fn serialize_feature_or_query(content: &str, style: bool) -> String {
    if let Some(tokens) = tokenize(content) {
        let condition_like = tokens.first().is_some_and(|first| {
            first.kind == Kind::Paren
                || (keyword(first, "not") && tokens.get(1).is_some_and(|next| matches!(next.kind, Kind::Paren | Kind::Function)))
        });
        if condition_like {
            if let Some(query) = serialize_query(&tokens, style) {
                return query;
            }
        }
    }
    if style {
        serialize_style_feature(content)
    } else {
        serialize_feature(content)
    }
}

/// A style feature keeps the author's value text: a custom property
/// declaration is normalized around the colon only, and a declaration of
/// anything else is general-enclosed text kept as written.
fn serialize_style_feature(content: &str) -> String {
    let trimmed = content.trim();
    if let Some(colon) = trimmed.find(':') {
        let name = trimmed[..colon].trim();
        if !name.contains(['<', '>', '=', '(']) {
            if name.starts_with("--") {
                return alloc::format!("{name}: {}", trimmed[colon + 1..].trim());
            }
            return content.into();
        }
    }
    serialize_feature(content)
}

pub(super) fn collapse_whitespace(text: &str) -> String {
    crate::svg_text_path::collapse_whitespace_with(text, char::is_whitespace)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Piece {
    Word(String),
    Colon,
    Operator(&'static str),
}

pub(super) fn pieces(content: &str) -> Option<Vec<Piece>> {
    let bytes = content.as_bytes();
    let mut output = Vec::new();
    let mut position = 0;
    while position < bytes.len() {
        let ch = content[position..].chars().next()?;
        if ch.is_whitespace() {
            position += ch.len_utf8();
            continue;
        }
        if ch == ':' {
            output.push(Piece::Colon);
            position += 1;
            continue;
        }
        if matches!(ch, '<' | '>' | '=') {
            let start = position;
            while position < bytes.len() && matches!(bytes[position], b'<' | b'>' | b'=') {
                position += 1;
            }
            output.push(Piece::Operator(match &content[start..position] {
                "<" => "<",
                "<=" => "<=",
                ">" => ">",
                ">=" => ">=",
                "=" => "=",
                _ => return None,
            }));
            continue;
        }
        let start = position;
        if let Some(end) = ident_end(content, position) {
            position = end;
        }
        loop {
            if position >= bytes.len() {
                break;
            }
            let ch = content[position..].chars().next()?;
            if ch == '(' {
                position = group_end(content, position)?;
                continue;
            }
            if ch.is_whitespace() || matches!(ch, ':' | '<' | '>' | '=') {
                break;
            }
            position += ch.len_utf8();
        }
        if position == start {
            return None;
        }
        output.push(Piece::Word(collapse_whitespace(&content[start..position])));
    }
    Some(output)
}

fn feature_word(word: &str) -> String {
    let is_name = word.chars().next().is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '-')
        && !word.contains('(')
        && !word.starts_with("--");
    if is_name {
        word.to_ascii_lowercase()
    } else {
        word.into()
    }
}

fn serialize_feature(content: &str) -> String {
    let fallback = || collapse_whitespace(content);
    let Some(parts) = pieces(content) else {
        return fallback();
    };
    match parts.as_slice() {
        [Piece::Word(name)] => feature_word(name),
        [Piece::Word(name), Piece::Colon, rest @ ..] if !rest.is_empty() => {
            let mut value = String::new();
            for piece in rest {
                match piece {
                    Piece::Word(word) => {
                        if !value.is_empty() {
                            value.push(' ');
                        }
                        value.push_str(word);
                    }
                    _ => return fallback(),
                }
            }
            alloc::format!("{}: {value}", feature_word(name))
        }
        _ => serialize_range(&parts).unwrap_or_else(fallback),
    }
}

fn serialize_range(parts: &[Piece]) -> Option<String> {
    let valid_shape = matches!(
        parts,
        [Piece::Word(_), Piece::Operator(_), Piece::Word(_)]
            | [Piece::Word(_), Piece::Operator(_), Piece::Word(_), Piece::Operator(_), Piece::Word(_)]
    );
    if !valid_shape {
        return None;
    }
    if let [_, Piece::Operator(left), _, Piece::Operator(right), _] = parts {
        let ascending = |operator: &str| matches!(operator, "<" | "<=");
        let descending = |operator: &str| matches!(operator, ">" | ">=");
        if !((ascending(left) && ascending(right)) || (descending(left) && descending(right))) {
            return None;
        }
    }
    let mut output = String::new();
    for piece in parts {
        if !output.is_empty() {
            output.push(' ');
        }
        match piece {
            Piece::Word(word) => output.push_str(&feature_word(word)),
            Piece::Operator(operator) => output.push_str(operator),
            Piece::Colon => return None,
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn serialized(prelude: &str) -> Option<String> {
        serialize_prelude(prelude)
    }

    #[test]
    fn serializes_features_and_names() {
        assert_eq!(serialized("(  wiDTh  )").as_deref(), Some("(width)"));
        assert_eq!(serialized("(width:100px)").as_deref(), Some("(width: 100px)"));
        assert_eq!(serialized("(100px>WIDTH>10px)").as_deref(), Some("(100px > width > 10px)"));
        assert_eq!(serialized("NAMe   (inline-sizE  < 1300px )").as_deref(), Some("NAMe (inline-size < 1300px)"));
        assert_eq!(serialized("(width),(height) ,--foo ,--bar").as_deref(), Some("(width), (height), --foo, --bar"));
        assert_eq!(serialized("STYLE(--foo: bar)").as_deref(), Some("style(--foo: bar)"));
    }

    #[test]
    fn rejects_invalid_preludes() {
        for prelude in ["", "(width),", ",(width)", "name screen", "none (width)", "and not (width)", "1px not (width)"] {
            assert_eq!(serialized(prelude), None, "{prelude}");
        }
    }
}
