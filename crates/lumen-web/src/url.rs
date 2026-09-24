//! WHATWG URL parsing and serialization: a port of ada 2.7's `ada::url` (the parser Node.js 20
//! vendors), so hrefs, component splits and setter behavior match Node byte for byte — including
//! ada's few deviations from the letter of the spec (e.g. an all-decimal IPv4 host is kept as
//! written). Hosts go through the UTS #46 port in `idna`.
//!
//! Everything is byte-oriented over the UTF-8 input; every stored component is ASCII (non-ASCII
//! is percent-encoded or punycoded), so byte offsets are also UTF-16 offsets for the JS side.

mod idna;

pub(crate) use idna::to_unicode as domain_to_unicode_raw;

/// ada's `ada::scheme::type` numbering (the JS side reads it as `scheme_type`).
pub(crate) mod kind {
    pub const HTTP: u8 = 0;
    pub const NOT_SPECIAL: u8 = 1;
    pub const HTTPS: u8 = 2;
    pub const WS: u8 = 3;
    pub const FTP: u8 = 4;
    pub const WSS: u8 = 5;
    pub const FILE: u8 = 6;
}

/// `u32::MAX` marks an absent port/search/hash in the component offsets (ada's "omitted").
pub(crate) const OMITTED: u32 = u32::MAX;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Url {
    /// Lowercased scheme without the trailing ':'.
    pub scheme: String,
    pub kind: u8,
    pub username: String,
    pub password: String,
    /// Serialized host; `None` for host-less URLs (`mailto:x`), `Some("")` for `file:///`.
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Serialized path ("/a/b" for list paths, the raw text for opaque paths).
    pub path: String,
    pub opaque: bool,
    /// Query without the leading '?'.
    pub query: Option<String>,
    /// Fragment without the leading '#'.
    pub fragment: Option<String>,
}

impl Default for Url {
    fn default() -> Self {
        Url {
            scheme: String::new(),
            kind: kind::NOT_SPECIAL,
            username: String::new(),
            password: String::new(),
            host: None,
            port: None,
            path: String::new(),
            opaque: false,
            query: None,
            fragment: None,
        }
    }
}

fn scheme_kind(s: &str) -> u8 {
    match s {
        "http" => kind::HTTP,
        "https" => kind::HTTPS,
        "ws" => kind::WS,
        "ftp" => kind::FTP,
        "wss" => kind::WSS,
        "file" => kind::FILE,
        _ => kind::NOT_SPECIAL,
    }
}

fn default_port_of(k: u8) -> u16 {
    match k {
        kind::HTTP | kind::WS => 80,
        kind::HTTPS | kind::WSS => 443,
        kind::FTP => 21,
        _ => 0,
    }
}

// ---- percent-encode sets (ada's character_sets) ----

type EncodeSet = fn(u8) -> bool;

fn c0_control_set(c: u8) -> bool {
    c < 0x20 || c > 0x7e
}
fn fragment_set(c: u8) -> bool {
    c0_control_set(c) || matches!(c, b' ' | b'"' | b'<' | b'>' | b'`')
}
fn query_set(c: u8) -> bool {
    c0_control_set(c) || matches!(c, b' ' | b'"' | b'#' | b'<' | b'>')
}
fn special_query_set(c: u8) -> bool {
    query_set(c) || c == b'\''
}
fn path_set(c: u8) -> bool {
    query_set(c) || matches!(c, b'?' | b'`' | b'{' | b'}')
}
fn userinfo_set(c: u8) -> bool {
    path_set(c)
        || matches!(
            c,
            b'/' | b':' | b';' | b'=' | b'@' | b'[' | b'\\' | b']' | b'^' | b'|'
        )
}

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

fn percent_encode(input: &[u8], set: EncodeSet) -> String {
    let mut out = String::with_capacity(input.len());
    for &b in input {
        if set(b) {
            out.push('%');
            out.push(HEX_UPPER[(b >> 4) as usize] as char);
            out.push(HEX_UPPER[(b & 15) as usize] as char);
        } else {
            out.push(b as char);
        }
    }
    out
}

fn percent_decode(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let c = input[i];
        if c == b'%'
            && i + 2 < input.len()
            && input[i + 1].is_ascii_hexdigit()
            && input[i + 2].is_ascii_hexdigit()
        {
            let hex = |x: u8| (x as char).to_digit(16).unwrap_or(0) as u8;
            out.push(hex(input[i + 1]) * 16 + hex(input[i + 2]));
            i += 3;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn is_forbidden_host(c: u8) -> bool {
    matches!(
        c,
        0 | b'\t'
            | b'\n'
            | b'\r'
            | b' '
            | b'#'
            | b'/'
            | b':'
            | b'<'
            | b'>'
            | b'?'
            | b'@'
            | b'['
            | b'\\'
            | b']'
            | b'^'
            | b'|'
    )
}

fn is_forbidden_domain(c: u8) -> bool {
    is_forbidden_host(c) || c == b'%' || c <= 32 || (127..255).contains(&c)
}

fn remove_tab_newline(input: &[u8]) -> Vec<u8> {
    input
        .iter()
        .copied()
        .filter(|&c| !matches!(c, b'\t' | b'\n' | b'\r'))
        .collect()
}

fn is_alnum_plus(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'+' | b'-' | b'.')
}

fn is_windows_drive_letter(input: &[u8]) -> bool {
    input.len() >= 2
        && input[0].is_ascii_alphabetic()
        && matches!(input[1], b':' | b'|')
        && (input.len() == 2 || matches!(input[2], b'/' | b'\\' | b'?' | b'#'))
}

fn is_normalized_windows_drive_letter(input: &[u8]) -> bool {
    input.len() >= 2 && input[0].is_ascii_alphabetic() && input[1] == b':'
}

fn is_single_dot(s: &[u8]) -> bool {
    s == b"." || s.eq_ignore_ascii_case(b"%2e")
}

fn is_double_dot(s: &[u8]) -> bool {
    s == b".."
        || s.eq_ignore_ascii_case(b".%2e")
        || s.eq_ignore_ascii_case(b"%2e.")
        || s.eq_ignore_ascii_case(b"%2e%2e")
}

fn find(hay: &[u8], pred: impl Fn(u8) -> bool) -> Option<usize> {
    hay.iter().position(|&c| pred(c))
}

/// Remove the last path segment (ada's `helpers::shorten_path`); false when nothing was removed.
fn shorten_path(path: &mut String, k: u8) -> bool {
    let first_delimiter = path.get(1..).and_then(|rest| rest.find('/'));
    if k == kind::FILE
        && first_delimiter.is_none()
        && !path.is_empty()
        && is_normalized_windows_drive_letter(&path.as_bytes()[1..])
    {
        return false;
    }
    match path.rfind('/') {
        Some(last) => {
            path.truncate(last);
            true
        }
        None => false,
    }
}

/// Append the segments of `input` (the path after its leading slash) to `path`, resolving
/// dot segments (ada's `helpers::parse_prepared_path`).
fn parse_prepared_path(mut input: &[u8], k: u8, path: &mut String) {
    let special = k != kind::NOT_SPECIAL;
    let split_backslash = special && input.contains(&b'\\');
    loop {
        let location = if split_backslash {
            find(input, |c| c == b'/' || c == b'\\')
        } else {
            find(input, |c| c == b'/')
        };
        let segment = match location {
            Some(l) => {
                let s = &input[..l];
                input = &input[l + 1..];
                s
            }
            None => input,
        };
        let buffer = percent_encode(segment, path_set);
        let b = buffer.as_bytes();
        if is_double_dot(b) {
            if (shorten_path(path, k) || special) && location.is_none() {
                path.push('/');
            }
        } else if is_single_dot(b) && location.is_none() {
            path.push('/');
        } else if !is_single_dot(b) {
            if k == kind::FILE && path.is_empty() && is_windows_drive_letter(b) {
                path.push('/');
                path.push(b[0] as char);
                path.push(':');
                path.push_str(&buffer[2..]);
            } else {
                path.push('/');
                path.push_str(&buffer);
            }
        }
        if location.is_none() {
            return;
        }
    }
}

/// Where the host ends in `view` and whether it ended on a ':' outside brackets
/// (ada's `helpers::get_host_delimiter_location`).
fn host_delimiter(special: bool, view: &[u8]) -> (usize, bool) {
    let is_delim = |c: u8| matches!(c, b':' | b'/' | b'?' | b'[') || (special && c == b'\\');
    let mut location = 0;
    while location < view.len() {
        match find(&view[location..], is_delim) {
            None => return (view.len(), false),
            Some(off) => {
                location += off;
                if view[location] == b'[' {
                    match find(&view[location..], |c| c == b']') {
                        Some(end) => location += end,
                        None => return (view.len(), false),
                    }
                } else {
                    return (location, view[location] == b':');
                }
            }
        }
    }
    (view.len(), false)
}

/// ada's `checkers::is_ipv4`: does this (lowercase) host "end in a number"?
fn ends_in_number(view: &str) -> bool {
    let mut v = view.as_bytes();
    let Some(&last) = v.last() else {
        return false;
    };
    let mut last = last;
    if last == b'.' {
        v = &v[..v.len() - 1];
        match v.last() {
            Some(&c) => last = c,
            None => return false,
        }
    }
    if !(last.is_ascii_digit() || (b'a'..=b'f').contains(&last) || last == b'x') {
        return false;
    }
    if let Some(dot) = v.iter().rposition(|&c| c == b'.') {
        v = &v[dot + 1..];
    }
    if v.iter().all(u8::is_ascii_digit) {
        return true;
    }
    if v.len() == 1 || !v.starts_with(b"0x") {
        return false;
    }
    v.len() == 2 || v[2..].iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
}

/// `std::from_chars` for u32: the longest digit prefix in `radix`; `None` when there is no digit
/// or the value overflows.
fn from_chars_u32(s: &[u8], radix: u32) -> Option<(u32, usize)> {
    let mut value: u64 = 0;
    let mut n = 0;
    while n < s.len() {
        let Some(d) = (s[n] as char).to_digit(radix) else {
            break;
        };
        value = value * radix as u64 + d as u64;
        if value > u32::MAX as u64 {
            return None;
        }
        n += 1;
    }
    (n > 0).then_some((value as u32, n))
}

fn serialize_ipv4(address: u64) -> String {
    format!(
        "{}.{}.{}.{}",
        (address >> 24) as u8,
        (address >> 16) as u8,
        (address >> 8) as u8,
        address as u8
    )
}

fn serialize_ipv6(address: &[u16; 8]) -> String {
    // Longest run of zero pieces (first wins); runs of length 1 are not compressed.
    let (mut compress, mut compress_len) = (8usize, 0usize);
    let mut i = 0;
    while i < 8 {
        if address[i] == 0 {
            let mut next = i + 1;
            while next != 8 && address[next] == 0 {
                next += 1;
            }
            if next - i > compress_len {
                compress_len = next - i;
                compress = i;
            }
            i = next;
        } else {
            i += 1;
        }
    }
    if compress_len <= 1 {
        compress = 8;
    }
    let mut out = String::from("[");
    let mut piece = 0;
    loop {
        if piece == compress {
            out.push(':');
            if piece == 0 {
                out.push(':');
            }
            piece += compress_len;
            if piece == 8 {
                break;
            }
        }
        out.push_str(&format!("{:x}", address[piece]));
        piece += 1;
        if piece == 8 {
            break;
        }
        out.push(':');
    }
    out.push(']');
    out
}

fn parse_ipv6(input: &[u8]) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    let mut address = [0u16; 8];
    let mut piece_index = 0usize;
    let mut compress: Option<usize> = None;
    let mut p = 0usize;
    let at = |p: usize| input.get(p).copied();
    if input[0] == b':' {
        if at(1) != Some(b':') {
            return None;
        }
        p += 2;
        piece_index += 1;
        compress = Some(piece_index);
    }
    while p < input.len() {
        if piece_index == 8 {
            return None;
        }
        if input[p] == b':' {
            if compress.is_some() {
                return None;
            }
            p += 1;
            piece_index += 1;
            compress = Some(piece_index);
            continue;
        }
        let (mut value, mut length) = (0u16, 0usize);
        while length < 4 && p < input.len() && input[p].is_ascii_hexdigit() {
            value = value * 0x10 + (input[p] as char).to_digit(16).unwrap_or(0) as u16;
            p += 1;
            length += 1;
        }
        if at(p) == Some(b'.') {
            if length == 0 {
                return None;
            }
            p -= length;
            if piece_index > 6 {
                return None;
            }
            let mut numbers_seen = 0;
            while p < input.len() {
                let mut ipv4_piece: Option<u16> = None;
                if numbers_seen > 0 {
                    if input[p] == b'.' && numbers_seen < 4 {
                        p += 1;
                    } else {
                        return None;
                    }
                }
                if !at(p).is_some_and(|c| c.is_ascii_digit()) {
                    return None;
                }
                while let Some(c) = at(p).filter(u8::is_ascii_digit) {
                    let number = (c - b'0') as u16;
                    ipv4_piece = match ipv4_piece {
                        None => Some(number),
                        Some(0) => return None,
                        Some(v) => Some(v * 10 + number),
                    };
                    if ipv4_piece > Some(255) {
                        return None;
                    }
                    p += 1;
                }
                address[piece_index] = address[piece_index]
                    .wrapping_mul(0x100)
                    .wrapping_add(ipv4_piece.unwrap_or(0));
                numbers_seen += 1;
                if numbers_seen == 2 || numbers_seen == 4 {
                    piece_index += 1;
                }
            }
            if numbers_seen != 4 {
                return None;
            }
            break;
        } else if at(p) == Some(b':') {
            p += 1;
            if p == input.len() {
                return None;
            }
        } else if p < input.len() {
            return None;
        }
        address[piece_index] = value;
        piece_index += 1;
    }
    match compress {
        Some(c) => {
            let mut swaps = piece_index - c;
            piece_index = 7;
            while piece_index != 0 && swaps > 0 {
                address.swap(piece_index, c + swaps - 1);
                piece_index -= 1;
                swaps -= 1;
            }
        }
        None if piece_index != 8 => return None,
        None => {}
    }
    Some(serialize_ipv6(&address))
}

/// ada's `url::parse_ipv4` (note: an all-decimal dotted quad is kept as written).
fn parse_ipv4(input: &str) -> Option<String> {
    let mut input = input.as_bytes();
    if input.last() == Some(&b'.') {
        input = &input[..input.len() - 1];
    }
    let original = input;
    let mut digit_count = 0u32;
    let mut pure_decimal = 0;
    let mut ipv4: u64 = 0;
    let mut finished = false;
    while digit_count < 4 && !input.is_empty() {
        let is_hex = input.len() >= 2 && input[0] == b'0' && (input[1] | 0x20) == b'x';
        let segment: u32;
        if is_hex && (input.len() == 2 || input[2] == b'.') {
            segment = 0;
            input = &input[2..];
        } else {
            let (v, n) = if is_hex {
                from_chars_u32(&input[2..], 16).map(|(v, n)| (v, n + 2))?
            } else if input.len() >= 2 && input[0] == b'0' && input[1].is_ascii_digit() {
                from_chars_u32(&input[1..], 8).map(|(v, n)| (v, n + 1))?
            } else {
                pure_decimal += 1;
                from_chars_u32(input, 10)?
            };
            segment = v;
            input = &input[n..];
        }
        if input.is_empty() {
            if segment as u64 > (1u64 << (32 - digit_count * 8)) {
                return None;
            }
            ipv4 <<= 32 - digit_count * 8;
            ipv4 |= segment as u64;
            finished = true;
            break;
        }
        if segment > 255 || input[0] != b'.' {
            return None;
        }
        ipv4 <<= 8;
        ipv4 |= segment as u64;
        input = &input[1..];
        digit_count += 1;
    }
    if !finished && (digit_count != 4 || !input.is_empty()) {
        return None;
    }
    if pure_decimal == 4 {
        Some(String::from_utf8_lossy(original).into_owned())
    } else {
        Some(serialize_ipv4(ipv4))
    }
}

/// Host parsing (`url::parse_host`): IPv6 literal, opaque host (non-special schemes), or a
/// domain through UTS #46 and the IPv4 parser.
fn parse_host(input: &[u8], special: bool) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    if input[0] == b'[' {
        if input.last() != Some(&b']') {
            return None;
        }
        return parse_ipv6(&input[1..input.len() - 1]);
    }
    if !special {
        if input.iter().any(|&c| is_forbidden_host(c)) {
            return None;
        }
        return Some(percent_encode(input, c0_control_set));
    }
    let lower = input.to_ascii_lowercase();
    if !lower.iter().any(|&c| is_forbidden_domain(c)) && !lower.windows(3).any(|w| w == b"xn-") {
        let host = String::from_utf8(lower).ok()?;
        if ends_in_number(&host) {
            return parse_ipv4(&host);
        }
        return Some(host);
    }
    let decoded = if input.contains(&b'%') {
        percent_decode(input)
    } else {
        input.to_vec()
    };
    let ascii = idna::to_ascii(&decoded)?;
    if ascii.bytes().any(is_forbidden_domain) {
        return None;
    }
    if ends_in_number(&ascii) {
        return parse_ipv4(&ascii);
    }
    Some(ascii)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    SchemeStart,
    Scheme,
    NoScheme,
    Authority,
    SpecialRelativeOrAuthority,
    PathOrAuthority,
    RelativeScheme,
    RelativeSlash,
    SpecialAuthoritySlashes,
    SpecialAuthorityIgnoreSlashes,
    Query,
    Host,
    OpaquePath,
    Port,
    PathStart,
    Path,
    FileSlash,
    FileHost,
    File,
}

impl Url {
    pub fn is_special(&self) -> bool {
        self.kind != kind::NOT_SPECIAL
    }

    fn has_credentials(&self) -> bool {
        !self.username.is_empty() || !self.password.is_empty()
    }

    fn cannot_have_credentials_or_port(&self) -> bool {
        self.host.as_deref().map_or(true, str::is_empty) || self.kind == kind::FILE
    }

    fn set_kind_scheme(&mut self, scheme: String) {
        self.kind = scheme_kind(&scheme);
        self.scheme = scheme;
    }

    fn copy_scheme(&mut self, base: &Url) {
        self.scheme = base.scheme.clone();
        self.kind = base.kind;
    }

    fn set_fragment(&mut self, fragment: Option<&[u8]>) {
        if let Some(f) = fragment {
            self.fragment = Some(percent_encode(f, fragment_set));
        }
    }

    /// The hostname (empty when there is no host).
    pub fn hostname(&self) -> &str {
        self.host.as_deref().unwrap_or("")
    }

    /// The HTTP request-target: path ("/" when empty) plus "?query" when a query is present.
    pub fn request_target(&self) -> String {
        let mut out = if self.path.is_empty() {
            "/".to_string()
        } else {
            self.path.clone()
        };
        if let Some(q) = &self.query {
            out.push('?');
            out.push_str(q);
        }
        out
    }

    pub fn href(&self) -> String {
        let mut out = format!("{}:", self.scheme);
        if let Some(host) = &self.host {
            out.push_str("//");
            if self.has_credentials() {
                out.push_str(&self.username);
                if !self.password.is_empty() {
                    out.push(':');
                    out.push_str(&self.password);
                }
                out.push('@');
            }
            out.push_str(host);
            if let Some(p) = self.port {
                out.push(':');
                out.push_str(&p.to_string());
            }
        } else if !self.opaque && self.path.starts_with("//") {
            out.push_str("/.");
        }
        out.push_str(&self.path);
        if let Some(q) = &self.query {
            out.push('?');
            out.push_str(q);
        }
        if let Some(f) = &self.fragment {
            out.push('#');
            out.push_str(f);
        }
        out
    }

    /// The offsets Node's `URLContext` reads: protocol_end, username_end, host_start, host_end,
    /// port, pathname_start, search_start, hash_start, scheme_type (ada's url_components).
    pub fn components(&self) -> [u32; 9] {
        let protocol_end = self.scheme.len() as u32 + 1;
        let (username_end, host_start, host_end, mut running);
        if let Some(host) = &self.host {
            let start = protocol_end + 2;
            if self.has_credentials() {
                username_end = start + self.username.len() as u32;
                let mut hs = username_end;
                if !self.password.is_empty() {
                    hs += self.password.len() as u32 + 1;
                }
                host_start = hs;
                host_end = hs + 1 + host.len() as u32;
            } else {
                username_end = start;
                host_start = start;
                host_end = start + host.len() as u32;
            }
            running = host_end;
        } else {
            username_end = protocol_end;
            host_start = protocol_end;
            host_end = protocol_end;
            running = if !self.opaque && self.path.starts_with("//") {
                protocol_end + 2
            } else {
                protocol_end
            };
        }
        let port = match self.port {
            Some(p) => {
                running += p.to_string().len() as u32 + 1;
                p as u32
            }
            None => OMITTED,
        };
        let pathname_start = running;
        running += self.path.len() as u32;
        let search_start = match &self.query {
            Some(q) => {
                let s = running;
                running += q.len() as u32 + 1;
                s
            }
            None => OMITTED,
        };
        let hash_start = if self.fragment.is_some() {
            running
        } else {
            OMITTED
        };
        [
            protocol_end,
            username_end,
            host_start,
            host_end,
            port,
            pathname_start,
            search_start,
            hash_start,
            self.kind as u32,
        ]
    }

    /// `url::parse_scheme` (lowercased `input`); with a state override it may decline silently.
    fn parse_scheme(&mut self, input: &str, state_override: bool) {
        let new_kind = scheme_kind(input);
        if state_override {
            if self.is_special() != (new_kind != kind::NOT_SPECIAL) {
                return;
            }
            if (self.has_credentials() || self.port.is_some()) && new_kind == kind::FILE {
                return;
            }
            if self.kind == kind::FILE && self.host.as_deref() == Some("") {
                return;
            }
        }
        self.set_kind_scheme(input.to_string());
        if state_override {
            let default = default_port_of(self.kind);
            if default != 0 && self.port == Some(default) {
                self.port = None;
            }
        }
    }

    /// `url::parse_port`: returns (bytes consumed, still valid).
    fn parse_port(&mut self, view: &[u8], check_trailing: bool) -> (usize, bool) {
        let mut value: u32 = 0;
        let mut consumed = 0;
        while consumed < view.len() && view[consumed].is_ascii_digit() {
            value = value * 10 + (view[consumed] - b'0') as u32;
            if value > u16::MAX as u32 {
                return (0, false);
            }
            consumed += 1;
        }
        let mut valid = true;
        if check_trailing {
            valid = consumed == view.len()
                || view[consumed] == b'/'
                || view[consumed] == b'?'
                || (self.is_special() && view[consumed] == b'\\');
        }
        if valid {
            let default = default_port_of(self.kind) as u32;
            let port_ok = (default == 0 && value == 0) || default != value;
            self.port = (consumed > 0 && port_ok).then_some(value as u16);
        }
        (consumed, valid)
    }

    /// `url::parse_path` (the pathname setter's parse).
    fn parse_path(&mut self, input: &[u8]) {
        let input = remove_tab_newline(input);
        if self.is_special() {
            if input.is_empty() {
                self.path = "/".into();
            } else if input[0] == b'/' || input[0] == b'\\' {
                parse_prepared_path(&input[1..], self.kind, &mut self.path);
            } else {
                parse_prepared_path(&input, self.kind, &mut self.path);
            }
        } else if !input.is_empty() {
            if input[0] == b'/' {
                parse_prepared_path(&input[1..], self.kind, &mut self.path);
            } else {
                parse_prepared_path(&input, self.kind, &mut self.path);
            }
        } else if self.host.is_none() {
            self.path = "/".into();
        }
    }

    // ---- setters (ada::url set_*); false means "leave the URL unchanged" ----

    pub fn set_href(&mut self, input: &str) -> bool {
        match parse_url(input, None) {
            Some(u) => {
                *self = u;
                true
            }
            None => false,
        }
    }

    pub fn set_protocol(&mut self, input: &str) -> bool {
        let mut view = remove_tab_newline(input.as_bytes());
        if view.is_empty() {
            return true;
        }
        if !view[0].is_ascii_alphabetic() {
            return false;
        }
        view.push(b':');
        let end = view.iter().position(|&c| !is_alnum_plus(c)).unwrap_or(view.len());
        if view.get(end) == Some(&b':') {
            let scheme = String::from_utf8_lossy(&view[..end]).to_ascii_lowercase();
            self.parse_scheme(&scheme, true);
            return true;
        }
        false
    }

    pub fn set_username(&mut self, input: &str) -> bool {
        if self.cannot_have_credentials_or_port() {
            return false;
        }
        self.username = percent_encode(input.as_bytes(), userinfo_set);
        true
    }

    pub fn set_password(&mut self, input: &str) -> bool {
        if self.cannot_have_credentials_or_port() {
            return false;
        }
        self.password = percent_encode(input.as_bytes(), userinfo_set);
        true
    }

    pub fn set_port(&mut self, input: &str) -> bool {
        if self.cannot_have_credentials_or_port() {
            return false;
        }
        let trimmed = remove_tab_newline(input.as_bytes());
        if trimmed.is_empty() {
            self.port = None;
            return true;
        }
        if trimmed[0] <= 0x20 {
            return false;
        }
        if !input.bytes().any(|c| c.is_ascii_digit()) {
            return false;
        }
        let previous = self.port;
        let (_, valid) = self.parse_port(&trimmed, false);
        if valid {
            return true;
        }
        self.port = previous;
        false
    }

    fn set_host_or_hostname(&mut self, input: &str, hostname_only: bool) -> bool {
        if self.opaque {
            return false;
        }
        let previous_host = self.host.clone();
        let previous_port = self.port;
        let raw = input.as_bytes();
        let raw = &raw[..find(raw, |c| c == b'#').unwrap_or(raw.len())];
        let new_host = remove_tab_newline(raw);

        if self.kind != kind::FILE {
            let (location, found_colon) = host_delimiter(self.is_special(), &new_host);
            let host_view = &new_host[..location];
            if found_colon {
                if hostname_only {
                    return false;
                }
                let rest = &new_host[location + 1..];
                if !rest.is_empty() {
                    self.set_port(&String::from_utf8_lossy(rest));
                }
            } else if host_view.is_empty()
                && (self.is_special() || self.has_credentials() || self.port.is_some())
            {
                return false;
            }
            if host_view.is_empty() && !self.is_special() {
                self.host = Some(String::new());
                return true;
            }
            match parse_host(host_view, self.is_special()) {
                Some(h) => {
                    self.host = Some(h);
                    true
                }
                None => {
                    self.host = previous_host;
                    self.port = previous_port;
                    false
                }
            }
        } else {
            let end = find(&new_host, |c| matches!(c, b'/' | b'\\' | b'?')).unwrap_or(new_host.len());
            let host_view = &new_host[..end];
            if host_view.is_empty() {
                self.host = Some(String::new());
            } else {
                match parse_host(host_view, true) {
                    Some(h) => {
                        self.host = Some(if h == "localhost" { String::new() } else { h });
                    }
                    None => {
                        self.host = previous_host;
                        self.port = previous_port;
                        return false;
                    }
                }
            }
            true
        }
    }

    pub fn set_host(&mut self, input: &str) -> bool {
        self.set_host_or_hostname(input, false)
    }

    pub fn set_hostname(&mut self, input: &str) -> bool {
        self.set_host_or_hostname(input, true)
    }

    pub fn set_pathname(&mut self, input: &str) -> bool {
        if self.opaque {
            return false;
        }
        self.path.clear();
        self.parse_path(input.as_bytes());
        true
    }

    fn strip_trailing_spaces_from_opaque_path(&mut self) {
        if !self.opaque || self.fragment.is_some() || self.query.is_some() {
            return;
        }
        let trimmed = self.path.trim_end_matches(' ').len();
        self.path.truncate(trimmed);
    }

    pub fn set_search(&mut self, input: &str) {
        if input.is_empty() {
            self.query = None;
            self.strip_trailing_spaces_from_opaque_path();
            return;
        }
        let value = input.strip_prefix('?').unwrap_or(input);
        let value = remove_tab_newline(value.as_bytes());
        let set: EncodeSet = if self.is_special() {
            special_query_set
        } else {
            query_set
        };
        self.query = Some(percent_encode(&value, set));
    }

    pub fn set_hash(&mut self, input: &str) {
        if input.is_empty() {
            self.fragment = None;
            self.strip_trailing_spaces_from_opaque_path();
            return;
        }
        let value = input.strip_prefix('#').unwrap_or(input);
        let value = remove_tab_newline(value.as_bytes());
        self.fragment = Some(percent_encode(&value, fragment_set));
    }
}

/// The basic URL parser (ada's `parser::parse_url` for `ada::url`); `None` on failure.
pub(crate) fn parse_url(user_input: &str, base: Option<&Url>) -> Option<Url> {
    let mut url = Url::default();
    let cleaned = remove_tab_newline(user_input.as_bytes());
    let mut data: &[u8] = &cleaned;
    while let Some((&first, rest)) = data.split_first() {
        if first > 0x20 {
            break;
        }
        data = rest;
    }
    while let Some((&last, rest)) = data.split_last() {
        if last > 0x20 {
            break;
        }
        data = rest;
    }
    let fragment: Option<&[u8]> = match find(data, |c| c == b'#') {
        Some(i) => {
            let f = &data[i + 1..];
            data = &data[..i];
            Some(f)
        }
        None => None,
    };
    let size = data.len();
    let mut pos = 0usize;
    let mut state = State::SchemeStart;
    let at = |p: usize| data.get(p).copied();

    while pos <= size {
        match state {
            State::SchemeStart => {
                if at(pos).is_some_and(|c| c.is_ascii_alphabetic()) {
                    state = State::Scheme;
                    pos += 1;
                } else {
                    state = State::NoScheme;
                }
            }
            State::Scheme => {
                while at(pos).is_some_and(is_alnum_plus) {
                    pos += 1;
                }
                if at(pos) == Some(b':') {
                    let scheme = String::from_utf8_lossy(&data[..pos]).to_ascii_lowercase();
                    url.parse_scheme(&scheme, false);
                    if url.kind == kind::FILE {
                        state = State::File;
                    } else if url.is_special() && base.is_some_and(|b| b.kind == url.kind) {
                        state = State::SpecialRelativeOrAuthority;
                    } else if url.is_special() {
                        state = State::SpecialAuthoritySlashes;
                    } else if pos + 1 < size && data[pos + 1] == b'/' {
                        state = State::PathOrAuthority;
                        pos += 1;
                    } else {
                        state = State::OpaquePath;
                    }
                } else {
                    state = State::NoScheme;
                    pos = 0;
                    continue;
                }
                pos += 1;
            }
            State::NoScheme => {
                let b = match base {
                    None => return None,
                    Some(b) if b.opaque && fragment.is_none() => return None,
                    Some(b) => b,
                };
                if b.opaque && fragment.is_some() && pos == size {
                    url.copy_scheme(b);
                    url.opaque = b.opaque;
                    url.path = b.path.clone();
                    url.query = b.query.clone();
                    url.set_fragment(fragment);
                    return Some(url);
                } else if b.kind != kind::FILE {
                    state = State::RelativeScheme;
                } else {
                    state = State::File;
                }
            }
            State::Authority => {
                if !data[pos..].contains(&b'@') {
                    state = State::Host;
                    continue;
                }
                let mut at_sign_seen = false;
                let mut password_token_seen = false;
                loop {
                    let view = &data[pos..];
                    let special = url.is_special();
                    let location = find(view, |c| {
                        matches!(c, b'@' | b'/' | b'?') || (special && c == b'\\')
                    })
                    .unwrap_or(view.len());
                    let authority = &view[..location];
                    let end = pos + location;
                    if end != size && data[end] == b'@' {
                        if at_sign_seen {
                            if password_token_seen {
                                url.password.push_str("%40");
                            } else {
                                url.username.push_str("%40");
                            }
                        }
                        at_sign_seen = true;
                        if !password_token_seen {
                            match find(authority, |c| c == b':') {
                                Some(colon) => {
                                    password_token_seen = true;
                                    url.username
                                        .push_str(&percent_encode(&authority[..colon], userinfo_set));
                                    url.password.push_str(&percent_encode(
                                        &authority[colon + 1..],
                                        userinfo_set,
                                    ));
                                }
                                None => url
                                    .username
                                    .push_str(&percent_encode(authority, userinfo_set)),
                            }
                        } else {
                            url.password.push_str(&percent_encode(authority, userinfo_set));
                        }
                    } else if end == size
                        || data[end] == b'/'
                        || data[end] == b'?'
                        || (special && data[end] == b'\\')
                    {
                        if at_sign_seen && authority.is_empty() {
                            return None;
                        }
                        state = State::Host;
                        break;
                    }
                    if end == size {
                        url.set_fragment(fragment);
                        return Some(url);
                    }
                    pos = end + 1;
                }
            }
            State::SpecialRelativeOrAuthority => {
                if data[pos..].starts_with(b"//") {
                    state = State::SpecialAuthorityIgnoreSlashes;
                    pos += 2;
                } else {
                    state = State::RelativeScheme;
                }
            }
            State::PathOrAuthority => {
                if at(pos) == Some(b'/') {
                    state = State::Authority;
                    pos += 1;
                } else {
                    state = State::Path;
                }
            }
            State::RelativeScheme => {
                let b = base?;
                url.copy_scheme(b);
                if at(pos) == Some(b'/') || (url.is_special() && at(pos) == Some(b'\\')) {
                    state = State::RelativeSlash;
                } else {
                    url.username = b.username.clone();
                    url.password = b.password.clone();
                    url.host = b.host.clone();
                    url.port = b.port;
                    url.opaque = b.opaque;
                    url.path = b.path.clone();
                    url.query = b.query.clone();
                    if at(pos) == Some(b'?') {
                        state = State::Query;
                    } else if pos != size {
                        url.query = None;
                        shorten_path(&mut url.path, url.kind);
                        state = State::Path;
                        continue;
                    }
                }
                pos += 1;
            }
            State::RelativeSlash => {
                let b = base?;
                if url.is_special() && matches!(at(pos), Some(b'/' | b'\\')) {
                    state = State::SpecialAuthorityIgnoreSlashes;
                } else if at(pos) == Some(b'/') {
                    state = State::Authority;
                } else {
                    url.username = b.username.clone();
                    url.password = b.password.clone();
                    url.host = b.host.clone();
                    url.port = b.port;
                    state = State::Path;
                    continue;
                }
                pos += 1;
            }
            State::SpecialAuthoritySlashes | State::SpecialAuthorityIgnoreSlashes => {
                if state == State::SpecialAuthoritySlashes && data[pos..].starts_with(b"//") {
                    pos += 2;
                }
                while matches!(at(pos), Some(b'/' | b'\\')) {
                    pos += 1;
                }
                state = State::Authority;
            }
            State::Query => {
                let set: EncodeSet = if url.is_special() {
                    special_query_set
                } else {
                    query_set
                };
                url.query = Some(percent_encode(&data[pos..], set));
                url.set_fragment(fragment);
                return Some(url);
            }
            State::Host => {
                let view = &data[pos..];
                let (location, found_colon) = host_delimiter(url.is_special(), view);
                let host_view = &view[..location];
                pos += location;
                if found_colon {
                    url.host = Some(parse_host(host_view, url.is_special())?);
                    state = State::Port;
                    pos += 1;
                } else {
                    if url.is_special() && host_view.is_empty() {
                        return None;
                    }
                    url.host = Some(if host_view.is_empty() {
                        String::new()
                    } else {
                        parse_host(host_view, url.is_special())?
                    });
                    state = State::PathStart;
                }
            }
            State::OpaquePath => {
                let mut view = &data[pos..];
                match find(view, |c| c == b'?') {
                    Some(q) => {
                        view = &view[..q];
                        state = State::Query;
                        pos += q + 1;
                    }
                    None => pos = size + 1,
                }
                url.opaque = true;
                url.path = percent_encode(view, c0_control_set);
            }
            State::Port | State::PathStart => {
                if state == State::Port {
                    let (consumed, valid) = url.parse_port(&data[pos..], true);
                    pos += consumed;
                    if !valid {
                        return None;
                    }
                    state = State::PathStart;
                }
                if url.is_special() {
                    state = State::Path;
                    if pos == size {
                        url.path = "/".into();
                        url.set_fragment(fragment);
                        return Some(url);
                    }
                    if data[pos] != b'/' && data[pos] != b'\\' {
                        continue;
                    }
                } else if at(pos) == Some(b'?') {
                    state = State::Query;
                } else if pos != size {
                    state = State::Path;
                    if data[pos] != b'/' {
                        continue;
                    }
                }
                pos += 1;
            }
            State::Path => {
                let mut view = &data[pos..];
                match find(view, |c| c == b'?') {
                    Some(q) => {
                        state = State::Query;
                        view = &view[..q];
                        pos += q + 1;
                    }
                    None => pos = size + 1,
                }
                parse_prepared_path(view, url.kind, &mut url.path);
            }
            State::FileSlash => {
                if matches!(at(pos), Some(b'/' | b'\\')) {
                    state = State::FileHost;
                    pos += 1;
                } else {
                    if let Some(b) = base.filter(|b| b.kind == kind::FILE) {
                        url.host = b.host.clone();
                        if !b.path.is_empty() && !is_windows_drive_letter(&data[pos..]) {
                            let first = &b.path[1..];
                            let first = &first[..first.find('/').unwrap_or(first.len())];
                            if is_normalized_windows_drive_letter(first.as_bytes()) {
                                url.path.push('/');
                                url.path.push_str(first);
                            }
                        }
                    }
                    state = State::Path;
                }
            }
            State::FileHost => {
                let view = &data[pos..];
                let end = find(view, |c| matches!(c, b'/' | b'\\' | b'?')).unwrap_or(view.len());
                let buffer = &view[..end];
                if is_windows_drive_letter(buffer) {
                    state = State::Path;
                } else if buffer.is_empty() {
                    url.host = Some(String::new());
                    state = State::PathStart;
                } else {
                    pos += buffer.len();
                    let h = parse_host(buffer, url.is_special())?;
                    url.host = Some(if h == "localhost" { String::new() } else { h });
                    state = State::PathStart;
                }
            }
            State::File => {
                let file_view = &data[pos..];
                url.set_kind_scheme("file".into());
                url.host = Some(String::new());
                if matches!(at(pos), Some(b'/' | b'\\')) {
                    state = State::FileSlash;
                } else if let Some(b) = base.filter(|b| b.kind == kind::FILE) {
                    url.host = b.host.clone();
                    url.path = b.path.clone();
                    url.query = b.query.clone();
                    url.opaque = b.opaque;
                    if at(pos) == Some(b'?') {
                        state = State::Query;
                    } else if pos != size {
                        url.query = None;
                        if !is_windows_drive_letter(file_view) {
                            shorten_path(&mut url.path, url.kind);
                        } else {
                            url.path.clear();
                            url.opaque = true;
                        }
                        state = State::Path;
                        continue;
                    }
                } else {
                    state = State::Path;
                    continue;
                }
                pos += 1;
            }
        }
    }
    url.set_fragment(fragment);
    Some(url)
}

/// `input` parsed on its own, or against `base` when given. The error text is for Rust callers
/// (fetch/WebSocket/EventSource); the JS `URL` class reports Node's `ERR_INVALID_URL` instead.
pub(crate) fn parse(input: &str, base: Option<&str>) -> Result<Url, String> {
    let base = match base {
        Some(b) => Some(parse_url(b, None).ok_or_else(|| format!("invalid base URL '{b}'"))?),
        None => None,
    };
    parse_url(input, base.as_ref()).ok_or_else(|| format!("invalid URL '{input}'"))
}

/// Node's `url.domainToASCII`: the hostname a special URL would get, or "" when invalid.
pub(crate) fn domain_to_ascii(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut u = parse_url("ws://x", None).unwrap_or_default();
    if !u.set_hostname(input) {
        return String::new();
    }
    u.hostname().to_string()
}

/// Node's `url.domainToUnicode`: `domainToASCII`, then decode the punycode labels.
pub(crate) fn domain_to_unicode(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut u = parse_url("ws://x", None).unwrap_or_default();
    if !u.set_hostname(input) {
        return String::new();
    }
    domain_to_unicode_raw(u.hostname())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn href(s: &str) -> String {
        parse(s, None).unwrap().href()
    }

    #[test]
    fn absolute_basics() {
        let u = parse("HTTP://User:Pw@Example.COM:8080/a/b?q=1#frag", None).unwrap();
        assert_eq!(u.scheme, "http");
        assert_eq!(u.username, "User");
        assert_eq!(u.password, "Pw");
        assert_eq!(u.hostname(), "example.com");
        assert_eq!(u.port, Some(8080));
        assert_eq!(u.path, "/a/b");
        assert_eq!(u.query.as_deref(), Some("q=1"));
        assert_eq!(u.href(), "http://User:Pw@example.com:8080/a/b?q=1#frag");
    }

    #[test]
    fn default_port_dropped_and_path_added() {
        assert_eq!(href("http://x.com:80"), "http://x.com/");
        assert_eq!(href("https://x.com:443/a"), "https://x.com/a");
        assert_eq!(parse("http://x.com:8080", None).unwrap().port, Some(8080));
    }

    #[test]
    fn path_normalization() {
        assert_eq!(parse("http://x.com/a/b/../c/./d", None).unwrap().path, "/a/c/d");
        assert_eq!(parse("http://x.com/a/..", None).unwrap().path, "/");
        assert_eq!(parse("http://x.com/a/b/", None).unwrap().path, "/a/b/");
    }

    #[test]
    fn relative_resolution() {
        let base = Some("http://x.com/a/b/c?old#f");
        let r = |s: &str| parse(s, base).unwrap().href();
        assert_eq!(r("d"), "http://x.com/a/b/d");
        assert_eq!(r("../d"), "http://x.com/a/d");
        assert_eq!(r("/d"), "http://x.com/d");
        assert_eq!(r("?q=2"), "http://x.com/a/b/c?q=2");
        assert_eq!(r("#g"), "http://x.com/a/b/c?old#g");
        assert_eq!(r("//y.com/z"), "http://y.com/z");
    }

    #[test]
    fn ipv6_ipv4_and_errors() {
        let u = parse("http://[::1]:9000/x", None).unwrap();
        assert_eq!(u.hostname(), "[::1]");
        assert_eq!(u.port, Some(9000));
        assert_eq!(href("http://[0:0:0:0:0:0:0:1]/"), "http://[::1]/");
        assert_eq!(href("http://0x7f.1/"), "http://127.0.0.1/");
        assert!(parse("http://", None).is_err());
        assert!(parse("nobase", None).is_err());
        assert_eq!(href("http://bücher.de/"), "http://xn--bcher-kva.de/");
    }

    #[test]
    fn opaque_schemes() {
        let u = parse("mailto:a@b.c", None).unwrap();
        assert_eq!(u.scheme, "mailto");
        assert_eq!(u.path, "a@b.c");
        assert!(u.opaque);
        assert_eq!(u.href(), "mailto:a@b.c");
    }

    #[test]
    fn file_urls_and_drive_letters() {
        let b = char::from(92);
        assert_eq!(href(&format!("file://D:{b}Sources{b}x")), "file:///D:/Sources/x");
        assert_eq!(href("file://D:/x"), "file:///D:/x");
        assert_eq!(href(&format!("file:D:{b}x")), "file:///D:/x");
        assert_eq!(href("file://localhost/D:/x"), "file:///D:/x");
        assert_eq!(href("file://C|/x"), "file:///C:/x");
        assert_eq!(href("file:///C:/../x"), "file:///C:/x");
        assert_eq!(href("file://D:"), "file:///D:");
        assert_eq!(href("file:/x/../../y"), "file:///y");
        assert_eq!(href("file://host/share/x"), "file://host/share/x");
    }

    #[test]
    fn components_match_node() {
        let c = parse("https://u:p@host:81/p?q#h", None).unwrap().components();
        assert_eq!(c, [6, 9, 11, 16, 81, 19, 21, 23, 2]);
        let c = parse("web+demo:/.//not-a-host/", None).unwrap().components();
        assert_eq!(c, [9, 9, 9, 9, OMITTED, 11, OMITTED, OMITTED, 1]);
    }
}
