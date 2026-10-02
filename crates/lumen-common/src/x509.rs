//! X.509 certificate reading shared by every language's TLS layer: a small lenient TLV walk (so
//! certificates OpenSSL accepts, with odd string types or invalid names inside extensions, still
//! parse), the text formats of OpenSSL's `X509_NAME_print_ex`, `ASN1_TIME_print` and Node's
//! `PrintGeneralName`, PEM framing, and [`decode_cert`], the structured form Python's
//! `ssl.getpeercert()` reports. No OS calls, no cryptography.

use crate::codec::{self, Padding};
use codec::hex_encode_upper as hex_upper;

#[derive(Clone, Copy)]
pub struct Tlv<'a> {
    pub tag: u8,
    pub value: &'a [u8],
    pub raw: &'a [u8],
}

pub fn read_tlv<'a>(input: &mut &'a [u8]) -> Option<Tlv<'a>> {
    let data = *input;
    let tag = *data.first()?;
    if tag & 0x1f == 0x1f {
        return None;
    }
    let first = *data.get(1)? as usize;
    let (len, header) = if first < 0x80 {
        (first, 2)
    } else {
        let count = first & 0x7f;
        if count == 0 || count > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..count {
            len = (len << 8) | *data.get(2 + i)? as usize;
        }
        (len, 2 + count)
    };
    let end = header.checked_add(len)?;
    if end > data.len() {
        return None;
    }
    *input = &data[end..];
    Some(Tlv { tag, value: &data[header..end], raw: &data[..end] })
}

pub fn expect<'a>(input: &mut &'a [u8], tag: u8) -> Option<Tlv<'a>> {
    let t = read_tlv(input)?;
    (t.tag == tag).then_some(t)
}

pub fn children(value: &[u8]) -> Option<Vec<Tlv<'_>>> {
    let mut rest = value;
    let mut out = Vec::new();
    while !rest.is_empty() {
        out.push(read_tlv(&mut rest)?);
    }
    Some(out)
}

pub const SEQ: u8 = 0x30;
pub const SET: u8 = 0x31;
pub const OID: u8 = 0x06;
pub const INTEGER: u8 = 0x02;
pub const BIT_STRING: u8 = 0x03;
pub const OCTET_STRING: u8 = 0x04;
pub const BOOLEAN: u8 = 0x01;
pub const UTF8_STRING: u8 = 0x0c;
pub const IA5_STRING: u8 = 0x16;

pub fn oid_text(content: &[u8]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut value: u128 = 0;
    for &b in content {
        value = (value << 7) | (b & 0x7f) as u128;
        if b & 0x80 == 0 {
            if parts.is_empty() {
                let first = if value < 80 { value / 40 } else { 2 };
                parts.push(first.to_string());
                parts.push((value - first * 40).to_string());
            } else {
                parts.push(value.to_string());
            }
            value = 0;
        }
    }
    parts.join(".")
}

pub struct Extension<'a> {
    pub oid: String,
    pub value: &'a [u8],
}

pub struct Cert<'a> {
    pub raw: &'a [u8],
    pub tbs: &'a [u8],
    pub version: u32,
    pub serial: &'a [u8],
    pub issuer: Tlv<'a>,
    pub not_before: Tlv<'a>,
    pub not_after: Tlv<'a>,
    pub subject: Tlv<'a>,
    pub spki: Tlv<'a>,
    pub extensions: Vec<Extension<'a>>,
    pub sig_alg: Tlv<'a>,
    pub signature: &'a [u8],
}

pub fn parse_cert(der: &[u8]) -> Option<Cert<'_>> {
    let mut input = der;
    let cert = expect(&mut input, SEQ)?;
    let mut body = cert.value;
    let tbs = expect(&mut body, SEQ)?;
    let sig_alg = expect(&mut body, SEQ)?;
    let sig = expect(&mut body, BIT_STRING)?;
    let signature = sig.value.get(1..)?;

    let mut t = tbs.value;
    let mut next = read_tlv(&mut t)?;
    let mut version = 0u32;
    if next.tag == 0xa0 {
        let mut inner = next.value;
        if let Some(value) = expect(&mut inner, INTEGER) {
            version = value.value.iter().fold(0u32, |acc, &b| (acc << 8) | b as u32);
        }
        next = read_tlv(&mut t)?;
    }
    if next.tag != INTEGER {
        return None;
    }
    let serial = next.value;
    expect(&mut t, SEQ)?;
    let issuer = expect(&mut t, SEQ)?;
    let validity = expect(&mut t, SEQ)?;
    let mut v = validity.value;
    let not_before = read_tlv(&mut v)?;
    let not_after = read_tlv(&mut v)?;
    let subject = expect(&mut t, SEQ)?;
    let spki = expect(&mut t, SEQ)?;
    let mut extensions = Vec::new();
    while !t.is_empty() {
        let item = read_tlv(&mut t)?;
        if item.tag == 0xa3 {
            let mut inner = item.value;
            let seq = expect(&mut inner, SEQ)?;
            for ext in children(seq.value)? {
                let mut e = ext.value;
                let oid = expect(&mut e, OID)?;
                let mut value = read_tlv(&mut e)?;
                if value.tag == BOOLEAN {
                    value = read_tlv(&mut e)?;
                }
                if value.tag != OCTET_STRING {
                    return None;
                }
                extensions.push(Extension { oid: oid_text(oid.value), value: value.value });
            }
        }
    }
    Some(Cert {
        raw: cert.raw,
        tbs: tbs.raw,
        version,
        serial,
        issuer,
        not_before,
        not_after,
        subject,
        spki,
        extensions,
        sig_alg,
        signature,
    })
}

impl<'a> Cert<'a> {
    pub fn ext(&self, oid: &str) -> Option<&'a [u8]> {
        self.extensions.iter().find(|e| e.oid == oid).map(|e| e.value)
    }
}

pub const OID_SAN: &str = "2.5.29.17";
pub const OID_AIA: &str = "1.3.6.1.5.5.7.1.1";
pub const OID_EKU: &str = "2.5.29.37";
pub const OID_BASIC_CONSTRAINTS: &str = "2.5.29.19";
pub const OID_KEY_USAGE: &str = "2.5.29.15";
pub const OID_SKI: &str = "2.5.29.14";
pub const OID_AKI: &str = "2.5.29.35";

pub const OID_RSA: &str = "1.2.840.113549.1.1.1";
pub const OID_RSA_PSS: &str = "1.2.840.113549.1.1.10";
pub const OID_EC: &str = "1.2.840.10045.2.1";
pub const OID_ED25519: &str = "1.3.101.112";
pub const OID_X25519: &str = "1.3.101.110";
pub const OID_ED448: &str = "1.3.101.113";
pub const OID_X448: &str = "1.3.101.111";
pub const OID_DSA: &str = "1.2.840.10040.4.1";

/// OpenSSL's short names of the attribute types it knows.
pub fn attribute_short_name(oid: &str) -> Option<&'static str> {
    Some(match oid {
        "2.5.4.3" => "CN",
        "2.5.4.4" => "SN",
        "2.5.4.5" => "serialNumber",
        "2.5.4.6" => "C",
        "2.5.4.7" => "L",
        "2.5.4.8" => "ST",
        "2.5.4.9" => "street",
        "2.5.4.10" => "O",
        "2.5.4.11" => "OU",
        "2.5.4.12" => "title",
        "2.5.4.13" => "description",
        "2.5.4.14" => "searchGuide",
        "2.5.4.15" => "businessCategory",
        "2.5.4.16" => "postalAddress",
        "2.5.4.17" => "postalCode",
        "2.5.4.18" => "postOfficeBox",
        "2.5.4.19" => "physicalDeliveryOfficeName",
        "2.5.4.20" => "telephoneNumber",
        "2.5.4.21" => "telexNumber",
        "2.5.4.22" => "teletexTerminalIdentifier",
        "2.5.4.23" => "facsimileTelephoneNumber",
        "2.5.4.24" => "x121Address",
        "2.5.4.25" => "internationaliSDNNumber",
        "2.5.4.26" => "registeredAddress",
        "2.5.4.27" => "destinationIndicator",
        "2.5.4.28" => "preferredDeliveryMethod",
        "2.5.4.29" => "presentationAddress",
        "2.5.4.30" => "supportedApplicationContext",
        "2.5.4.31" => "member",
        "2.5.4.32" => "owner",
        "2.5.4.33" => "roleOccupant",
        "2.5.4.34" => "seeAlso",
        "2.5.4.35" => "userPassword",
        "2.5.4.36" => "userCertificate",
        "2.5.4.37" => "cACertificate",
        "2.5.4.38" => "authorityRevocationList",
        "2.5.4.39" => "certificateRevocationList",
        "2.5.4.40" => "crossCertificatePair",
        "2.5.4.41" => "name",
        "2.5.4.42" => "GN",
        "2.5.4.43" => "initials",
        "2.5.4.44" => "generationQualifier",
        "2.5.4.45" => "x500UniqueIdentifier",
        "2.5.4.46" => "dnQualifier",
        "2.5.4.47" => "enhancedSearchGuide",
        "2.5.4.48" => "protocolInformation",
        "2.5.4.49" => "distinguishedName",
        "2.5.4.50" => "uniqueMember",
        "2.5.4.51" => "houseIdentifier",
        "2.5.4.52" => "supportedAlgorithms",
        "2.5.4.53" => "deltaRevocationList",
        "2.5.4.54" => "dmdName",
        "2.5.4.65" => "pseudonym",
        "2.5.4.72" => "role",
        "2.5.4.97" => "organizationIdentifier",
        "2.5.4.98" => "c3",
        "2.5.4.99" => "n3",
        "2.5.4.100" => "dnsName",
        "1.2.840.113549.1.9.1" => "emailAddress",
        "1.2.840.113549.1.9.2" => "unstructuredName",
        "1.2.840.113549.1.9.3" => "contentType",
        "1.2.840.113549.1.9.7" => "challengePassword",
        "1.2.840.113549.1.9.8" => "unstructuredAddress",
        "0.9.2342.19200300.100.1.1" => "UID",
        "0.9.2342.19200300.100.1.3" => "mail",
        "0.9.2342.19200300.100.1.25" => "DC",
        "1.3.6.1.4.1.311.60.2.1.1" => "jurisdictionL",
        "1.3.6.1.4.1.311.60.2.1.2" => "jurisdictionST",
        "1.3.6.1.4.1.311.60.2.1.3" => "jurisdictionC",
        _ => return None,
    })
}

/// `i2t_ASN1_OBJECT`: the long name of an access method, or its numeric form.
pub fn access_method_name(oid: &str) -> String {
    match oid {
        "1.3.6.1.5.5.7.48.1" => "OCSP",
        "1.3.6.1.5.5.7.48.2" => "CA Issuers",
        "1.3.6.1.5.5.7.48.3" => "AD Time Stamping",
        "1.3.6.1.5.5.7.48.4" => "ad dvcs",
        "1.3.6.1.5.5.7.48.5" => "CA Repository",
        _ => return oid.to_string(),
    }
    .to_string()
}

// ---- strings ------------------------------------------------------------------------------------

/// `tag2nbyte`: bytes per character of an ASN.1 string type (0 UTF-8, -1 not a string).
pub fn char_width(tag: u8) -> i8 {
    match tag {
        UTF8_STRING => 0,
        0x12 | 0x13 | 0x14 | IA5_STRING | 0x17 | 0x18 | 0x1a => 1,
        0x1c => 4,
        0x1e => 2,
        _ => -1,
    }
}

/// The characters of a string value, each as its UTF-8 bytes (a UTF8String is one char per byte,
/// as `ASN1_STRFLGS_UTF8_CONVERT` treats it). `None` when a wide string is malformed.
pub fn string_chars(tag: u8, value: &[u8], dump_unknown: bool) -> Option<Vec<Vec<u8>>> {
    let width = match char_width(tag) {
        -1 if dump_unknown => return None,
        -1 | 0 => 1,
        w => w as usize,
    };
    if char_width(tag) == 0 {
        return Some(value.iter().map(|&b| vec![b]).collect());
    }
    if value.len() % width != 0 {
        return None;
    }
    Some(
        value
            .chunks(width)
            .map(|c| {
                let cp = c.iter().fold(0u32, |acc, &b| (acc << 8) | b as u32);
                let ch = char::from_u32(cp).unwrap_or('\u{fffd}');
                let mut buf = [0u8; 4];
                ch.encode_utf8(&mut buf).as_bytes().to_vec()
            })
            .collect(),
    )
}

/// `ASN1_STRING_to_UTF8`.
pub fn string_to_utf8(tag: u8, value: &[u8]) -> Option<String> {
    match char_width(tag) {
        -1 => None,
        0 => std::str::from_utf8(value).ok().map(str::to_string),
        _ => string_chars(tag, value, false).map(|chars| String::from_utf8_lossy(&chars.concat()).into_owned()),
    }
}

/// `do_esc_char` with `ASN1_STRFLGS_ESC_2253` (and `ESC_CTRL` when `ctrl`), MSB bytes unescaped.
pub fn escape_chars(chars: &[Vec<u8>], ctrl: bool, out: &mut Vec<u8>) {
    let n = chars.len();
    for (i, ch) in chars.iter().enumerate() {
        for &c in ch {
            if c > 0x7f {
                out.push(c);
                continue;
            }
            let first = i == 0;
            let last = i + 1 == n;
            let backslash = matches!(c, b'"' | b'+' | b',' | b';' | b'<' | b'>' | b'\\')
                || (first && (c == b' ' || c == b'#'))
                || (last && c == b' ');
            if backslash {
                out.push(b'\\');
                out.push(c);
            } else if ctrl && (c < 0x20 || c == 0x7f) {
                out.extend_from_slice(format!("\\{c:02X}").as_bytes());
            } else {
                out.push(c);
            }
        }
    }
}

// ---- names --------------------------------------------------------------------------------------

pub struct NameEntry<'a> {
    pub oid: String,
    pub value: Tlv<'a>,
}

pub fn name_rdns(name: &[u8]) -> Option<Vec<Vec<NameEntry<'_>>>> {
    let mut out = Vec::new();
    for set in children(name)? {
        if set.tag != SET {
            return None;
        }
        let mut rdn = Vec::new();
        for atv in children(set.value)? {
            let mut a = atv.value;
            let oid = expect(&mut a, OID)?;
            let value = read_tlv(&mut a)?;
            rdn.push(NameEntry { oid: oid_text(oid.value), value });
        }
        out.push(rdn);
    }
    Some(out)
}

/// `X509_NAME_print_ex` with Node's multi-line flags, or (`rfc2253`) with
/// `XN_FLAG_RFC2253 & ~ASN1_STRFLGS_ESC_MSB & ~ASN1_STRFLGS_ESC_CTRL`.
pub fn print_name(name: &[u8], rfc2253: bool) -> Option<Vec<u8>> {
    let mut rdns = name_rdns(name)?;
    if rfc2253 {
        rdns.reverse();
    }
    let (sep, sep_mv) = if rfc2253 { (&b","[..], &b"+"[..]) } else { (&b"\n"[..], &b" + "[..]) };
    let mut out = Vec::new();
    for (i, rdn) in rdns.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(sep);
        }
        for (j, entry) in rdn.iter().enumerate() {
            if j > 0 {
                out.extend_from_slice(sep_mv);
            }
            let short = attribute_short_name(&entry.oid);
            out.extend_from_slice(short.unwrap_or(&entry.oid).as_bytes());
            out.push(b'=');
            let chars = if rfc2253 && short.is_none() {
                None
            } else {
                string_chars(entry.value.tag, entry.value.value, rfc2253)
            };
            match chars {
                Some(chars) => escape_chars(&chars, !rfc2253, &mut out),
                None => {
                    out.push(b'#');
                    out.extend_from_slice(hex_upper(entry.value.raw).as_bytes());
                }
            }
        }
    }
    Some(out)
}

/// Canonical form for name comparison: attribute types with case-folded, whitespace-collapsed
/// string values.
pub fn canonical_name(name: &[u8]) -> Option<Vec<Vec<(String, Vec<u8>)>>> {
    Some(
        name_rdns(name)?
            .into_iter()
            .map(|rdn| {
                rdn.into_iter()
                    .map(|e| {
                        let value = match string_to_utf8(e.value.tag, e.value.value) {
                            Some(s) => s
                                .split_ascii_whitespace()
                                .collect::<Vec<_>>()
                                .join(" ")
                                .to_ascii_lowercase()
                                .into_bytes(),
                            None => e.value.raw.to_vec(),
                        };
                        (e.oid, value)
                    })
                    .collect()
            })
            .collect(),
    )
}

// ---- general names ------------------------------------------------------------------------------

pub fn is_safe_alt_name(name: &[u8], utf8: bool) -> bool {
    name.iter().all(|&c| match c {
        b'"' | b'\\' | b',' | b'\'' => false,
        _ if utf8 => !(c < b' ' || c == 0x7f),
        _ => (b' '..=b'~').contains(&c),
    })
}

pub fn print_alt_name(out: &mut Vec<u8>, name: &[u8], utf8: bool, safe_prefix: Option<&str>) {
    if is_safe_alt_name(name, utf8) {
        if let Some(p) = safe_prefix {
            out.extend_from_slice(p.as_bytes());
            out.push(b':');
        }
        out.extend_from_slice(name);
        return;
    }
    out.push(b'"');
    if let Some(p) = safe_prefix {
        out.extend_from_slice(p.as_bytes());
        out.push(b':');
    }
    for &c in name {
        if c == b'\\' {
            out.extend_from_slice(b"\\\\");
        } else if c == b'"' {
            out.extend_from_slice(b"\\\"");
        } else if (c >= b' ' && c != b',' && c <= b'~') || (utf8 && c & 0x80 != 0) {
            out.push(c);
        } else {
            out.extend_from_slice(format!("\\u00{c:02x}").as_bytes());
        }
    }
    out.push(b'"');
}

pub fn othername_prefix(oid: &str) -> Option<(&'static str, bool)> {
    Some(match oid {
        "1.3.6.1.5.5.7.8.9" => ("SmtpUTF8Mailbox", true),
        "1.3.6.1.5.5.7.8.5" => ("XmppAddr", true),
        "1.3.6.1.5.5.7.8.7" => ("SRVName", false),
        "1.3.6.1.4.1.311.20.2.3" => ("UPN", true),
        "1.3.6.1.5.5.7.8.8" => ("NAIRealm", true),
        _ => return None,
    })
}

/// Node's `PrintGeneralName`.
pub fn print_general_name(out: &mut Vec<u8>, gen: &Tlv) -> Option<()> {
    match gen.tag {
        0x82 => {
            out.extend_from_slice(b"DNS:");
            print_alt_name(out, gen.value, false, None);
        }
        0x81 => {
            out.extend_from_slice(b"email:");
            print_alt_name(out, gen.value, false, None);
        }
        0x86 => {
            out.extend_from_slice(b"URI:");
            print_alt_name(out, gen.value, false, None);
        }
        0xa4 => {
            out.extend_from_slice(b"DirName:");
            let mut inner = gen.value;
            let name = expect(&mut inner, SEQ)?;
            let text = print_name(name.value, true)?;
            print_alt_name(out, &text, true, None);
        }
        0x87 => {
            out.extend_from_slice(b"IP Address:");
            let b = gen.value;
            if b.len() == 4 {
                out.extend_from_slice(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]).as_bytes());
            } else if b.len() == 16 {
                let groups: Vec<String> =
                    b.chunks(2).map(|p| format!("{:X}", (p[0] as u16) << 8 | p[1] as u16)).collect();
                out.extend_from_slice(groups.join(":").as_bytes());
            } else {
                out.extend_from_slice(format!("<invalid length={}>", b.len()).as_bytes());
            }
        }
        0x88 => {
            out.extend_from_slice(b"Registered ID:");
            out.extend_from_slice(oid_text(gen.value).as_bytes());
        }
        0xa0 => {
            let mut v = gen.value;
            let oid = expect(&mut v, OID)?;
            let explicit = expect(&mut v, 0xa0)?;
            let mut iv = explicit.value;
            let value = read_tlv(&mut iv)?;
            match othername_prefix(&oid_text(oid.value)) {
                Some((prefix, unicode))
                    if (unicode && value.tag == UTF8_STRING) || (!unicode && value.tag == IA5_STRING) =>
                {
                    out.extend_from_slice(b"othername:");
                    print_alt_name(out, value.value, unicode, Some(prefix));
                }
                _ => out.extend_from_slice(b"othername:<unsupported>"),
            }
        }
        0xa3 => out.extend_from_slice(b"X400Name:<unsupported>"),
        0xa5 => out.extend_from_slice(b"EdiPartyName:<unsupported>"),
        _ => return None,
    }
    Some(())
}

pub fn general_names(ext: &[u8]) -> Option<Vec<Tlv<'_>>> {
    let mut input = ext;
    let seq = expect(&mut input, SEQ)?;
    children(seq.value)
}

pub fn san_text(ext: &[u8]) -> Option<String> {
    let mut out = Vec::new();
    for (i, gen) in general_names(ext)?.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b", ");
        }
        print_general_name(&mut out, gen)?;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

pub fn info_access_text(ext: &[u8]) -> Option<String> {
    let mut input = ext;
    let seq = expect(&mut input, SEQ)?;
    let mut out = Vec::new();
    for (i, desc) in children(seq.value)?.iter().enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        let mut d = desc.value;
        let method = expect(&mut d, OID)?;
        let location = read_tlv(&mut d)?;
        out.extend_from_slice(access_method_name(&oid_text(method.value)).as_bytes());
        out.extend_from_slice(b" - ");
        print_general_name(&mut out, &location)?;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

// ---- time ---------------------------------------------------------------------------------------

/// `ASN1_TIME_print`: `Sep  3 21:40:37 2022 GMT`.
pub fn print_time(t: &Tlv) -> Option<String> {
    let s = std::str::from_utf8(t.value).ok()?;
    let (year, rest) = match t.tag {
        0x17 => {
            let yy: u32 = s.get(0..2)?.parse().ok()?;
            (if yy < 50 { 2000 + yy } else { 1900 + yy }, s.get(2..)?)
        }
        0x18 => (s.get(0..4)?.parse().ok()?, s.get(4..)?),
        _ => return None,
    };
    let num = |r: std::ops::Range<usize>| -> Option<u32> {
        let part = rest.get(r)?;
        if part.bytes().all(|b| b.is_ascii_digit()) { part.parse().ok() } else { None }
    };
    let month = num(0..2)?;
    let day = num(2..4)?;
    let hour = num(4..6)?;
    let minute = num(6..8)?;
    let mut tail = rest.get(8..)?;
    let second = if tail.len() >= 2 && tail.as_bytes()[..2].iter().all(u8::is_ascii_digit) {
        let v = tail[..2].parse().ok()?;
        tail = &tail[2..];
        v
    } else {
        0
    };
    let mut frac = "";
    if t.tag == 0x18 && tail.starts_with('.') {
        let digits = tail[1..].bytes().take_while(u8::is_ascii_digit).count();
        frac = &tail[..1 + digits];
        tail = &tail[1 + digits..];
    }
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let name = MONTHS.get((month as usize).checked_sub(1)?)?;
    let gmt = if tail == "Z" { " GMT" } else { "" };
    Some(format!("{name} {day:2} {hour:02}:{minute:02}:{second:02}{frac} {year}{gmt}"))
}

// ---- PEM ----------------------------------------------------------------------------------------

pub fn pem_encode(label: &str, der: &[u8]) -> String {
    let b64 = codec::base64_encode(der, false, true);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

pub enum PemError {
    NoStartLine,
    BadBase64,
}

/// The body of the first PEM block whose label is one of `labels`.
pub fn pem_find(input: &[u8], labels: &[&str]) -> Result<Vec<u8>, PemError> {
    let text = String::from_utf8_lossy(input);
    let mut pos = 0;
    while let Some(at) = text[pos..].find("-----BEGIN ") {
        let start = pos + at + "-----BEGIN ".len();
        let Some(end_label) = text[start..].find("-----") else { break };
        let label = &text[start..start + end_label];
        let body_start = start + end_label + 5;
        pos = body_start;
        if !labels.contains(&label) {
            continue;
        }
        let end_marker = format!("-----END {label}-----");
        let Some(end) = text[body_start..].find(&end_marker) else { return Err(PemError::NoStartLine) };
        let body = &text[body_start..body_start + end];
        let lines: String = body
            .lines()
            .filter(|l| !l.contains(':'))
            .flat_map(|l| l.chars().filter(|c| !c.is_whitespace()))
            .collect();
        return codec::base64_decode_strict(lines.as_bytes(), false, Padding::Required).map_err(|_| PemError::BadBase64);
    }
    Err(PemError::NoStartLine)
}


// ---- decoded certificate ------------------------------------------------------------------------

/// A name as the ordered relative distinguished names of `(attribute, value)` pairs.
pub type NameTuples = Vec<Vec<(String, String)>>;

/// One `subjectAltName` entry.
#[derive(Debug, Clone, PartialEq)]
pub enum AltName {
    Text(String, String),
    DirName(NameTuples),
}

/// The fields `ssl.getpeercert()` reports for a certificate.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CertInfo {
    pub subject: NameTuples,
    pub issuer: NameTuples,
    pub version: u32,
    pub serial_number: String,
    pub not_before: String,
    pub not_after: String,
    pub subject_alt_name: Vec<AltName>,
    pub ocsp: Vec<String>,
    pub ca_issuers: Vec<String>,
    pub crl_distribution_points: Vec<String>,
}

const OID_CRL_DP: &str = "2.5.29.31";
const OID_AD_OCSP: &str = "1.3.6.1.5.5.7.48.1";
const OID_AD_CA_ISSUERS: &str = "1.3.6.1.5.5.7.48.2";

/// The labels of PEM blocks that hold a certificate.
pub const CERTIFICATE_LABELS: &[&str] = &["CERTIFICATE", "X509 CERTIFICATE", "TRUSTED CERTIFICATE"];

/// OpenSSL's long name of an attribute type (`commonName`), the dotted form when it has none:
/// what `OBJ_obj2txt(.., no_name = 0)` prints.
pub fn attribute_long_name(oid: &str) -> String {
    let long = match oid {
        "2.5.4.3" => "commonName",
        "2.5.4.4" => "surname",
        "2.5.4.6" => "countryName",
        "2.5.4.7" => "localityName",
        "2.5.4.8" => "stateOrProvinceName",
        "2.5.4.9" => "streetAddress",
        "2.5.4.10" => "organizationName",
        "2.5.4.11" => "organizationalUnitName",
        "2.5.4.42" => "givenName",
        "2.5.4.98" => "countryCode3c",
        "2.5.4.99" => "countryCode3n",
        "0.9.2342.19200300.100.1.1" => "userId",
        "0.9.2342.19200300.100.1.3" => "rfc822Mailbox",
        "0.9.2342.19200300.100.1.25" => "domainComponent",
        "1.3.6.1.4.1.311.60.2.1.1" => "jurisdictionLocalityName",
        "1.3.6.1.4.1.311.60.2.1.2" => "jurisdictionStateOrProvinceName",
        "1.3.6.1.4.1.311.60.2.1.3" => "jurisdictionCountryName",
        other => return attribute_short_name(other).map_or_else(|| other.to_string(), str::to_string),
    };
    long.to_string()
}

/// A name's RDNs as `(long attribute name, UTF-8 value)` pairs (`_create_tuple_for_X509_NAME`).
pub fn name_tuples(name: &[u8]) -> Option<NameTuples> {
    let mut out = Vec::new();
    for rdn in name_rdns(name)? {
        let mut entries = Vec::new();
        for entry in rdn {
            let value = string_to_utf8(entry.value.tag, entry.value.value)?;
            entries.push((attribute_long_name(&entry.oid), value));
        }
        out.push(entries);
    }
    Some(out)
}

/// The hex form `i2a_ASN1_INTEGER` prints for a serial number (without the sign padding byte).
pub fn serial_hex(content: &[u8]) -> String {
    let mut magnitude = content;
    while magnitude.len() > 1 && magnitude[0] == 0 {
        magnitude = &magnitude[1..];
    }
    if magnitude.is_empty() {
        return "00".to_string();
    }
    hex_upper(magnitude)
}

fn ip_text(bytes: &[u8]) -> String {
    match bytes.len() {
        4 => format!("{}.{}.{}.{}", bytes[0], bytes[1], bytes[2], bytes[3]),
        16 => bytes
            .chunks(2)
            .map(|pair| format!("{:X}", (pair[0] as u16) << 8 | pair[1] as u16))
            .collect::<Vec<_>>()
            .join(":"),
        _ => "<invalid>".to_string(),
    }
}

fn alt_names(ext: &[u8]) -> Option<Vec<AltName>> {
    let mut out = Vec::new();
    for name in general_names(ext)? {
        let text = |label: &str| AltName::Text(label.to_string(), String::from_utf8_lossy(name.value).into_owned());
        out.push(match name.tag {
            0x82 => text("DNS"),
            0x81 => text("email"),
            0x86 => text("URI"),
            0x87 => AltName::Text("IP Address".to_string(), ip_text(name.value)),
            0x88 => AltName::Text("Registered ID".to_string(), oid_text(name.value)),
            0xa4 => {
                let mut inner = name.value;
                let seq = expect(&mut inner, SEQ)?;
                AltName::DirName(name_tuples(seq.value)?)
            }
            0xa0 => AltName::Text("othername".to_string(), "<unsupported>".to_string()),
            0xa3 => AltName::Text("X400Name".to_string(), "<unsupported>".to_string()),
            0xa5 => AltName::Text("EdiPartyName".to_string(), "<unsupported>".to_string()),
            _ => return None,
        });
    }
    Some(out)
}

/// The URIs of the `AuthorityInfoAccess` entries with access method `method`.
fn access_uris(ext: &[u8], method: &str) -> Option<Vec<String>> {
    let mut input = ext;
    let seq = expect(&mut input, SEQ)?;
    let mut out = Vec::new();
    for desc in children(seq.value)? {
        let mut d = desc.value;
        let oid = expect(&mut d, OID)?;
        let location = read_tlv(&mut d)?;
        if oid_text(oid.value) == method && location.tag == 0x86 {
            out.push(String::from_utf8_lossy(location.value).into_owned());
        }
    }
    Some(out)
}

/// The URIs of the full names of the `CRLDistributionPoints` extension.
fn crl_uris(ext: &[u8]) -> Option<Vec<String>> {
    let mut input = ext;
    let seq = expect(&mut input, SEQ)?;
    let mut out = Vec::new();
    for point in children(seq.value)? {
        let mut parts = children(point.value)?.into_iter();
        let Some(first) = parts.next() else { continue };
        if first.tag != 0xa0 {
            continue;
        }
        let mut inner = first.value;
        let Some(full) = read_tlv(&mut inner) else { continue };
        if full.tag != 0xa0 {
            continue;
        }
        for name in children(full.value)? {
            if name.tag == 0x86 {
                out.push(String::from_utf8_lossy(name.value).into_owned());
            }
        }
    }
    Some(out)
}

/// Decodes a DER certificate into the fields Python reports; `None` when it is malformed.
pub fn decode_cert(der: &[u8]) -> Option<CertInfo> {
    let cert = parse_cert(der)?;
    let mut info = CertInfo {
        subject: name_tuples(cert.subject.value)?,
        issuer: name_tuples(cert.issuer.value)?,
        version: cert.version + 1,
        serial_number: serial_hex(cert.serial),
        not_before: print_time(&cert.not_before)?,
        not_after: print_time(&cert.not_after)?,
        ..Default::default()
    };
    if let Some(ext) = cert.ext(OID_SAN) {
        info.subject_alt_name = alt_names(ext)?;
    }
    if let Some(ext) = cert.ext(OID_AIA) {
        info.ocsp = access_uris(ext, OID_AD_OCSP)?;
        info.ca_issuers = access_uris(ext, OID_AD_CA_ISSUERS)?;
    }
    if let Some(ext) = cert.ext(OID_CRL_DP) {
        info.crl_distribution_points = crl_uris(ext)?;
    }
    Some(info)
}
