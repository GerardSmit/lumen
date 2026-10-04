//! String literal scanning and escape decoding, shared by the tokenizer and the f-string parser.

pub(crate) const MAX_FSTRING_DEPTH: u32 = 50;

#[derive(Clone, Copy)]
pub(crate) struct Quote {
    pub ch: char,
    pub triple: bool,
}

#[derive(Debug)]
pub(crate) enum ScanErr {
    Unterminated,
    Msg(String),
}

fn msg<T>(m: &str) -> Result<T, ScanErr> {
    Err(ScanErr::Msg(m.to_string()))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FieldEnd {
    Close,
    Conv,
    Spec,
    Debug,
}

/// Finds the closing quote of a string whose body starts at `i`; returns its index.
pub(crate) fn find_end(
    s: &[char],
    mut i: usize,
    q: Quote,
    fmt: bool,
    raw: bool,
    depth: u32,
) -> Result<usize, ScanErr> {
    if depth > MAX_FSTRING_DEPTH {
        return msg("too many nested f-strings");
    }
    loop {
        let Some(&c) = s.get(i) else {
            return Err(ScanErr::Unterminated);
        };
        match c {
            '\\' => {
                let Some(&n) = s.get(i + 1) else {
                    return Err(ScanErr::Unterminated);
                };
                if fmt && (n == '{' || n == '}') {
                    i += 1;
                } else if fmt && !raw && n == 'N' && s.get(i + 2) == Some(&'{') {
                    i += 3;
                    while i < s.len() && s[i] != '}' && s[i] != '\n' {
                        i += 1;
                    }
                    i += 1;
                } else {
                    i += 2;
                }
            }
            '\n' if !q.triple => return Err(ScanErr::Unterminated),
            _ if c == q.ch => {
                if !q.triple || (s.get(i + 1) == Some(&q.ch) && s.get(i + 2) == Some(&q.ch)) {
                    return Ok(i);
                }
                i += 1;
            }
            '{' if fmt => {
                if s.get(i + 1) == Some(&'{') {
                    i += 2;
                } else {
                    i = scan_field(s, i + 1, q, raw, depth)?;
                }
            }
            '}' if fmt => {
                if s.get(i + 1) == Some(&'}') {
                    i += 2;
                } else {
                    return msg("f-string: single '}' is not allowed");
                }
            }
            _ => i += 1,
        }
    }
}

/// Scans a replacement field starting just after its `{`; returns the index after its `}`.
fn scan_field(s: &[char], i: usize, q: Quote, raw: bool, depth: u32) -> Result<usize, ScanErr> {
    let (end, kind) = scan_field_expr(s, i, depth)?;
    let mut j = end;
    if kind == FieldEnd::Debug {
        j = skip_debug_ws(s, j);
    }
    if s.get(j) == Some(&'!') {
        j += 1;
        while s
            .get(j)
            .is_some_and(|c| c.is_alphanumeric() || *c == '_' || c.is_whitespace())
        {
            j += 1;
        }
    }
    if s.get(j) == Some(&':') {
        j = scan_spec(s, j + 1, q, raw, depth)?;
    }
    if s.get(j) != Some(&'}') {
        return msg("f-string: expecting '}'");
    }
    Ok(j + 1)
}

/// Index just after the `=` at `j` and any whitespace following it.
pub(crate) fn skip_debug_ws(s: &[char], j: usize) -> usize {
    let mut j = j + 1;
    loop {
        match s.get(j) {
            Some(c) if c.is_whitespace() => j += 1,
            Some('#') => {
                while s.get(j).is_some_and(|&c| c != '\n') {
                    j += 1;
                }
            }
            _ => return j,
        }
    }
}

/// Scans a format spec starting after its `:`; returns the index of the closing `}`.
fn scan_spec(s: &[char], mut i: usize, q: Quote, raw: bool, depth: u32) -> Result<usize, ScanErr> {
    loop {
        let Some(&c) = s.get(i) else {
            return msg("f-string: expecting '}'");
        };
        match c {
            '}' => return Ok(i),
            '{' => i = scan_field(s, i + 1, q, raw, depth + 1)?,
            '\\' => {
                let n = s.get(i + 1).copied();
                i += if matches!(n, Some('{') | Some('}')) {
                    1
                } else {
                    2
                };
            }
            '\n' if !q.triple => return Err(ScanErr::Unterminated),
            _ if c == q.ch => return msg("f-string: expecting '}'"),
            _ => i += 1,
        }
    }
}

/// Scans the expression part of a replacement field starting at `start` (just after `{`).
/// Returns the index of the char that ends the expression and how it ends.
pub(crate) fn scan_field_expr(
    s: &[char],
    start: usize,
    depth: u32,
) -> Result<(usize, FieldEnd), ScanErr> {
    let mut i = start;
    let mut stack: Vec<char> = Vec::new();
    while let Some(&c) = s.get(i) {
        match c {
            '\'' | '"' => {
                let mut j = i;
                while j > start && i - j < 2 && s[j - 1].is_ascii_alphabetic() {
                    j -= 1;
                }
                let prefix: String = s[j..i].iter().map(|c| c.to_ascii_lowercase()).collect();
                let fmt = prefix.contains('f');
                let raw = prefix.contains('r');
                let triple = s.get(i + 1) == Some(&c) && s.get(i + 2) == Some(&c);
                let body = i + if triple { 3 } else { 1 };
                let end = find_end(s, body, Quote { ch: c, triple }, fmt, raw, depth + 1)?;
                i = end + if triple { 3 } else { 1 };
                continue;
            }
            '(' => stack.push(')'),
            '[' => stack.push(']'),
            '{' => stack.push('}'),
            ')' | ']' | '}' => match stack.pop() {
                Some(e) if e == c => {}
                Some(_) => return msg("f-string: closing parenthesis does not match"),
                None if c == '}' => return Ok((i, FieldEnd::Close)),
                None => return msg(&format!("f-string: unmatched '{c}'")),
            },
            '#' => {
                while i < s.len() && s[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '!' if stack.is_empty() => {
                if s.get(i + 1) == Some(&'=') {
                    i += 2;
                    continue;
                }
                return Ok((i, FieldEnd::Conv));
            }
            ':' if stack.is_empty() => return Ok((i, FieldEnd::Spec)),
            '=' if stack.is_empty() => {
                if s.get(i + 1) == Some(&'=') {
                    i += 2;
                    continue;
                }
                if i == start || !matches!(s[i - 1], '<' | '>' | '=' | '!') {
                    return Ok((i, FieldEnd::Debug));
                }
            }
            _ => {}
        }
        i += 1;
    }
    msg("f-string: expecting '}'")
}

/// The Unicode name of `c` (never an alias); None for unnamed characters such as controls.
pub(crate) fn char_name(c: char) -> Option<String> {
    lumen_common::ucd::name(c as u32, lumen_common::ucd::Version::Current)
}

/// The character `\N{name}` denotes: a name or alias in any case (named sequences are not
/// characters).
pub(crate) fn lookup_name(name: &str) -> Option<char> {
    match lumen_common::ucd::lookup(name, lumen_common::ucd::Version::Current, false)?.as_slice() {
        [c] => char::from_u32(*c),
        _ => None,
    }
}

fn hex(s: &[char], i: usize, n: usize) -> Option<u32> {
    let digits = s.get(i..i + n)?;
    let mut v = 0u32;
    for d in digits {
        v = v.checked_mul(16)? + d.to_digit(16)?;
    }
    Some(v)
}

fn octal(s: &[char], i: &mut usize, first: char) -> u32 {
    let mut v = first as u32 - '0' as u32;
    for _ in 0..2 {
        match s.get(*i).and_then(|c| c.to_digit(8)) {
            Some(d) => {
                v = v * 8 + d;
                *i += 1;
            }
            None => break,
        }
    }
    v
}

pub(crate) fn decode_str(s: &[char], raw: bool) -> Result<String, String> {
    let mut out = String::with_capacity(s.len());
    if raw {
        out.extend(s);
        return Ok(out);
    }
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        i += 1;
        if c != '\\' {
            out.push(c);
            continue;
        }
        let Some(&e) = s.get(i) else {
            out.push('\\');
            break;
        };
        i += 1;
        match e {
            '\n' => {}
            '\\' | '\'' | '"' => out.push(e),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'f' => out.push('\x0c'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'v' => out.push('\x0b'),
            '0'..='7' => out.push(char::from_u32(octal(s, &mut i, e)).unwrap_or('\u{fffd}')),
            'x' | 'u' | 'U' => {
                let (n, what) = match e {
                    'x' => (2, "\\xXX"),
                    'u' => (4, "\\uXXXX"),
                    _ => (8, "\\UXXXXXXXX"),
                };
                let Some(v) = hex(s, i, n) else {
                    return Err(format!(
                        "(unicode error) 'unicodeescape' codec can't decode bytes: truncated {what} escape"
                    ));
                };
                i += n;
                if v > 0x10ffff {
                    return Err(
                        "(unicode error) 'unicodeescape' codec can't decode bytes: illegal Unicode character"
                            .into(),
                    );
                }
                lumen_common::smuggle::push_code_point(&mut out, v);
            }
            'N' => {
                let close = (s.get(i) == Some(&'{'))
                    .then(|| s[i..].iter().position(|&c| c == '}'))
                    .flatten();
                let Some(close) = close else {
                    return Err(
                        "(unicode error) 'unicodeescape' codec can't decode bytes: malformed \\N character escape"
                            .into(),
                    );
                };
                let name: String = s[i + 1..i + close].iter().collect();
                i += close + 1;
                match lookup_name(&name) {
                    Some(c) => out.push(c),
                    None => {
                        return Err(format!(
                            "(unicode error) 'unicodeescape' codec can't decode bytes: unknown Unicode character name '{name}'"
                        ))
                    }
                }
            }
            other => {
                out.push('\\');
                out.push(other);
            }
        }
    }
    Ok(out)
}

pub(crate) fn decode_bytes(s: &[char], raw: bool) -> Result<Vec<u8>, String> {
    if s.iter().any(|c| !c.is_ascii()) {
        return Err("bytes can only contain ASCII literal characters".into());
    }
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        i += 1;
        if c != '\\' || raw {
            out.push(c as u8);
            continue;
        }
        let Some(&e) = s.get(i) else {
            out.push(b'\\');
            break;
        };
        i += 1;
        match e {
            '\n' => {}
            '\\' | '\'' | '"' => out.push(e as u8),
            'a' => out.push(7),
            'b' => out.push(8),
            'f' => out.push(12),
            'n' => out.push(b'\n'),
            'r' => out.push(b'\r'),
            't' => out.push(b'\t'),
            'v' => out.push(11),
            '0'..='7' => out.push((octal(s, &mut i, e) & 0xff) as u8),
            'x' => {
                let Some(v) = hex(s, i, 2) else {
                    return Err("(value error) invalid \\x escape".into());
                };
                i += 2;
                out.push(v as u8);
            }
            other => {
                out.push(b'\\');
                out.push(other as u8);
            }
        }
    }
    Ok(out)
}
