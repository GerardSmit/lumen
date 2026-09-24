//! UTS #46 domain processing: a port of ada's `ada::idna` (the IDNA implementation Node.js uses
//! for `new URL()` hosts and `url.domainToASCII`), with its tables, so hosts come out byte-identical
//! to Node's, including ada's choices of CheckBidi/CheckJoiners handling.

#[rustfmt::skip]
#[allow(clippy::unreadable_literal)]
mod tables;

use tables::*;

// ---- mapping (UTS46 section 5) ----

fn find_range_index(key: u32) -> usize {
    // Last row whose start is <= key.
    match TABLE.binary_search_by(|row| row[0].cmp(&key)) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    }
}

/// Map per the IDNA mapping table; `None` when a disallowed code point is present.
fn map(input: &[u32]) -> Option<Vec<u32>> {
    let mut out = Vec::with_capacity(input.len());
    for &x in input {
        let descriptor = TABLE[find_range_index(x)][1];
        match descriptor as u8 {
            0 => {} // ignored
            1 => out.push(x),
            2 => return None, // disallowed
            _ => {
                let count = (descriptor >> 24) as usize;
                let index = ((descriptor >> 8) & 0xFFFF) as usize;
                out.extend_from_slice(&MAPPINGS[index..index + count]);
            }
        }
    }
    Some(out)
}

// ---- NFC normalization ----

const HANGUL_SBASE: u32 = 0xAC00;
const HANGUL_TBASE: u32 = 0x11A7;
const HANGUL_VBASE: u32 = 0x1161;
const HANGUL_LBASE: u32 = 0x1100;
const HANGUL_LCOUNT: u32 = 19;
const HANGUL_VCOUNT: u32 = 21;
const HANGUL_TCOUNT: u32 = 28;
const HANGUL_NCOUNT: u32 = HANGUL_VCOUNT * HANGUL_TCOUNT;
const HANGUL_SCOUNT: u32 = HANGUL_LCOUNT * HANGUL_VCOUNT * HANGUL_TCOUNT;

fn decomposition_of(c: u32) -> &'static [u32] {
    if c >= 0x110000 {
        return &[];
    }
    let block = &DECOMPOSITION_BLOCK[DECOMPOSITION_INDEX[(c >> 8) as usize] as usize];
    let lo = block[(c % 256) as usize];
    let hi = block[(c % 256) as usize + 1];
    let len = ((hi >> 2) - (lo >> 2)) as usize;
    if len == 0 || lo & 1 != 0 {
        return &[];
    }
    let start = (lo >> 2) as usize;
    &DECOMPOSITION_DATA[start..start + len]
}

fn ccc(c: u32) -> u8 {
    if c < 0x110000 {
        CCC_BLOCK[CCC_INDEX[(c >> 8) as usize] as usize][(c % 256) as usize]
    } else {
        0
    }
}

fn decompose(input: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(input.len());
    for &c in input {
        if (HANGUL_SBASE..HANGUL_SBASE + HANGUL_SCOUNT).contains(&c) {
            let s = c - HANGUL_SBASE;
            out.push(HANGUL_LBASE + s / HANGUL_NCOUNT);
            out.push(HANGUL_VBASE + (s % HANGUL_NCOUNT) / HANGUL_TCOUNT);
            if s % HANGUL_TCOUNT != 0 {
                out.push(HANGUL_TBASE + s % HANGUL_TCOUNT);
            }
        } else {
            let d = decomposition_of(c);
            if d.is_empty() {
                out.push(c);
            } else {
                out.extend_from_slice(d);
            }
        }
    }
    // Canonical ordering (stable insertion sort by combining class).
    for idx in 1..out.len() {
        let k = ccc(out[idx]);
        if k == 0 {
            continue;
        }
        let cur = out[idx];
        let mut back = idx;
        while back != 0 && ccc(out[back - 1]) > k {
            out[back] = out[back - 1];
            back -= 1;
        }
        out[back] = cur;
    }
    out
}

fn composition_row(c: u32) -> (u16, u16) {
    let block = &COMPOSITION_BLOCK[COMPOSITION_INDEX[(c >> 8) as usize] as usize];
    (block[(c % 256) as usize], block[(c % 256) as usize + 1])
}

fn compose(input: &mut Vec<u32>) {
    let n = input.len();
    let mut i = 0; // input_count
    let mut w = 0; // composition_count
    while i < n {
        input[w] = input[i];
        let c = input[i];
        if (HANGUL_LBASE..HANGUL_LBASE + HANGUL_LCOUNT).contains(&c) {
            if i + 1 < n && (HANGUL_VBASE..HANGUL_VBASE + HANGUL_VCOUNT).contains(&input[i + 1]) {
                input[w] = HANGUL_SBASE
                    + ((c - HANGUL_LBASE) * HANGUL_VCOUNT + input[i + 1] - HANGUL_VBASE)
                        * HANGUL_TCOUNT;
                i += 1;
                if i + 1 < n && input[i + 1] > HANGUL_TBASE && input[i + 1] < HANGUL_TBASE + HANGUL_TCOUNT
                {
                    i += 1;
                    input[w] += input[i] - HANGUL_TBASE;
                }
            }
        } else if (HANGUL_SBASE..HANGUL_SBASE + HANGUL_SCOUNT).contains(&c) {
            if (c - HANGUL_SBASE) % HANGUL_TCOUNT != 0
                && i + 1 < n
                && input[i + 1] > HANGUL_TBASE
                && input[i + 1] < HANGUL_TBASE + HANGUL_TCOUNT
            {
                i += 1;
                input[w] += input[i] - HANGUL_TBASE;
            }
        } else if c < 0x110000 {
            let mut row = composition_row(c);
            let start = w;
            let mut previous_ccc: i32 = -1;
            while i + 1 < n {
                let next = input[i + 1];
                let k = ccc(next);
                if row.1 != row.0 && previous_ccc < k as i32 {
                    // Binary search the (code point, composite) pairs for `next`.
                    let (mut left, mut right) = (row.0 as usize, row.1 as usize);
                    while left + 2 < right {
                        let middle = left + (((right - left) >> 1) & !1);
                        if COMPOSITION_DATA[middle] <= next {
                            left = middle;
                        }
                        if COMPOSITION_DATA[middle] >= next {
                            right = middle;
                        }
                    }
                    if COMPOSITION_DATA[left] == next {
                        let composite = COMPOSITION_DATA[left + 1];
                        input[start] = composite;
                        row = composition_row(composite);
                        i += 1;
                        continue;
                    }
                }
                if k == 0 {
                    break;
                }
                previous_ccc = k as i32;
                w += 1;
                input[w] = next;
                i += 1;
            }
        }
        i += 1;
        w += 1;
    }
    input.truncate(w);
}

fn normalize(input: &[u32]) -> Vec<u32> {
    let mut d = decompose(input);
    compose(&mut d);
    d
}

// ---- punycode (RFC 3492) ----

const BASE: i32 = 36;
const TMIN: i32 = 1;
const TMAX: i32 = 26;
const SKEW: i32 = 38;
const DAMP: i32 = 700;
const INITIAL_BIAS: i32 = 72;
const INITIAL_N: u32 = 128;

fn digit_value(c: u8) -> i32 {
    match c {
        b'a'..=b'z' => (c - b'a') as i32,
        b'0'..=b'9' => (c - b'0') as i32 + 26,
        _ => -1,
    }
}

fn digit_char(d: i32) -> u8 {
    if d < 26 {
        (d + 97) as u8
    } else {
        (d + 22) as u8
    }
}

fn adapt(mut d: i32, n: i32, first: bool) -> i32 {
    d = if first { d / DAMP } else { d / 2 };
    d += d / n;
    let mut k = 0;
    while d > ((BASE - TMIN) * TMAX) / 2 {
        d /= BASE - TMIN;
        k += BASE;
    }
    k + (((BASE - TMIN + 1) * d) / (d + SKEW))
}

/// Decode a punycode label body (without "xn--"); `None` on malformed input.
pub(crate) fn punycode_decode(mut input: &[u8]) -> Option<Vec<u32>> {
    let mut out: Vec<u32> = Vec::with_capacity(input.len());
    let mut n = INITIAL_N;
    let mut i: i32 = 0;
    let mut bias = INITIAL_BIAS;
    if let Some(end) = input.iter().rposition(|&c| c == b'-') {
        for &c in &input[..end] {
            if c >= 0x80 {
                return None;
            }
            out.push(c as u32);
        }
        input = &input[end + 1..];
    }
    while !input.is_empty() {
        let oldi = i;
        let mut w: i32 = 1;
        let mut k = BASE;
        loop {
            let (&c, rest) = input.split_first()?;
            input = rest;
            let digit = digit_value(c);
            if digit < 0 || digit > (i32::MAX - i) / w {
                return None;
            }
            i += digit * w;
            let t = if k <= bias {
                TMIN
            } else if k >= bias + TMAX {
                TMAX
            } else {
                k - bias
            };
            if digit < t {
                break;
            }
            if w > i32::MAX / (BASE - t) {
                return None;
            }
            w *= BASE - t;
            k += BASE;
        }
        let len = out.len() as i32 + 1;
        bias = adapt(i - oldi, len, oldi == 0);
        if i / len > (0x7fff_ffff - n) as i32 {
            return None;
        }
        n += (i / len) as u32;
        i %= len;
        if n < 0x80 {
            return None;
        }
        out.insert(i as usize, n);
        i += 1;
    }
    Some(out)
}

fn punycode_encode(input: &[u32], out: &mut String) -> bool {
    let mut n = INITIAL_N;
    let mut d: i32 = 0;
    let mut bias = INITIAL_BIAS;
    let mut h = 0usize;
    for &c in input {
        if c < 0x80 {
            h += 1;
            out.push(c as u8 as char);
        }
        if c > 0x10ffff || (0xd880..0xe000).contains(&c) {
            return false;
        }
    }
    let b = h;
    if b > 0 {
        out.push('-');
    }
    while h < input.len() {
        let mut m = 0x10FFFF;
        for &c in input {
            if c >= n && c < m {
                m = c;
            }
        }
        if (m - n) as i64 > (i32::MAX as i64 - d as i64) / (h as i64 + 1) {
            return false;
        }
        d += ((m - n) as usize * (h + 1)) as i32;
        n = m;
        for &c in input {
            if c < n {
                if d == i32::MAX {
                    return false;
                }
                d += 1;
            }
            if c == n {
                let mut q = d;
                let mut k = BASE;
                loop {
                    let t = if k <= bias {
                        TMIN
                    } else if k >= bias + TMAX {
                        TMAX
                    } else {
                        k - bias
                    };
                    if q < t {
                        break;
                    }
                    out.push(digit_char(t + ((q - t) % (BASE - t))) as char);
                    q = (q - t) / (BASE - t);
                    k += BASE;
                }
                out.push(digit_char(q) as char);
                bias = adapt(d, h as i32 + 1, h == b);
                d = 0;
                h += 1;
            }
        }
        d += 1;
        n += 1;
    }
    true
}

// ---- label validity (UTS46 4.1, with ada's CheckJoiners/CheckBidi) ----

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Dir {
    None = 0,
    Bn = 1,
    Cs = 2,
    Es = 3,
    On = 4,
    En = 5,
    L = 6,
    R = 7,
    Nsm = 8,
    Al = 9,
    An = 10,
    Et = 11,
}

fn direction(c: u32) -> u8 {
    let i = DIR_TABLE.partition_point(|&(_, last, _)| last < c);
    match DIR_TABLE.get(i) {
        Some(&(first, _, d)) if c >= first => d,
        _ => Dir::None as u8,
    }
}

fn is(d: u8, dir: Dir) -> bool {
    d == dir as u8
}

fn is_label_valid(label: &[u32]) -> bool {
    let Some(&first) = label.first() else {
        return true;
    };
    if COMBINING.binary_search(&first).is_ok() {
        return false;
    }
    for (i, &c) in label.iter().enumerate() {
        if c == 0x200c {
            if i > 0 && VIRAMA.binary_search(&label[i - 1]).is_ok() {
                return true;
            }
            if i == 0 || i + 1 >= label.len() {
                return false;
            }
            let l_or_d = |x: &u32| JOIN_L.binary_search(x).is_ok() || JOIN_D.binary_search(x).is_ok();
            let r_or_d = |x: &u32| JOIN_R.binary_search(x).is_ok() || JOIN_D.binary_search(x).is_ok();
            return label[..i].iter().any(l_or_d) && label[i + 1..].iter().any(r_or_d);
        } else if c == 0x200d {
            return i > 0 && VIRAMA.binary_search(&label[i - 1]).is_ok();
        }
    }
    let Some(last_non_nsm) = label.iter().rposition(|&c| !is(direction(c), Dir::Nsm)) else {
        return false;
    };
    let rtl = label.iter().any(|&c| {
        let d = direction(c);
        is(d, Dir::R) || is(d, Dir::Al) || is(d, Dir::An)
    });
    if !rtl {
        return true;
    }
    if is(direction(label[0]), Dir::L) {
        // ada's loop stops before the last non-NSM character, so its end-of-label rule never
        // fires for LTR labels; kept as-is for parity with Node.
        for &c in label.iter().take(last_non_nsm) {
            let d = direction(c);
            let ok = [Dir::L, Dir::En, Dir::Es, Dir::Cs, Dir::Et, Dir::On, Dir::Bn, Dir::Nsm]
                .iter()
                .any(|&x| is(d, x));
            if !ok {
                return false;
            }
        }
        true
    } else {
        let (mut has_an, mut has_en) = (false, false);
        for (i, &c) in label.iter().enumerate().take(last_non_nsm + 1) {
            let d = direction(c);
            if is(d, Dir::En) {
                has_en = true;
                if has_an {
                    return false;
                }
            }
            if is(d, Dir::An) {
                has_an = true;
                if has_en {
                    return false;
                }
            }
            let ok = [
                Dir::R,
                Dir::Al,
                Dir::An,
                Dir::En,
                Dir::Es,
                Dir::Cs,
                Dir::Et,
                Dir::On,
                Dir::Bn,
                Dir::Nsm,
            ]
            .iter()
            .any(|&x| is(d, x));
            if !ok {
                return false;
            }
            if i == last_non_nsm && ![Dir::R, Dir::Al, Dir::An, Dir::En].iter().any(|&x| is(d, x)) {
                return false;
            }
        }
        true
    }
}

// ---- ToASCII / ToUnicode ----

fn verify_xn_label(ascii_body: &[u8]) -> bool {
    let Some(decoded) = punycode_decode(ascii_body) else {
        return false;
    };
    let Some(post_map) = map(&decoded) else {
        return false;
    };
    if post_map != decoded {
        return false;
    }
    let normal = normalize(&post_map);
    if normal != post_map || normal.is_empty() {
        return false;
    }
    is_label_valid(&normal)
}

/// UTS46 ToASCII as the URL standard configures it; `None` on failure (ada's "" result).
pub(crate) fn to_ascii(input: &[u8]) -> Option<String> {
    let mut out = String::with_capacity(input.len());
    if input.is_ascii() {
        let lower = input.to_ascii_lowercase();
        let labels: Vec<&[u8]> = lower.split(|&c| c == b'.').collect();
        for (n, label) in labels.iter().enumerate() {
            if label.starts_with(b"xn--") {
                out.push_str(std::str::from_utf8(label).ok()?);
                if !verify_xn_label(&label[4..]) {
                    return None;
                }
            } else {
                out.push_str(std::str::from_utf8(label).ok()?);
            }
            if n + 1 < labels.len() {
                out.push('.');
            }
        }
        return non_empty(out);
    }
    let text = std::str::from_utf8(input).ok()?;
    let cps: Vec<u32> = text.chars().map(|c| c as u32).collect();
    if cps.is_empty() {
        return None;
    }
    let mapped = normalize(&map(&cps)?);
    let labels: Vec<&[u32]> = mapped.split(|&c| c == '.' as u32).collect();
    for (n, label) in labels.iter().enumerate() {
        if label.is_empty() {
            // nothing
        } else if label.starts_with(&[b'x' as u32, b'n' as u32, b'-' as u32, b'-' as u32]) {
            let mut body = Vec::with_capacity(label.len());
            for &c in label.iter() {
                if c >= 0x80 {
                    return None;
                }
                out.push(c as u8 as char);
                body.push(c as u8);
            }
            if !verify_xn_label(&body[4..]) {
                return None;
            }
        } else if label.iter().all(|&c| c < 0x80) {
            out.extend(label.iter().map(|&c| c as u8 as char));
        } else {
            if !is_label_valid(label) {
                return None;
            }
            out.push_str("xn--");
            if !punycode_encode(label, &mut out) {
                return None;
            }
        }
        if n + 1 < labels.len() {
            out.push('.');
        }
    }
    non_empty(out)
}

fn non_empty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

/// ada's ToUnicode: decode each well-formed `xn--` label, leave everything else untouched.
pub(crate) fn to_unicode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let labels: Vec<&str> = input.split('.').collect();
    for (n, label) in labels.iter().enumerate() {
        let decoded = label
            .strip_prefix("xn--")
            .filter(|body| body.is_ascii())
            .and_then(|body| punycode_decode(body.as_bytes()))
            .and_then(|cps| cps.into_iter().map(char::from_u32).collect::<Option<String>>());
        match decoded {
            Some(s) => out.push_str(&s),
            None => out.push_str(label),
        }
        if n + 1 < labels.len() {
            out.push('.');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_and_unicode() {
        assert_eq!(to_ascii(b"EXAMPLE.com").as_deref(), Some("example.com"));
        assert_eq!(to_ascii("bücher.de".as_bytes()).as_deref(), Some("xn--bcher-kva.de"));
        assert_eq!(to_ascii("ＥＸＡＭＰＬＥ。com".as_bytes()).as_deref(), Some("example.com"));
        assert_eq!(to_ascii(b"xn--bcher-kva.de").as_deref(), Some("xn--bcher-kva.de"));
        assert_eq!(to_ascii(b"xn--a.de"), None);
        assert_eq!(to_unicode("xn--bcher-kva.de"), "bücher.de");
        // NFC composition: u + combining diaeresis.
        assert_eq!(to_ascii("bu\u{308}cher".as_bytes()).as_deref(), Some("xn--bcher-kva"));
        // Hangul syllables.
        assert_eq!(to_ascii("한국".as_bytes()).as_deref(), Some("xn--3e0b707e"));
    }
}
