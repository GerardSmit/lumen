//! Tokenizers over decoded text. Each returns the token and the offset just past it, `Need` when the
//! text ends inside the token, or `Bad(pos)` for an invalid token at byte offset `pos`.

use super::chars::*;

pub enum Scan<T> {
    Tok(T, usize),
    Need,
    Bad(usize),
}

use Scan::*;

fn at(s: &str, i: usize) -> Option<char> {
    s.get(i..).and_then(|r| r.chars().next())
}

/// Offset past a name starting at `i` (which must be a name start), or `None` if there is none.
pub fn name_end(s: &str, i: usize) -> Option<usize> {
    let mut j = i;
    let mut first = true;
    for c in s[i..].chars() {
        if first {
            if !is_name_start(c) {
                return None;
            }
            first = false;
        } else if !is_name_char(c) {
            break;
        }
        j += c.len_utf8();
    }
    if first {
        None
    } else {
        Some(j)
    }
}

pub fn skip_space(s: &str, mut i: usize) -> usize {
    while let Some(c) = at(s, i) {
        if !is_space(c) {
            break;
        }
        i += 1;
    }
    i
}

pub struct StartTag<'a> {
    pub name: &'a str,
    pub attrs: Vec<(&'a str, &'a str)>,
    pub empty: bool,
}

/// `<name attr="v" ...>` or `<name .../>`; `s[i]` is `<` and a name start follows.
pub fn start_tag(s: &str, i: usize) -> Scan<StartTag<'_>> {
    let ne = match name_end(s, i + 1) {
        Some(e) => e,
        None => return Bad(i + 1),
    };
    let name = &s[i + 1..ne];
    let mut attrs = Vec::new();
    let mut j = ne;
    let mut need_space = true;
    loop {
        let sp = skip_space(s, j);
        let spaced = sp > j;
        j = sp;
        let c = match at(s, j) {
            Some(c) => c,
            None => return Need,
        };
        match c {
            '>' => return Tok(StartTag { name, attrs, empty: false }, j + 1),
            '/' => {
                return match at(s, j + 1) {
                    None => Need,
                    Some('>') => Tok(StartTag { name, attrs, empty: true }, j + 2),
                    Some(_) => Bad(j + 1),
                };
            }
            _ => {}
        }
        if need_space && !spaced {
            return Bad(j);
        }
        let ae = match name_end(s, j) {
            Some(e) => e,
            None => return Bad(j),
        };
        let aname = &s[j..ae];
        j = skip_space(s, ae);
        match at(s, j) {
            None => return Need,
            Some('=') => j += 1,
            Some(_) => return Bad(j),
        }
        j = skip_space(s, j);
        let q = match at(s, j) {
            None => return Need,
            Some(q @ ('"' | '\'')) => q,
            Some(_) => return Bad(j),
        };
        j += 1;
        let vstart = j;
        loop {
            match at(s, j) {
                None => return Need,
                Some(c) if c == q => break,
                Some('<') => return Bad(j),
                Some(c) if !is_xml_char(c) => return Bad(j),
                Some('&') => {
                    // a reference inside the value must be well formed
                    match reference(s, j) {
                        Tok(_, e) => {
                            j = e;
                            continue;
                        }
                        Need => return Need,
                        Bad(p) => return Bad(p),
                    }
                }
                Some(c) => j += c.len_utf8(),
            }
        }
        attrs.push((aname, &s[vstart..j]));
        j += 1;
        need_space = true;
    }
}

/// `</name>`; `s[i..]` starts with `</`.
pub fn end_tag(s: &str, i: usize) -> Scan<&str> {
    let ne = match at(s, i + 2) {
        None => return Need,
        Some(_) => match name_end(s, i + 2) {
            Some(e) => e,
            None => return Bad(i + 2),
        },
    };
    let j = skip_space(s, ne);
    match at(s, j) {
        None => Need,
        Some('>') => Tok(&s[i + 2..ne], j + 1),
        Some(_) => Bad(j),
    }
}

/// `<!-- ... -->`; yields the text between the delimiters.
pub fn comment(s: &str, i: usize) -> Scan<&str> {
    let start = i + 4;
    let mut j = start;
    loop {
        match at(s, j) {
            None => return Need,
            Some('-') => match at(s, j + 1) {
                None => return Need,
                Some('-') => match at(s, j + 2) {
                    None => return Need,
                    Some('>') => return Tok(&s[start..j], j + 3),
                    Some(_) => return Bad(j + 2),
                },
                Some(_) => j += 1,
            },
            Some(c) if !is_xml_char(c) => return Bad(j),
            Some(c) => j += c.len_utf8(),
        }
    }
}

pub struct Pi<'a> {
    pub target: &'a str,
    pub data: &'a str,
}

/// `<?target data?>`; `s[i..]` starts with `<?`.
pub fn pi(s: &str, i: usize) -> Scan<Pi<'_>> {
    let ne = match at(s, i + 2) {
        None => return Need,
        Some(_) => match name_end(s, i + 2) {
            Some(e) => e,
            None => return Bad(i + 2),
        },
    };
    let target = &s[i + 2..ne];
    match at(s, ne) {
        None => Need,
        Some('?') => match at(s, ne + 1) {
            None => Need,
            Some('>') => Tok(Pi { target, data: "" }, ne + 2),
            Some(_) => Bad(ne + 1),
        },
        Some(c) if is_space(c) => {
            let ds = skip_space(s, ne);
            let mut j = ds;
            loop {
                match at(s, j) {
                    None => return Need,
                    Some('?') => match at(s, j + 1) {
                        None => return Need,
                        Some('>') => return Tok(Pi { target, data: &s[ds..j] }, j + 2),
                        Some(_) => j += 1,
                    },
                    Some(c) if !is_xml_char(c) => return Bad(j),
                    Some(c) => j += c.len_utf8(),
                }
            }
        }
        Some(_) => Bad(ne),
    }
}

pub enum Ref<'a> {
    /// `&#N;`; `None` when the number is not an XML character.
    Char(Option<char>),
    Entity(&'a str),
}

/// `&#N;`, `&#xH;` or `&name;`.
pub fn reference(s: &str, i: usize) -> Scan<Ref<'_>> {
    match at(s, i + 1) {
        None => Need,
        Some('#') => {
            let (hex, mut j) = match at(s, i + 2) {
                None => return Need,
                Some('x') => (true, i + 3),
                Some(_) => (false, i + 2),
            };
            let digits = j;
            let mut v: u32 = 0;
            loop {
                match at(s, j) {
                    None => return Need,
                    Some(';') if j > digits => break,
                    Some(c) => match c.to_digit(if hex { 16 } else { 10 }) {
                        Some(d) => {
                            v = v.saturating_mul(if hex { 16 } else { 10 }).saturating_add(d);
                            j += 1;
                        }
                        None => return Bad(j),
                    },
                }
            }
            Tok(Ref::Char(char::from_u32(v).filter(|c| is_xml_char(*c))), j + 1)
        }
        Some(_) => match name_end(s, i + 1) {
            None => Bad(i + 1),
            Some(e) => match at(s, e) {
                None => Need,
                Some(';') => Tok(Ref::Entity(&s[i + 1..e]), e + 1),
                Some(_) => Bad(e),
            },
        },
    }
}

/// `%name;`
pub fn pe_reference(s: &str, i: usize) -> Scan<&str> {
    match at(s, i + 1) {
        None => Need,
        Some(_) => match name_end(s, i + 1) {
            None => Bad(i + 1),
            Some(e) => match at(s, e) {
                None => Need,
                Some(';') => Tok(&s[i + 1..e], e + 1),
                Some(_) => Bad(e),
            },
        },
    }
}

/// A run of character data starting at `i`. Returns the end offset (equal to `i` when the run must
/// wait for more input), or the offset of an invalid character.
pub fn text_run(s: &str, i: usize, more: bool) -> Result<usize, usize> {
    let mut j = i;
    while let Some(c) = at(s, j) {
        match c {
            '<' | '&' | '\r' | '\n' => break,
            ']' => {
                let rest = &s[j + 1..];
                if rest.starts_with("]>") {
                    return Err(j + 2);
                }
                if (rest.is_empty() || rest == "]") && more {
                    break;
                }
                j += 1;
            }
            c if !is_xml_char(c) => return Err(j),
            c => j += c.len_utf8(),
        }
    }
    Ok(j)
}

/// A quoted literal at `i`; returns the offset past the closing quote.
pub fn literal(s: &str, i: usize) -> Scan<&str> {
    let q = match at(s, i) {
        None => return Need,
        Some(q @ ('"' | '\'')) => q,
        Some(_) => return Bad(i),
    };
    let mut j = i + 1;
    loop {
        match at(s, j) {
            None => return Need,
            Some(c) if c == q => return Tok(&s[i + 1..j], j + 1),
            Some(c) if !is_xml_char(c) => return Bad(j),
            Some(c) => j += c.len_utf8(),
        }
    }
}

pub struct DoctypeHead<'a> {
    pub name: &'a str,
    pub pubid: Option<&'a str>,
    pub sysid: Option<&'a str>,
    pub has_subset: bool,
    /// Every token of the header, for the default handler.
    pub tokens: Vec<(usize, usize)>,
}

/// `<!DOCTYPE name [PUBLIC "p" "s" | SYSTEM "s"] ( [ | > )`; `s[i..]` starts with `<!DOCTYPE`.
pub fn doctype_head(s: &str, i: usize) -> Scan<DoctypeHead<'_>> {
    let mut tokens = vec![(i, i + 9)];
    let mut j = i + 9;
    macro_rules! space {
        ($required:expr) => {{
            let e = skip_space(s, j);
            if e == j {
                match at(s, j) {
                    None => return Need,
                    Some(_) if $required => return Bad(j),
                    Some(_) => {}
                }
            } else {
                tokens.push((j, e));
            }
            j = e;
        }};
    }
    space!(true);
    let ne = match at(s, j) {
        None => return Need,
        Some(_) => match name_end(s, j) {
            Some(e) => e,
            None => return Bad(j),
        },
    };
    let name = &s[j..ne];
    tokens.push((j, ne));
    j = ne;
    space!(false);
    let mut pubid = None;
    let mut sysid = None;
    let rest = &s[j..];
    let kw = |k: &str| rest.starts_with(k);
    if kw("PUBLIC") || kw("SYSTEM") {
        let public = kw("PUBLIC");
        tokens.push((j, j + 6));
        j += 6;
        space!(true);
        if public {
            match literal(s, j) {
                Tok(p, e) => {
                    tokens.push((j, e));
                    pubid = Some(p);
                    j = e;
                }
                Need => return Need,
                Bad(p) => return Bad(p),
            }
            space!(true);
        }
        match literal(s, j) {
            Tok(p, e) => {
                tokens.push((j, e));
                sysid = Some(p);
                j = e;
            }
            Need => return Need,
            Bad(p) => return Bad(p),
        }
        space!(false);
    } else if rest.len() < 6 && ("PUBLIC".starts_with(rest) || "SYSTEM".starts_with(rest)) && !rest.is_empty() {
        return Need;
    }
    match at(s, j) {
        None => Need,
        Some('[') => {
            tokens.push((j, j + 1));
            Tok(DoctypeHead { name, pubid, sysid, has_subset: true, tokens }, j + 1)
        }
        Some('>') => {
            tokens.push((j, j + 1));
            Tok(DoctypeHead { name, pubid, sysid, has_subset: false, tokens }, j + 1)
        }
        Some(_) => Bad(j),
    }
}

/// One `<!KEYWORD ... >` declaration: ends at the first `>` outside a quoted literal.
pub fn declaration(s: &str, i: usize) -> Scan<()> {
    let mut j = i + 2;
    let mut quote: Option<char> = None;
    loop {
        match at(s, j) {
            None => return Need,
            Some(c) => {
                if !is_xml_char(c) {
                    return Bad(j);
                }
                match quote {
                    Some(q) if c == q => quote = None,
                    Some(_) => {}
                    None => match c {
                        '"' | '\'' => quote = Some(c),
                        '>' => return Tok((), j + 1),
                        _ => {}
                    },
                }
                j += c.len_utf8();
            }
        }
    }
}

/// Whether `rest` is a proper prefix of `full` (so more input could complete it).
pub fn partial_prefix(full: &str, rest: &str) -> bool {
    rest.len() < full.len() && full.starts_with(rest)
}
