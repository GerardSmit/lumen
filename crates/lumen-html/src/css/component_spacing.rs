//! Restores the whitespace minified stylesheets drop between component values
//! that CSS Syntax still tokenizes separately (`.3em.3em`, `1px#fff`,
//! `rgba(0,0,0,.5)0 1px`), so the whitespace-based value parsers see them.
use alloc::string::String;

/// Returns the respaced value when at least one boundary needed a separator.
/// Function and bracket contents, strings, escapes and comments are untouched.
pub(super) fn separate(name: &str, raw: &str) -> Option<String> {
    if name.starts_with("--")
        || name == "unicode-range"
        || raw.bytes().any(|byte| matches!(byte, b'"' | b'\'' | b'\\'))
        || raw.contains("/*")
    {
        return None;
    }
    let bytes = raw.as_bytes();
    let mut output = String::new();
    let mut copied = 0;
    let mut depth = 0usize;
    let mut position = 0;
    while position < bytes.len() {
        let byte = bytes[position];
        if depth > 0 {
            match byte {
                b'(' | b'[' | b'{' => depth += 1,
                b')' | b']' | b'}' => {
                    depth -= 1;
                    if depth == 0 && needs_space_after_block(bytes, position + 1) {
                        output.push_str(&raw[copied..position + 1]);
                        output.push(' ');
                        copied = position + 1;
                    }
                }
                _ => {}
            }
            position += 1;
            continue;
        }
        match byte {
            b'(' | b'[' | b'{' => {
                depth = 1;
                position += 1;
            }
            b'#' => {
                position += 1;
                while bytes.get(position).is_some_and(|&b| is_name_byte(b)) {
                    position += 1;
                }
            }
            _ if number_len(&bytes[position..]) > 0 => {
                let mut end = position + number_len(&bytes[position..]);
                let unit = unit_len(&bytes[end..]);
                end += unit;
                let split = match bytes.get(end) {
                    Some(b'.') => bytes.get(end + 1).is_some_and(u8::is_ascii_digit),
                    Some(b'#') => true,
                    Some(b'+') => number_len(&bytes[end..]) > 0,
                    Some(b'-') => unit == 0 && number_len(&bytes[end..]) > 0,
                    _ => false,
                };
                if split {
                    output.push_str(&raw[copied..end]);
                    output.push(' ');
                    copied = end;
                }
                position = end;
            }
            _ if is_ident_start(bytes, position) => {
                let mut end = position + 1;
                while bytes.get(end).is_some_and(|&b| is_name_byte(b)) {
                    end += 1;
                }
                if bytes.get(end) == Some(&b'#') {
                    output.push_str(&raw[copied..end]);
                    output.push(' ');
                    copied = end;
                }
                position = end;
            }
            _ => position += 1,
        }
    }
    if output.is_empty() {
        return None;
    }
    output.push_str(&raw[copied..]);
    Some(output)
}

fn is_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_') || byte >= 0x80
}

fn is_ident_start(bytes: &[u8], position: usize) -> bool {
    match bytes[position] {
        byte if byte.is_ascii_alphabetic() || byte == b'_' || byte >= 0x80 => true,
        b'-' => bytes
            .get(position + 1)
            .is_some_and(|&next| next.is_ascii_alphabetic() || matches!(next, b'_' | b'-') || next >= 0x80),
        _ => false,
    }
}

fn needs_space_after_block(bytes: &[u8], position: usize) -> bool {
    match bytes.get(position) {
        Some(&byte) => byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'#' | b'+' | b'-' | b'_') || byte >= 0x80,
        None => false,
    }
}

/// Length of a number token (sign, digits, fraction, exponent) at the start, or 0.
fn number_len(bytes: &[u8]) -> usize {
    let mut end = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        end = 1;
    }
    let digits = |from: usize| bytes[from.min(bytes.len())..].iter().take_while(|byte| byte.is_ascii_digit()).count();
    let integer = digits(end);
    let mut cursor = end + integer;
    if bytes.get(cursor) == Some(&b'.') && digits(cursor + 1) > 0 {
        cursor += 1 + digits(cursor + 1);
    } else if integer == 0 {
        return 0;
    }
    if matches!(bytes.get(cursor), Some(b'e' | b'E')) {
        let sign = usize::from(matches!(bytes.get(cursor + 1), Some(b'+' | b'-')));
        let exponent = digits(cursor + 1 + sign);
        if exponent > 0 {
            cursor += 1 + sign + exponent;
        }
    }
    cursor
}

/// Length of the unit that follows a number: `%` or an identifier sequence.
fn unit_len(bytes: &[u8]) -> usize {
    if bytes.first() == Some(&b'%') {
        return 1;
    }
    if !bytes.is_empty() && is_ident_start(bytes, 0) {
        return 1 + bytes[1..].iter().take_while(|&&byte| is_name_byte(byte)).count();
    }
    0
}

#[cfg(test)]
mod tests {
    use super::{separate, String};

    fn sep(raw: &str) -> String {
        separate("border-width", raw).unwrap_or_else(|| String::from(raw))
    }

    #[test]
    fn splits_adjacent_tokens() {
        assert_eq!(sep(".3em.3em 0"), ".3em .3em 0");
        assert_eq!(sep("1px.5em"), "1px .5em");
        assert_eq!(sep("2px.5px 3px"), "2px .5px 3px");
        assert_eq!(sep("1px#fff"), "1px #fff");
        assert_eq!(sep("1px+2px"), "1px +2px");
        assert_eq!(sep("10-5"), "10 -5");
        assert_eq!(sep("0 1px rgba(0,0,0,.5)0 1px"), "0 1px rgba(0,0,0,.5) 0 1px");
        assert_eq!(sep("url(x)no-repeat"), "url(x) no-repeat");
        assert_eq!(sep("1px solid#fff"), "1px solid #fff");
    }

    #[test]
    fn keeps_single_tokens() {
        for raw in ["10px-5px", "1.5em", "1e-3", "calc(1px+2px)", "#fff", "rgb(0 0 0/.5)", "12px/1.5", "12px/1.5 x", "1/2", "1 / span 2", "url(a.png)", "url(a.png) 0 0/cover"] {
            assert!(separate("margin", raw).is_none(), "{raw}");
        }
        assert!(separate("--x", ".3em.3em").is_none());
        assert!(separate("unicode-range", "U+0025-00FF").is_none());
    }
}
