//! CSS Syntax section 9: substituted text that would re-tokenize differently once its neighbours
//! are adjacent gets an empty comment between them.

use alloc::string::String;

#[derive(Clone, Copy, PartialEq)]
enum Tail {
    Ident,
    Name,
    Number,
    Minus,
    Hash,
    At,
    Dot,
    Slash,
    Other,
}

#[derive(Clone, Copy, PartialEq)]
enum Head {
    Ident,
    Minus,
    Cdc,
    Paren,
    Numeric,
    Percent,
    Star,
    Other,
}

fn name_char(ch: char) -> bool { ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || !ch.is_ascii() }

fn ident_start(ch: char) -> bool { ch.is_ascii_alphabetic() || ch == '_' || !ch.is_ascii() || ch == '\\' }

fn tail(text: &str) -> Tail {
    let Some(last) = text.chars().next_back() else { return Tail::Other };
    match last {
        '#' => return Tail::Hash,
        '@' => return Tail::At,
        '.' | '+' => return Tail::Dot,
        '/' => return Tail::Slash,
        _ => {}
    }
    if !name_char(last) {
        return Tail::Other;
    }
    let start = text.char_indices().rev().take_while(|(_, ch)| name_char(*ch)).last().map_or(text.len(), |(at, _)| at);
    let run = &text[start..];
    let before = text[..start].chars().next_back();
    if before == Some('\\') {
        return Tail::Other;
    }
    if run == "-" {
        return Tail::Minus;
    }
    let digits = run.strip_prefix('-').filter(|rest| rest.starts_with(|ch: char| ch.is_ascii_digit())).unwrap_or(run);
    if digits.starts_with(|ch: char| ch.is_ascii_digit()) {
        let rest = digits.trim_start_matches(|ch: char| ch.is_ascii_digit());
        let exponent = rest.strip_prefix(['e', 'E']).is_some_and(|tail| {
            let tail = tail.strip_prefix('-').unwrap_or(tail);
            !tail.is_empty() && tail.bytes().all(|byte| byte.is_ascii_digit())
        });
        return if rest.is_empty() || exponent { Tail::Number } else { Tail::Name };
    }
    match before {
        Some('@') | Some('#') => Tail::Name,
        _ => Tail::Ident,
    }
}

fn head(text: &str) -> Head {
    let mut chars = text.chars();
    let Some(first) = chars.next() else { return Head::Other };
    let second = chars.next();
    let third = chars.next();
    match first {
        '(' => Head::Paren,
        '%' => Head::Percent,
        '*' => Head::Star,
        '-' => {
            if second == Some('-') && third == Some('>') {
                Head::Cdc
            } else if second.is_some_and(|ch| ch.is_ascii_digit()) || (second == Some('.') && third.is_some_and(|ch| ch.is_ascii_digit())) {
                Head::Numeric
            } else if second.is_some_and(|ch| ident_start(ch) || ch == '-') {
                Head::Ident
            } else {
                Head::Minus
            }
        }
        '+' if second.is_some_and(|ch| ch.is_ascii_digit()) || (second == Some('.') && third.is_some_and(|ch| ch.is_ascii_digit())) => Head::Numeric,
        '.' if second.is_some_and(|ch| ch.is_ascii_digit()) => Head::Numeric,
        ch if ch.is_ascii_digit() => Head::Numeric,
        ch if ident_start(ch) => Head::Ident,
        _ => Head::Other,
    }
}

/// Whether `next` placed directly after `out` would merge with its last token.
pub(super) fn needs_comment(out: &str, next: &str) -> bool {
    let next = head(next);
    if next == Head::Other {
        return false;
    }
    match tail(out) {
        Tail::Ident => matches!(next, Head::Ident | Head::Minus | Head::Numeric | Head::Cdc | Head::Paren),
        Tail::Name => matches!(next, Head::Ident | Head::Minus | Head::Numeric | Head::Cdc),
        Tail::Hash | Tail::Minus => matches!(next, Head::Ident | Head::Minus | Head::Numeric),
        Tail::Number => matches!(next, Head::Ident | Head::Numeric | Head::Percent),
        Tail::At => matches!(next, Head::Ident | Head::Minus),
        Tail::Dot => next == Head::Numeric,
        Tail::Slash => next == Head::Star,
        Tail::Other => false,
    }
}

/// Appends `next` to `out`, separated by a comment when the two would otherwise merge.
pub(super) fn push_joined(out: &mut String, next: &str) {
    if needs_comment(out, next) {
        out.push_str("/**/");
    }
    out.push_str(next);
}
