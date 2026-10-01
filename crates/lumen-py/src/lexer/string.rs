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

const NAMES: &[(&str, char)] = &[
    ("NULL", '\0'),
    ("SPACE", ' '),
    ("NO-BREAK SPACE", '\u{a0}'),
    ("EM SPACE", '\u{2003}'),
    ("EN SPACE", '\u{2002}'),
    ("THIN SPACE", '\u{2009}'),
    ("ZERO WIDTH SPACE", '\u{200b}'),
    ("ZERO WIDTH JOINER", '\u{200d}'),
    ("ZERO WIDTH NON-JOINER", '\u{200c}'),
    ("LINE FEED", '\n'),
    ("CARRIAGE RETURN", '\r'),
    ("CHARACTER TABULATION", '\t'),
    ("BULLET", '\u{2022}'),
    ("EM DASH", '\u{2014}'),
    ("EN DASH", '\u{2013}'),
    ("HYPHEN", '\u{2010}'),
    ("MINUS SIGN", '\u{2212}'),
    ("HORIZONTAL ELLIPSIS", '\u{2026}'),
    ("MIDDLE DOT", '\u{b7}'),
    ("DEGREE SIGN", '\u{b0}'),
    ("COPYRIGHT SIGN", '\u{a9}'),
    ("REGISTERED SIGN", '\u{ae}'),
    ("TRADE MARK SIGN", '\u{2122}'),
    ("EURO SIGN", '\u{20ac}'),
    ("POUND SIGN", '\u{a3}'),
    ("YEN SIGN", '\u{a5}'),
    ("CENT SIGN", '\u{a2}'),
    ("SECTION SIGN", '\u{a7}'),
    ("PILCROW SIGN", '\u{b6}'),
    ("MICRO SIGN", '\u{b5}'),
    ("PLUS-MINUS SIGN", '\u{b1}'),
    ("MULTIPLICATION SIGN", '\u{d7}'),
    ("DIVISION SIGN", '\u{f7}'),
    ("LEFT DOUBLE QUOTATION MARK", '\u{201c}'),
    ("RIGHT DOUBLE QUOTATION MARK", '\u{201d}'),
    ("LEFT SINGLE QUOTATION MARK", '\u{2018}'),
    ("RIGHT SINGLE QUOTATION MARK", '\u{2019}'),
    ("LEFT-POINTING DOUBLE ANGLE QUOTATION MARK", '\u{ab}'),
    ("RIGHT-POINTING DOUBLE ANGLE QUOTATION MARK", '\u{bb}'),
    ("RIGHTWARDS ARROW", '\u{2192}'),
    ("LEFTWARDS ARROW", '\u{2190}'),
    ("UPWARDS ARROW", '\u{2191}'),
    ("DOWNWARDS ARROW", '\u{2193}'),
    ("CHECK MARK", '\u{2713}'),
    ("BALLOT X", '\u{2717}'),
    ("BLACK STAR", '\u{2605}'),
    ("WHITE STAR", '\u{2606}'),
    ("SNOWMAN", '\u{2603}'),
    ("INFINITY", '\u{221e}'),
    ("NOT EQUAL TO", '\u{2260}'),
    ("LESS-THAN OR EQUAL TO", '\u{2264}'),
    ("GREATER-THAN OR EQUAL TO", '\u{2265}'),
    ("GREEK SMALL LETTER ALPHA", '\u{3b1}'),
    ("GREEK SMALL LETTER BETA", '\u{3b2}'),
    ("GREEK SMALL LETTER GAMMA", '\u{3b3}'),
    ("GREEK SMALL LETTER DELTA", '\u{3b4}'),
    ("GREEK SMALL LETTER MU", '\u{3bc}'),
    ("GREEK SMALL LETTER PI", '\u{3c0}'),
    ("GREEK SMALL LETTER SIGMA", '\u{3c3}'),
    ("GREEK SMALL LETTER OMEGA", '\u{3c9}'),
    ("GREEK CAPITAL LETTER OMEGA", '\u{3a9}'),
    ("GREEK CAPITAL LETTER SIGMA", '\u{3a3}'),
    ("GREEK CAPITAL LETTER DELTA", '\u{394}'),
    ("LATIN SMALL LETTER E WITH ACUTE", '\u{e9}'),
    ("LATIN SMALL LETTER A WITH GRAVE", '\u{e0}'),
    ("LATIN SMALL LETTER U WITH DIAERESIS", '\u{fc}'),
    ("LATIN SMALL LETTER SHARP S", '\u{df}'),
    ("LATIN CAPITAL LETTER A WITH GRAVE", '\u{c0}'),
    ("REPLACEMENT CHARACTER", '\u{fffd}'),
    ("BYTE ORDER MARK", '\u{feff}'),
    ("ZERO WIDTH NO-BREAK SPACE", '\u{feff}'),
    ("SNAKE", '\u{1f40d}'),
    ("GRINNING FACE", '\u{1f600}'),
    ("PILE OF POO", '\u{1f4a9}'),
    ("DIGIT ZERO", '0'),
    ("DIGIT ONE", '1'),
    ("LATIN SMALL LETTER A", 'a'),
    ("LATIN CAPITAL LETTER A", 'A'),
    ("LATIN SMALL LETTER Z", 'z'),
    ("LATIN CAPITAL LETTER Z", 'Z'),
    ("LEFT CURLY BRACKET", '{'),
    ("RIGHT CURLY BRACKET", '}'),
    ("QUOTATION MARK", '"'),
    ("APOSTROPHE", '\''),
    ("REVERSE SOLIDUS", '\\'),
    ("DOLLAR SIGN", '$'),
    ("COMMERCIAL AT", '@'),
    ("NUMBER SIGN", '#'),
];

const DIGITS: [&str; 10] = [
    "ZERO", "ONE", "TWO", "THREE", "FOUR", "FIVE", "SIX", "SEVEN", "EIGHT", "NINE",
];

const GREEK: [&str; 24] = [
    "ALPHA", "BETA", "GAMMA", "DELTA", "EPSILON", "ZETA", "ETA", "THETA", "IOTA", "KAPPA", "LAMDA",
    "MU", "NU", "XI", "OMICRON", "PI", "RHO", "SIGMA", "TAU", "UPSILON", "PHI", "CHI", "PSI",
    "OMEGA",
];

const ASCII_PUNCT: [(&str, char); 31] = [
    ("EXCLAMATION MARK", '!'),
    ("PERCENT SIGN", '%'),
    ("AMPERSAND", '&'),
    ("LEFT PARENTHESIS", '('),
    ("RIGHT PARENTHESIS", ')'),
    ("ASTERISK", '*'),
    ("PLUS SIGN", '+'),
    ("COMMA", ','),
    ("HYPHEN-MINUS", '-'),
    ("FULL STOP", '.'),
    ("SOLIDUS", '/'),
    ("COLON", ':'),
    ("SEMICOLON", ';'),
    ("LESS-THAN SIGN", '<'),
    ("EQUALS SIGN", '='),
    ("GREATER-THAN SIGN", '>'),
    ("QUESTION MARK", '?'),
    ("LEFT SQUARE BRACKET", '['),
    ("RIGHT SQUARE BRACKET", ']'),
    ("CIRCUMFLEX ACCENT", '^'),
    ("LOW LINE", '_'),
    ("GRAVE ACCENT", '`'),
    ("VERTICAL LINE", '|'),
    ("TILDE", '~'),
    ("SOFT HYPHEN", '\u{ad}'),
    ("NARROW NO-BREAK SPACE", '\u{202f}'),
    ("EMPTY SET", '\u{2205}'),
    ("LATIN CAPITAL LETTER AE", '\u{c6}'),
    ("LATIN SMALL LETTER AE", '\u{e6}'),
    ("LATIN SMALL LETTER A WITH DIAERESIS", '\u{e4}'),
    ("LATIN CAPITAL LETTER A WITH DIAERESIS", '\u{c4}'),
];

fn lookup_name(name: &str) -> Option<char> {
    let up = name.to_ascii_uppercase();
    if let Some((_, c)) = NAMES
        .iter()
        .chain(ASCII_PUNCT.iter())
        .find(|(n, _)| *n == up)
    {
        return Some(*c);
    }
    if let Some(letter) = up.strip_prefix("LATIN SMALL LETTER ") {
        return single_letter(letter).map(|c| c.to_ascii_lowercase());
    }
    if let Some(letter) = up.strip_prefix("LATIN CAPITAL LETTER ") {
        return single_letter(letter);
    }
    if let Some(d) = up.strip_prefix("DIGIT ") {
        let n = DIGITS.iter().position(|x| *x == d)?;
        return char::from_digit(n as u32, 10);
    }
    let (small, rest) = match (
        up.strip_prefix("GREEK SMALL LETTER "),
        up.strip_prefix("GREEK CAPITAL LETTER "),
    ) {
        (Some(r), _) => (true, r),
        (_, Some(r)) => (false, r),
        _ => return None,
    };
    let idx = GREEK.iter().position(|x| *x == rest)? as u32;
    let idx = if idx >= 17 { idx + 1 } else { idx };
    char::from_u32(if small { 0x3b1 + idx } else { 0x391 + idx })
}

fn single_letter(s: &str) -> Option<char> {
    let mut it = s.chars();
    match (it.next(), it.next()) {
        (Some(c), None) if c.is_ascii_uppercase() => Some(c),
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
                out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
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
