//! Canonical spelling of plain numeric tokens in specified values: a leading
//! `+` is dropped, `.5` becomes `0.5`, trailing fraction zeros are removed and
//! negative zero becomes `0`.
use alloc::string::String;

/// Returns the rewritten value when at least one token changes. Values with
/// functions, strings, escapes or slashes are left alone.
pub(super) fn canonicalize(name: &str, raw: &str) -> Option<String> {
    if name.starts_with("--")
        || raw
            .bytes()
            .any(|byte| matches!(byte, b'(' | b'"' | b'\'' | b'\\' | b'/' | b'!' | b';' | b'#'))
    {
        return None;
    }
    if !tokens(raw).any(|(_, token)| parts(token).is_some_and(|parts| parts.changes())) {
        return None;
    }
    let mut output = String::with_capacity(raw.len());
    let mut last = 0;
    for (start, token) in tokens(raw) {
        output.push_str(&raw[last..start]);
        match parts(token) {
            Some(parts) if parts.changes() => parts.write(&mut output),
            _ => output.push_str(token),
        }
        last = start + token.len();
    }
    output.push_str(&raw[last..]);
    Some(output)
}

fn tokens(raw: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut start = None;
    let mut chars = raw.char_indices();
    let mut done = false;
    core::iter::from_fn(move || {
        if done {
            return None;
        }
        loop {
            match chars.next() {
                Some((index, ch)) if ch.is_ascii_whitespace() || ch == ',' => {
                    if let Some(begin) = start.take() {
                        return Some((begin, &raw[begin..index]));
                    }
                }
                Some((index, _)) => {
                    start.get_or_insert(index);
                }
                None => {
                    done = true;
                    return start.take().map(|begin| (begin, &raw[begin..]));
                }
            }
        }
    })
}

struct Parts<'a> {
    negative: bool,
    sign: bool,
    integer: &'a str,
    fraction: &'a str,
    unit: &'a str,
}

impl Parts<'_> {
    fn integer(&self) -> &str {
        self.integer.trim_start_matches('0')
    }

    fn fraction(&self) -> &str {
        self.fraction.trim_end_matches('0')
    }

    fn zero(&self) -> bool {
        self.integer().is_empty() && self.fraction().is_empty()
    }

    fn changes(&self) -> bool {
        (self.sign && !self.negative)
            || (self.negative && self.zero())
            || self.integer.is_empty()
            || (self.integer.len() > 1 && self.integer.starts_with('0'))
            || self.fraction().len() != self.fraction.len()
    }

    fn write(&self, output: &mut String) {
        if self.negative && !self.zero() {
            output.push('-');
        }
        let integer = self.integer();
        output.push_str(if integer.is_empty() { "0" } else { integer });
        let fraction = self.fraction();
        if !fraction.is_empty() {
            output.push('.');
            output.push_str(fraction);
        }
        output.push_str(self.unit);
    }
}

fn parts(token: &str) -> Option<Parts<'_>> {
    let bytes = token.as_bytes();
    let mut position = 0;
    let (negative, sign) = match bytes.first()? {
        b'-' => {
            position = 1;
            (true, true)
        }
        b'+' => {
            position = 1;
            (false, true)
        }
        _ => (false, false),
    };
    let integer_start = position;
    while bytes.get(position).is_some_and(u8::is_ascii_digit) {
        position += 1;
    }
    let integer = &token[integer_start..position];
    let mut fraction = "";
    if bytes.get(position) == Some(&b'.') {
        let fraction_start = position + 1;
        let mut end = fraction_start;
        while bytes.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end == fraction_start {
            return None;
        }
        fraction = &token[fraction_start..end];
        position = end;
    }
    if integer.is_empty() && fraction.is_empty() {
        return None;
    }
    let unit = &token[position..];
    if unit.bytes().any(|byte| !(byte.is_ascii_alphabetic() || byte == b'%')) {
        return None;
    }
    if matches!(unit.as_bytes().first(), Some(b'e' | b'E'))
        && unit.as_bytes().get(1).is_some_and(|byte| byte.is_ascii_digit() || *byte == b'-' || *byte == b'+')
    {
        return None;
    }
    Some(Parts { negative, sign, integer, fraction, unit })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_leading_dot_and_negative_zero() {
        assert_eq!(canonicalize("background-position", "5% -.5%").as_deref(), Some("5% -0.5%"));
        assert_eq!(canonicalize("width", ".1em").as_deref(), Some("0.1em"));
        assert_eq!(canonicalize("line-height", "-0").as_deref(), Some("0"));
        assert_eq!(canonicalize("width", "10px"), None);
        assert_eq!(canonicalize("width", "calc(.5px)"), None);
    }
}
