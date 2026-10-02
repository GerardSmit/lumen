//! X.509 certificates (`crypto.X509Certificate`, the legacy peer-certificate object) and SPKAC
//! (`crypto.Certificate`). The DER walk is a small lenient TLV reader so that certificates OpenSSL
//! accepts (odd string types, invalid names inside extensions) still parse; the text formats mirror
//! OpenSSL's `X509_NAME_print_ex`, `ASN1_TIME_print` and Node's `PrintGeneralName`. Signatures and
//! key checks use the RustCrypto crates.

use lumen_common::codec::{self, Padding};
use lumen::embed::OpError;

use crate::hash::{self, Algo};


#[lumen_bind::module(name = "crypto")]
pub(crate) mod bindings {
use super::*;

#[derive(Clone, Copy)]
struct Tlv<'a> {
    tag: u8,
    value: &'a [u8],
    raw: &'a [u8],
}

fn read_tlv<'a>(input: &mut &'a [u8]) -> Option<Tlv<'a>> {
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

fn expect<'a>(input: &mut &'a [u8], tag: u8) -> Option<Tlv<'a>> {
    let t = read_tlv(input)?;
    (t.tag == tag).then_some(t)
}

fn children(value: &[u8]) -> Option<Vec<Tlv<'_>>> {
    let mut rest = value;
    let mut out = Vec::new();
    while !rest.is_empty() {
        out.push(read_tlv(&mut rest)?);
    }
    Some(out)
}

const SEQ: u8 = 0x30;
const SET: u8 = 0x31;
const OID: u8 = 0x06;
const INTEGER: u8 = 0x02;
const BIT_STRING: u8 = 0x03;
const OCTET_STRING: u8 = 0x04;
const BOOLEAN: u8 = 0x01;
const UTF8_STRING: u8 = 0x0c;
const IA5_STRING: u8 = 0x16;

fn oid_text(content: &[u8]) -> String {
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

struct Extension<'a> {
    oid: String,
    value: &'a [u8],
}

struct Cert<'a> {
    raw: &'a [u8],
    tbs: &'a [u8],
    serial: &'a [u8],
    issuer: Tlv<'a>,
    not_before: Tlv<'a>,
    not_after: Tlv<'a>,
    subject: Tlv<'a>,
    spki: Tlv<'a>,
    extensions: Vec<Extension<'a>>,
    sig_alg: Tlv<'a>,
    signature: &'a [u8],
}

fn parse_cert(der: &[u8]) -> Option<Cert<'_>> {
    let mut input = der;
    let cert = expect(&mut input, SEQ)?;
    let mut body = cert.value;
    let tbs = expect(&mut body, SEQ)?;
    let sig_alg = expect(&mut body, SEQ)?;
    let sig = expect(&mut body, BIT_STRING)?;
    let signature = sig.value.get(1..)?;

    let mut t = tbs.value;
    let mut next = read_tlv(&mut t)?;
    if next.tag == 0xa0 {
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
    fn ext(&self, oid: &str) -> Option<&'a [u8]> {
        self.extensions.iter().find(|e| e.oid == oid).map(|e| e.value)
    }
}

const OID_SAN: &str = "2.5.29.17";
const OID_AIA: &str = "1.3.6.1.5.5.7.1.1";
const OID_EKU: &str = "2.5.29.37";
const OID_BASIC_CONSTRAINTS: &str = "2.5.29.19";
const OID_KEY_USAGE: &str = "2.5.29.15";
const OID_SKI: &str = "2.5.29.14";
const OID_AKI: &str = "2.5.29.35";

const OID_RSA: &str = "1.2.840.113549.1.1.1";
const OID_RSA_PSS: &str = "1.2.840.113549.1.1.10";
const OID_EC: &str = "1.2.840.10045.2.1";
const OID_ED25519: &str = "1.3.101.112";
const OID_X25519: &str = "1.3.101.110";
const OID_ED448: &str = "1.3.101.113";
const OID_X448: &str = "1.3.101.111";
const OID_DSA: &str = "1.2.840.10040.4.1";

/// OpenSSL's short names of the attribute types it knows.
fn attribute_short_name(oid: &str) -> Option<&'static str> {
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
fn access_method_name(oid: &str) -> String {
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
fn char_width(tag: u8) -> i8 {
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
fn string_chars(tag: u8, value: &[u8], dump_unknown: bool) -> Option<Vec<Vec<u8>>> {
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
fn string_to_utf8(tag: u8, value: &[u8]) -> Option<String> {
    match char_width(tag) {
        -1 => None,
        0 => std::str::from_utf8(value).ok().map(str::to_string),
        _ => string_chars(tag, value, false).map(|chars| String::from_utf8_lossy(&chars.concat()).into_owned()),
    }
}

/// `do_esc_char` with `ASN1_STRFLGS_ESC_2253` (and `ESC_CTRL` when `ctrl`), MSB bytes unescaped.
fn escape_chars(chars: &[Vec<u8>], ctrl: bool, out: &mut Vec<u8>) {
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

use codec::hex_encode_upper as hex_upper;

// ---- names --------------------------------------------------------------------------------------

struct NameEntry<'a> {
    oid: String,
    value: Tlv<'a>,
}

fn name_rdns(name: &[u8]) -> Option<Vec<Vec<NameEntry<'_>>>> {
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
fn print_name(name: &[u8], rfc2253: bool) -> Option<Vec<u8>> {
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
fn canonical_name(name: &[u8]) -> Option<Vec<Vec<(String, Vec<u8>)>>> {
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

fn is_safe_alt_name(name: &[u8], utf8: bool) -> bool {
    name.iter().all(|&c| match c {
        b'"' | b'\\' | b',' | b'\'' => false,
        _ if utf8 => !(c < b' ' || c == 0x7f),
        _ => (b' '..=b'~').contains(&c),
    })
}

fn print_alt_name(out: &mut Vec<u8>, name: &[u8], utf8: bool, safe_prefix: Option<&str>) {
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

fn othername_prefix(oid: &str) -> Option<(&'static str, bool)> {
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
fn print_general_name(out: &mut Vec<u8>, gen: &Tlv) -> Option<()> {
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

fn general_names(ext: &[u8]) -> Option<Vec<Tlv<'_>>> {
    let mut input = ext;
    let seq = expect(&mut input, SEQ)?;
    children(seq.value)
}

fn san_text(ext: &[u8]) -> Option<String> {
    let mut out = Vec::new();
    for (i, gen) in general_names(ext)?.iter().enumerate() {
        if i > 0 {
            out.extend_from_slice(b", ");
        }
        print_general_name(&mut out, gen)?;
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

fn info_access_text(ext: &[u8]) -> Option<String> {
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
fn print_time(t: &Tlv) -> Option<String> {
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

// ---- keys ---------------------------------------------------------------------------------------

struct Spki<'a> {
    alg: String,
    params: Option<Tlv<'a>>,
    key: &'a [u8],
}

fn parse_spki(raw: &[u8]) -> Option<Spki<'_>> {
    let mut input = raw;
    let seq = expect(&mut input, SEQ)?;
    let mut s = seq.value;
    let alg = expect(&mut s, SEQ)?;
    let bits = expect(&mut s, BIT_STRING)?;
    let mut a = alg.value;
    let oid = expect(&mut a, OID)?;
    let params = read_tlv(&mut a);
    Some(Spki { alg: oid_text(oid.value), params, key: bits.value.get(1..)? })
}

#[derive(Clone, Copy, PartialEq)]
enum Curve {
    P256,
    P384,
    P521,
    K256,
}

fn curve_of(params: Option<Tlv>) -> Option<Curve> {
    let p = params?;
    if p.tag != OID {
        return None;
    }
    Some(match oid_text(p.value).as_str() {
        "1.2.840.10045.3.1.7" => Curve::P256,
        "1.3.132.0.34" => Curve::P384,
        "1.3.132.0.35" => Curve::P521,
        "1.3.132.0.10" => Curve::K256,
        _ => return None,
    })
}

/// The uncompressed SEC1 encoding of a point on `curve`, or `None` when it is not one.
fn normalize_point(curve: Curve, point: &[u8]) -> Option<Vec<u8>> {
    use elliptic_curve::sec1::ToEncodedPoint;
    Some(match curve {
        Curve::P256 => p256::PublicKey::from_sec1_bytes(point).ok()?.to_encoded_point(false).as_bytes().to_vec(),
        Curve::P384 => p384::PublicKey::from_sec1_bytes(point).ok()?.to_encoded_point(false).as_bytes().to_vec(),
        Curve::P521 => p521::PublicKey::from_sec1_bytes(point).ok()?.to_encoded_point(false).as_bytes().to_vec(),
        Curve::K256 => k256::PublicKey::from_sec1_bytes(point).ok()?.to_encoded_point(false).as_bytes().to_vec(),
    })
}

fn rsa_public(key: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    use pkcs1::der::Decode;
    let k = pkcs1::RsaPublicKey::from_der(key).ok()?;
    Some((k.modulus.as_bytes().to_vec(), k.public_exponent.as_bytes().to_vec()))
}

fn strip_zeros(b: &[u8]) -> &[u8] {
    let i = b.iter().position(|&x| x != 0).unwrap_or(b.len());
    &b[i..]
}

/// Whether OpenSSL could build an EVP_PKEY from this SPKI.
fn spki_is_valid(spki: &Spki) -> bool {
    match spki.alg.as_str() {
        OID_RSA | OID_RSA_PSS => rsa_public(spki.key).is_some(),
        OID_EC => match curve_of(spki.params) {
            Some(curve) => normalize_point(curve, spki.key).is_some(),
            None => true,
        },
        OID_ED25519 | OID_X25519 => spki.key.len() == 32,
        OID_ED448 => spki.key.len() == 57,
        OID_X448 => spki.key.len() == 56,
        _ => true,
    }
}

/// The public half of a PKCS#8 private key as (algorithm OID, comparable public key bytes).
fn private_to_public(pkcs8: &[u8]) -> Option<(String, Vec<u8>)> {
    let mut input = pkcs8;
    let seq = expect(&mut input, SEQ)?;
    let mut s = seq.value;
    expect(&mut s, INTEGER)?;
    let alg = expect(&mut s, SEQ)?;
    let key = expect(&mut s, OCTET_STRING)?;
    let mut a = alg.value;
    let oid = oid_text(expect(&mut a, OID)?.value);
    let params = read_tlv(&mut a);
    let public = match oid.as_str() {
        OID_RSA | OID_RSA_PSS => {
            use pkcs1::der::Decode;
            let k = pkcs1::RsaPrivateKey::from_der(key.value).ok()?;
            let mut v = strip_zeros(k.modulus.as_bytes()).to_vec();
            v.push(0xff);
            v.extend_from_slice(strip_zeros(k.public_exponent.as_bytes()));
            v
        }
        OID_EC => {
            use sec1::der::Decode;
            let k = sec1::EcPrivateKey::from_der(key.value).ok()?;
            let curve = curve_of(params).or_else(|| {
                let oid = k.parameters?.named_curve()?;
                curve_of_oid(&oid.to_string())
            })?;
            ec_public_from_secret(curve, k.private_key)?
        }
        OID_ED25519 => {
            let mut inner = key.value;
            let secret: [u8; 32] = expect(&mut inner, OCTET_STRING)?.value.try_into().ok()?;
            ed25519_dalek::SigningKey::from_bytes(&secret).verifying_key().to_bytes().to_vec()
        }
        OID_X25519 => {
            let mut inner = key.value;
            let secret: [u8; 32] = expect(&mut inner, OCTET_STRING)?.value.try_into().ok()?;
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(secret)).as_bytes().to_vec()
        }
        _ => return None,
    };
    Some((oid, public))
}

fn curve_of_oid(oid: &str) -> Option<Curve> {
    Some(match oid {
        "1.2.840.10045.3.1.7" => Curve::P256,
        "1.3.132.0.34" => Curve::P384,
        "1.3.132.0.35" => Curve::P521,
        "1.3.132.0.10" => Curve::K256,
        _ => return None,
    })
}

fn ec_public_from_secret(curve: Curve, scalar: &[u8]) -> Option<Vec<u8>> {
    use elliptic_curve::sec1::ToEncodedPoint;
    Some(match curve {
        Curve::P256 => p256::SecretKey::from_slice(scalar).ok()?.public_key().to_encoded_point(false).as_bytes().to_vec(),
        Curve::P384 => p384::SecretKey::from_slice(scalar).ok()?.public_key().to_encoded_point(false).as_bytes().to_vec(),
        Curve::P521 => p521::SecretKey::from_slice(scalar).ok()?.public_key().to_encoded_point(false).as_bytes().to_vec(),
        Curve::K256 => k256::SecretKey::from_slice(scalar).ok()?.public_key().to_encoded_point(false).as_bytes().to_vec(),
    })
}

/// The SPKI's public key in the comparable form `private_to_public` produces.
fn spki_public(spki: &Spki) -> Option<Vec<u8>> {
    Some(match spki.alg.as_str() {
        OID_RSA | OID_RSA_PSS => {
            let (n, e) = rsa_public(spki.key)?;
            let mut v = strip_zeros(&n).to_vec();
            v.push(0xff);
            v.extend_from_slice(strip_zeros(&e));
            v
        }
        OID_EC => normalize_point(curve_of(spki.params)?, spki.key)?,
        _ => spki.key.to_vec(),
    })
}

// ---- signatures ---------------------------------------------------------------------------------

fn digest_info_prefix(algo: Algo) -> Option<&'static [u8]> {
    Some(match algo {
        Algo::Md5 => &[0x30, 0x20, 0x30, 0x0c, 0x06, 0x08, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x05, 0x05, 0x00, 0x04, 0x10],
        Algo::Sha1 => &[0x30, 0x21, 0x30, 0x09, 0x06, 0x05, 0x2b, 0x0e, 0x03, 0x02, 0x1a, 0x05, 0x00, 0x04, 0x14],
        Algo::Sha224 => &[0x30, 0x2d, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x04, 0x05, 0x00, 0x04, 0x1c],
        Algo::Sha256 => &[0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05, 0x00, 0x04, 0x20],
        Algo::Sha384 => &[0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05, 0x00, 0x04, 0x30],
        Algo::Sha512 => &[0x30, 0x51, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x03, 0x05, 0x00, 0x04, 0x40],
        _ => return None,
    })
}

fn hash_of_oid(oid: &str) -> Option<Algo> {
    Some(match oid {
        "1.2.840.113549.2.5" => Algo::Md5,
        "1.3.14.3.2.26" => Algo::Sha1,
        "2.16.840.1.101.3.4.2.4" => Algo::Sha224,
        "2.16.840.1.101.3.4.2.1" => Algo::Sha256,
        "2.16.840.1.101.3.4.2.2" => Algo::Sha384,
        "2.16.840.1.101.3.4.2.3" => Algo::Sha512,
        _ => return None,
    })
}

enum SigScheme {
    Pkcs1(Algo),
    Pss(Algo, usize),
    Ecdsa(Algo),
    Ed25519,
}

/// RSASSA-PSS-params: hash (default SHA-1), MGF1 over the same hash, salt length (default 20).
fn pss_params(params: Option<Tlv>) -> Option<(Algo, usize)> {
    let mut hash = Algo::Sha1;
    let mut mgf_hash = Algo::Sha1;
    let mut salt = 20usize;
    if let Some(p) = params.filter(|p| p.tag == SEQ) {
        for field in children(p.value)? {
            let mut inner = field.value;
            match field.tag {
                0xa0 => {
                    let mut a = expect(&mut inner, SEQ)?.value;
                    hash = hash_of_oid(&oid_text(expect(&mut a, OID)?.value))?;
                }
                0xa1 => {
                    let mut a = expect(&mut inner, SEQ)?.value;
                    if oid_text(expect(&mut a, OID)?.value) != "1.2.840.113549.1.1.8" {
                        return None;
                    }
                    let mut h = expect(&mut a, SEQ)?.value;
                    mgf_hash = hash_of_oid(&oid_text(expect(&mut h, OID)?.value))?;
                }
                0xa2 => {
                    let v = expect(&mut inner, INTEGER)?.value;
                    salt = v.iter().fold(0usize, |acc, &b| acc.saturating_mul(256).saturating_add(b as usize));
                }
                _ => {}
            }
        }
    }
    (hash == mgf_hash).then_some((hash, salt))
}

fn sig_scheme(alg: &Tlv) -> Option<SigScheme> {
    let mut a = alg.value;
    let oid = oid_text(expect(&mut a, OID)?.value);
    let params = read_tlv(&mut a);
    Some(match oid.as_str() {
        "1.2.840.113549.1.1.4" => SigScheme::Pkcs1(Algo::Md5),
        "1.2.840.113549.1.1.5" | "1.3.14.3.2.29" => SigScheme::Pkcs1(Algo::Sha1),
        "1.2.840.113549.1.1.14" => SigScheme::Pkcs1(Algo::Sha224),
        "1.2.840.113549.1.1.11" => SigScheme::Pkcs1(Algo::Sha256),
        "1.2.840.113549.1.1.12" => SigScheme::Pkcs1(Algo::Sha384),
        "1.2.840.113549.1.1.13" => SigScheme::Pkcs1(Algo::Sha512),
        OID_RSA_PSS => {
            let (h, salt) = pss_params(params)?;
            SigScheme::Pss(h, salt)
        }
        "1.2.840.10045.4.1" => SigScheme::Ecdsa(Algo::Sha1),
        "1.2.840.10045.4.3.1" => SigScheme::Ecdsa(Algo::Sha224),
        "1.2.840.10045.4.3.2" => SigScheme::Ecdsa(Algo::Sha256),
        "1.2.840.10045.4.3.3" => SigScheme::Ecdsa(Algo::Sha384),
        "1.2.840.10045.4.3.4" => SigScheme::Ecdsa(Algo::Sha512),
        OID_ED25519 => SigScheme::Ed25519,
        _ => return None,
    })
}

fn ecdsa_verify<C>(point: &[u8], hash: &[u8], sig: &[u8]) -> bool
where
    C: elliptic_curve::PrimeCurve + elliptic_curve::CurveArithmetic,
    ecdsa::der::MaxSize<C>: elliptic_curve::generic_array::ArrayLength<u8>,
    <elliptic_curve::FieldBytesSize<C> as std::ops::Add>::Output:
        std::ops::Add<ecdsa::der::MaxOverhead> + elliptic_curve::generic_array::ArrayLength<u8>,
    elliptic_curve::AffinePoint<C>: ecdsa::hazmat::VerifyPrimitive<C>
        + elliptic_curve::sec1::FromEncodedPoint<C>
        + elliptic_curve::sec1::ToEncodedPoint<C>,
    elliptic_curve::FieldBytesSize<C>: elliptic_curve::sec1::ModulusSize,
{
    use signature::hazmat::PrehashVerifier;
    let Ok(vk) = ecdsa::VerifyingKey::<C>::from_sec1_bytes(point) else { return false };
    let Ok(sig) = ecdsa::Signature::<C>::from_der(sig) else { return false };
    vk.verify_prehash(hash, &sig).is_ok()
}

/// Verify `signature` over `data` with the SPKI's key under the AlgorithmIdentifier `alg`.
fn verify_signature(alg: &Tlv, spki_raw: &[u8], data: &[u8], signature: &[u8]) -> bool {
    let (Some(scheme), Some(spki)) = (sig_scheme(alg), parse_spki(spki_raw)) else { return false };
    match scheme {
        SigScheme::Pkcs1(h) | SigScheme::Pss(h, _) => {
            let rsa_ok = matches!(scheme, SigScheme::Pkcs1(_)) && spki.alg == OID_RSA
                || matches!(scheme, SigScheme::Pss(..)) && (spki.alg == OID_RSA || spki.alg == OID_RSA_PSS);
            if !rsa_ok {
                return false;
            }
            let Some((n, e)) = rsa_public(spki.key) else { return false };
            match scheme {
                SigScheme::Pkcs1(_) => {
                    let key = rsa::RsaPublicKey::new_unchecked(rsa::BigUint::from_bytes_be(&n), rsa::BigUint::from_bytes_be(&e));
                    let Some(prefix) = digest_info_prefix(h) else { return false };
                    let padding = rsa::Pkcs1v15Sign { hash_len: Some(h.out_len()), prefix: prefix.into() };
                    key.verify(padding, &hash::digest(h, data), signature).is_ok()
                }
                SigScheme::Pss(_, salt) => crate::crypto::sign::bindings::rsa_pss_verify(&n, &e, h, salt, data, signature),
                _ => false,
            }
        }
        SigScheme::Ecdsa(h) => {
            if spki.alg != OID_EC {
                return false;
            }
            let hashed = hash::digest(h, data);
            match curve_of(spki.params) {
                Some(Curve::P256) => ecdsa_verify::<p256::NistP256>(spki.key, &hashed, signature),
                Some(Curve::P384) => ecdsa_verify::<p384::NistP384>(spki.key, &hashed, signature),
                Some(Curve::P521) => ecdsa_verify::<p521::NistP521>(spki.key, &hashed, signature),
                Some(Curve::K256) => ecdsa_verify::<k256::Secp256k1>(spki.key, &hashed, signature),
                None => false,
            }
        }
        SigScheme::Ed25519 => {
            if spki.alg != OID_ED25519 {
                return false;
            }
            let (Ok(key), Ok(sig)) = (
                <[u8; 32]>::try_from(spki.key),
                ed25519_dalek::Signature::from_slice(signature),
            ) else {
                return false;
            };
            ed25519_dalek::VerifyingKey::from_bytes(&key).is_ok_and(|vk| vk.verify_strict(data, &sig).is_ok())
        }
    }
}

// ---- extensions ---------------------------------------------------------------------------------

/// keyUsage bits, `None` when the extension is absent.
fn key_usage(cert: &Cert) -> Option<u16> {
    let ext = cert.ext(OID_KEY_USAGE)?;
    let mut input = ext;
    let bits = expect(&mut input, BIT_STRING)?.value;
    let b0 = *bits.get(1).unwrap_or(&0) as u16;
    let b1 = *bits.get(2).unwrap_or(&0) as u16;
    Some(b0 | (b1 << 8))
}

const KU_KEY_CERT_SIGN: u16 = 0x04;

/// `X509_check_ca(cert) == 1`.
fn is_ca(cert: &Cert) -> bool {
    if key_usage(cert).is_some_and(|ku| ku & KU_KEY_CERT_SIGN == 0) {
        return false;
    }
    let Some(ext) = cert.ext(OID_BASIC_CONSTRAINTS) else { return false };
    let mut input = ext;
    let Some(seq) = expect(&mut input, SEQ) else { return false };
    let mut s = seq.value;
    matches!(read_tlv(&mut s), Some(t) if t.tag == BOOLEAN && t.value.first().is_some_and(|&b| b != 0))
}

fn subject_key_id<'a>(cert: &Cert<'a>) -> Option<&'a [u8]> {
    let mut input = cert.ext(OID_SKI)?;
    Some(expect(&mut input, OCTET_STRING)?.value)
}

/// `X509_check_issued(issuer, subject) == X509_V_OK`.
fn check_issued(issuer: &Cert, subject: &Cert) -> bool {
    match (canonical_name(issuer.subject.value), canonical_name(subject.issuer.value)) {
        (Some(a), Some(b)) if a == b => {}
        _ => return false,
    }
    if let Some(aki) = subject.ext(OID_AKI) {
        let mut input = aki;
        let Some(seq) = expect(&mut input, SEQ) else { return false };
        let Some(fields) = children(seq.value) else { return false };
        for field in fields {
            match field.tag {
                0x80 => {
                    if subject_key_id(issuer).is_some_and(|ski| ski != field.value) {
                        return false;
                    }
                }
                0x82 => {
                    if strip_zeros(field.value) != strip_zeros(issuer.serial) {
                        return false;
                    }
                }
                _ => {}
            }
        }
    }
    !key_usage(issuer).is_some_and(|ku| ku & KU_KEY_CERT_SIGN == 0)
}

// ---- host / email / IP checks -------------------------------------------------------------------

const FLAG_ALWAYS_CHECK_SUBJECT: u32 = 1;
const FLAG_NO_WILDCARDS: u32 = 2;
const FLAG_NO_PARTIAL_WILDCARDS: u32 = 4;
const FLAG_MULTI_LABEL_WILDCARDS: u32 = 8;
const FLAG_SINGLE_LABEL_SUBDOMAINS: u32 = 16;
const FLAG_NEVER_CHECK_SUBJECT: u32 = 32;
const FLAG_DOT_SUBDOMAINS: u32 = 0x8000;

fn skip_prefix<'p>(pattern: &'p [u8], subject_len: usize, flags: u32) -> &'p [u8] {
    if flags & FLAG_DOT_SUBDOMAINS == 0 {
        return pattern;
    }
    let mut p = pattern;
    while p.len() > subject_len && p[0] != 0 {
        if flags & FLAG_SINGLE_LABEL_SUBDOMAINS != 0 && p[0] == b'.' {
            break;
        }
        p = &p[1..];
    }
    if p.len() == subject_len { p } else { pattern }
}

fn equal_nocase(pattern: &[u8], subject: &[u8], flags: u32) -> bool {
    let pattern = skip_prefix(pattern, subject.len(), flags);
    pattern.len() == subject.len()
        && pattern.iter().zip(subject).all(|(&l, &r)| l != 0 && l.to_ascii_lowercase() == r.to_ascii_lowercase())
}

fn equal_case(pattern: &[u8], subject: &[u8], flags: u32) -> bool {
    let pattern = skip_prefix(pattern, subject.len(), flags);
    pattern == subject
}

fn equal_email(a: &[u8], b: &[u8], _flags: u32) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut i = a.len();
    while i > 0 {
        i -= 1;
        if a[i] == b'@' || b[i] == b'@' {
            if !equal_nocase(&a[i..], &b[i..], 0) {
                return false;
            }
            break;
        }
    }
    if i == 0 {
        i = a.len();
    }
    equal_case(&a[..i], &b[..i], 0)
}

fn valid_star(p: &[u8], flags: u32) -> Option<usize> {
    const START: u8 = 1;
    const IDNA: u8 = 2;
    const HYPHEN: u8 = 4;
    let mut star = None;
    let mut state = START;
    let mut dots = 0;
    for i in 0..p.len() {
        let c = p[i];
        if c == b'*' {
            let at_start = state & START != 0;
            let at_end = i == p.len() - 1 || p[i + 1] == b'.';
            if star.is_some() || state & IDNA != 0 || dots > 0 {
                return None;
            }
            if flags & FLAG_NO_PARTIAL_WILDCARDS != 0 && (!at_start || !at_end) {
                return None;
            }
            if !at_start && !at_end {
                return None;
            }
            star = Some(i);
            state &= !START;
        } else if c.is_ascii_alphanumeric() {
            if state & START != 0 && p.len() - i >= 4 && p[i..i + 4].eq_ignore_ascii_case(b"xn--") {
                state |= IDNA;
            }
            state &= !(HYPHEN | START);
        } else if c == b'.' {
            if state & (HYPHEN | START) != 0 {
                return None;
            }
            state = START;
            dots += 1;
        } else if c == b'-' {
            if state & START != 0 {
                return None;
            }
            state |= HYPHEN;
        } else {
            return None;
        }
    }
    if state & (START | HYPHEN) != 0 || dots < 2 {
        return None;
    }
    star
}

fn wildcard_match(prefix: &[u8], suffix: &[u8], subject: &[u8], flags: u32) -> bool {
    if subject.len() < prefix.len() + suffix.len() {
        return false;
    }
    if !equal_nocase(prefix, &subject[..prefix.len()], flags) {
        return false;
    }
    let ws = prefix.len();
    let we = subject.len() - suffix.len();
    if !equal_nocase(&subject[we..], suffix, flags) {
        return false;
    }
    let mut allow_multi = false;
    let mut allow_idna = false;
    if prefix.is_empty() && suffix.first() == Some(&b'.') {
        if ws == we {
            return false;
        }
        allow_idna = true;
        allow_multi = flags & FLAG_MULTI_LABEL_WILDCARDS != 0;
    }
    if !allow_idna && subject.len() >= 4 && subject[..4].eq_ignore_ascii_case(b"xn--") {
        return false;
    }
    if we == ws + 1 && subject[ws] == b'*' {
        return true;
    }
    subject[ws..we].iter().all(|&c| c.is_ascii_alphanumeric() || c == b'-' || (allow_multi && c == b'.'))
}

fn equal_wildcard(pattern: &[u8], subject: &[u8], flags: u32) -> bool {
    let star = if subject.len() > 1 && subject[0] == b'.' { None } else { valid_star(pattern, flags) };
    match star {
        None => equal_nocase(pattern, subject, flags),
        Some(i) => wildcard_match(&pattern[..i], &pattern[i + 1..], subject, flags),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum CheckType {
    Email,
    Dns,
    Ip,
}

/// `do_x509_check`: the matched name, `Ok(None)` for no match.
fn do_check(cert: &Cert, chk: &[u8], mut flags: u32, kind: CheckType) -> Result<Option<Vec<u8>>, ()> {
    let (gen_tag, cn_oid): (u8, Option<&str>) = match kind {
        CheckType::Email => (0x81, Some("1.2.840.113549.1.9.1")),
        CheckType::Dns => {
            if chk.len() > 1 && chk[0] == b'.' {
                flags |= FLAG_DOT_SUBDOMAINS;
            }
            (0x82, Some("2.5.4.3"))
        }
        CheckType::Ip => (0x87, None),
    };
    let equal = |pattern: &[u8], subject: &[u8]| -> bool {
        match kind {
            CheckType::Email => equal_email(pattern, subject, flags),
            CheckType::Dns if flags & FLAG_NO_WILDCARDS != 0 => equal_nocase(pattern, subject, flags),
            CheckType::Dns => equal_wildcard(pattern, subject, flags),
            CheckType::Ip => equal_case(pattern, subject, flags),
        }
    };
    if let Some(ext) = cert.ext(OID_SAN) {
        if let Some(names) = general_names(ext) {
            let mut san_present = false;
            for gen in names.iter().filter(|g| g.tag == gen_tag) {
                san_present = true;
                if !gen.value.is_empty() && equal(gen.value, chk) {
                    return Ok(Some(gen.value.to_vec()));
                }
            }
            if san_present && flags & FLAG_ALWAYS_CHECK_SUBJECT == 0 {
                return Ok(None);
            }
        }
    }
    let Some(cn_oid) = cn_oid else { return Ok(None) };
    if flags & FLAG_NEVER_CHECK_SUBJECT != 0 {
        return Ok(None);
    }
    for rdn in name_rdns(cert.subject.value).unwrap_or_default() {
        for entry in rdn.iter().filter(|e| e.oid == cn_oid) {
            if entry.value.value.is_empty() {
                continue;
            }
            let Some(s) = string_to_utf8(entry.value.tag, entry.value.value) else { return Err(()) };
            if equal(s.as_bytes(), chk) {
                return Ok(Some(s.into_bytes()));
            }
        }
    }
    Ok(None)
}

fn invalid_arg(message: &'static str) -> OpError {
    OpError::type_error(message).with_code("ERR_INVALID_ARG_VALUE")
}

fn operation_failed() -> OpError {
    OpError::error("Operation failed").with_code("ERR_CRYPTO_OPERATION_FAILED")
}

/// The name argument of `X509_check_host` / `X509_check_email`: NUL only as the final byte.
fn check_name(name: &str) -> Result<&[u8], OpError> {
    let b = name.as_bytes();
    let scan = if b.len() > 1 { &b[..b.len() - 1] } else { b };
    if scan.contains(&0) {
        return Err(invalid_arg("Invalid name"));
    }
    Ok(if b.len() > 1 && b[b.len() - 1] == 0 { &b[..b.len() - 1] } else { b })
}

// ---- PEM ----------------------------------------------------------------------------------------

fn pem_encode(label: &str, der: &[u8]) -> String {
    let b64 = codec::base64_encode(der, false, true);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

enum PemError {
    NoStartLine,
    BadBase64,
}

/// The body of the first PEM block whose label is one of `labels`.
fn pem_find(input: &[u8], labels: &[&str]) -> Result<Vec<u8>, PemError> {
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

fn asn1_error() -> OpError {
    OpError::error("error:068000A8:asn1 encoding routines::wrong tag").with_code("ERR_OSSL_ASN1_WRONG_TAG")
}

fn cert_of(der: &[u8]) -> Result<Cert<'_>, OpError> {
    parse_cert(der).ok_or_else(asn1_error)
}

// ---- ops ----------------------------------------------------------------------------------------

/// `parseX509`: the DER of a PEM (`CERTIFICATE`, `X509 CERTIFICATE`, `TRUSTED CERTIFICATE`) or DER
/// certificate.
#[op(name = "x509Parse")]
fn x509_parse(input: &[u8]) -> Result<Vec<u8>, OpError> {
    match pem_find(input, &["CERTIFICATE", "X509 CERTIFICATE", "TRUSTED CERTIFICATE"]) {
        Ok(der) => parse_cert(&der).map(|c| c.raw.to_vec()).ok_or_else(asn1_error),
        Err(PemError::BadBase64) => Err(OpError::error("error:04800064:PEM routines::bad base64 decode")
            .with_code("ERR_OSSL_PEM_BAD_BASE64_DECODE")),
        Err(PemError::NoStartLine) => parse_cert(input).map(|c| c.raw.to_vec()).ok_or_else(|| {
            OpError::error("error:0480006C:PEM routines::no start line").with_code("ERR_OSSL_PEM_NO_START_LINE")
        }),
    }
}

#[op(name = "x509Subject")]
fn x509_subject(der: &[u8]) -> Result<Option<String>, OpError> {
    let c = cert_of(der)?;
    Ok(print_name(c.subject.value, false).map(|b| String::from_utf8_lossy(&b).into_owned()))
}

#[op(name = "x509Issuer")]
fn x509_issuer(der: &[u8]) -> Result<Option<String>, OpError> {
    let c = cert_of(der)?;
    Ok(print_name(c.issuer.value, false).map(|b| String::from_utf8_lossy(&b).into_owned()))
}

/// `[present, text]`: text is `null` when the extension cannot be decoded.
#[op(name = "x509SubjectAltName")]
fn x509_subject_alt_name(der: &[u8]) -> Result<(bool, Option<String>), OpError> {
    let c = cert_of(der)?;
    Ok(match c.ext(OID_SAN) {
        Some(ext) => (true, san_text(ext)),
        None => (false, None),
    })
}

#[op(name = "x509InfoAccess")]
fn x509_info_access(der: &[u8]) -> Result<(bool, Option<String>), OpError> {
    let c = cert_of(der)?;
    Ok(match c.ext(OID_AIA) {
        Some(ext) => (true, info_access_text(ext)),
        None => (false, None),
    })
}

/// `[validFrom, validTo]`.
#[op(name = "x509Validity")]
fn x509_validity(der: &[u8]) -> Result<(String, String), OpError> {
    let c = cert_of(der)?;
    Ok((print_time(&c.not_before).unwrap_or_default(), print_time(&c.not_after).unwrap_or_default()))
}

#[op(name = "x509Fingerprint")]
fn x509_fingerprint(der: &[u8], algorithm: &str) -> Result<String, OpError> {
    let c = cert_of(der)?;
    let algo = Algo::from_name(algorithm).ok_or_else(|| OpError::error("Digest method not supported"))?;
    let md = hash::digest(algo, c.raw);
    Ok(md.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":"))
}

/// The extended key usage OIDs, `null` without the extension.
#[op(name = "x509KeyUsage")]
fn x509_key_usage(der: &[u8]) -> Result<Option<Vec<String>>, OpError> {
    let c = cert_of(der)?;
    let Some(ext) = c.ext(OID_EKU) else { return Ok(None) };
    let mut input = ext;
    let Some(seq) = expect(&mut input, SEQ) else { return Ok(None) };
    Ok(children(seq.value).map(|oids| oids.iter().filter(|t| t.tag == OID).map(|t| oid_text(t.value)).collect()))
}

/// `BN_bn2hex` of the serial number.
#[op(name = "x509SerialNumber")]
fn x509_serial_number(der: &[u8]) -> Result<String, OpError> {
    let c = cert_of(der)?;
    let negative = c.serial.first().is_some_and(|&b| b & 0x80 != 0);
    let magnitude = if negative {
        let mut v = c.serial.to_vec();
        let mut carry = true;
        for b in v.iter_mut().rev() {
            *b = !*b;
            if carry {
                let (r, o) = b.overflowing_add(1);
                *b = r;
                carry = o;
            }
        }
        v
    } else {
        c.serial.to_vec()
    };
    let hex = hex_upper(strip_zeros(&magnitude));
    Ok(match (negative, hex.is_empty()) {
        (_, true) => "0".to_string(),
        (true, false) => format!("-{hex}"),
        (false, false) => hex,
    })
}

#[op(name = "x509Pem")]
fn x509_pem(der: &[u8]) -> Result<String, OpError> {
    let c = cert_of(der)?;
    Ok(pem_encode("CERTIFICATE", c.raw))
}

/// The SubjectPublicKeyInfo DER; throws like `X509_get_pubkey` when the key cannot be decoded.
#[op(name = "x509PublicKey")]
fn x509_public_key(der: &[u8]) -> Result<Vec<u8>, OpError> {
    let c = cert_of(der)?;
    match parse_spki(c.spki.raw) {
        Some(spki) if spki_is_valid(&spki) => Ok(c.spki.raw.to_vec()),
        _ => Err(OpError::error("error:03000072:digital envelope routines::decode error")
            .with_code("ERR_OSSL_EVP_DECODE_ERROR")),
    }
}

#[op(name = "x509CheckCA")]
fn x509_check_ca(der: &[u8]) -> Result<bool, OpError> {
    Ok(is_ca(&cert_of(der)?))
}

#[op(name = "x509CheckHost")]
fn x509_check_host(der: &[u8], name: &str, flags: u32) -> Result<Option<String>, OpError> {
    let c = cert_of(der)?;
    let chk = check_name(name)?;
    match do_check(&c, chk, flags, CheckType::Dns) {
        Ok(found) => Ok(found.map(|b| b.iter().map(|&x| x as char).collect())),
        Err(()) => Err(operation_failed()),
    }
}

#[op(name = "x509CheckEmail")]
fn x509_check_email(der: &[u8], email: &str, flags: u32) -> Result<bool, OpError> {
    let c = cert_of(der)?;
    let chk = check_name(email)?;
    match do_check(&c, chk, flags, CheckType::Email) {
        Ok(found) => Ok(found.is_some()),
        Err(()) => Err(operation_failed()),
    }
}

/// `X509_check_ip_asc`.
#[op(name = "x509CheckIP")]
fn x509_check_ip(der: &[u8], ip: &str, flags: u32) -> Result<bool, OpError> {
    let c = cert_of(der)?;
    let addr: Vec<u8> = match ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(a)) => a.octets().to_vec(),
        Ok(std::net::IpAddr::V6(a)) => a.octets().to_vec(),
        Err(_) => return Err(invalid_arg("Invalid IP")),
    };
    match do_check(&c, &addr, flags, CheckType::Ip) {
        Ok(found) => Ok(found.is_some()),
        Err(()) => Err(operation_failed()),
    }
}

/// Whether `issuer` issued `der` (`X509_check_issued`).
#[op(name = "x509CheckIssued")]
fn x509_check_issued(der: &[u8], issuer: &[u8]) -> Result<bool, OpError> {
    Ok(check_issued(&cert_of(issuer)?, &cert_of(der)?))
}

/// Whether the PKCS#8 private key matches the certificate's public key.
#[op(name = "x509CheckPrivateKey")]
fn x509_check_private_key(der: &[u8], pkcs8: &[u8]) -> Result<bool, OpError> {
    let c = cert_of(der)?;
    let Some(spki) = parse_spki(c.spki.raw) else { return Ok(false) };
    let Some((alg, public)) = private_to_public(pkcs8) else { return Ok(false) };
    Ok(alg == spki.alg && spki_public(&spki).is_some_and(|p| p == public))
}

/// Whether the certificate's signature verifies under the SPKI public key.
#[op(name = "x509Verify")]
fn x509_verify(der: &[u8], spki: &[u8]) -> Result<bool, OpError> {
    let c = cert_of(der)?;
    Ok(verify_signature(&c.sig_alg, spki, c.tbs, c.signature))
}

/// The entries of the subject (`issuer` false) or issuer name as `[type, value, ...]` for the
/// legacy object; `null` when a value is not a string.
#[op(name = "x509NameEntries")]
fn x509_name_entries(der: &[u8], issuer: bool) -> Result<Option<Vec<String>>, OpError> {
    let c = cert_of(der)?;
    let name = if issuer { c.issuer } else { c.subject };
    let Some(rdns) = name_rdns(name.value) else { return Ok(None) };
    let mut out = Vec::new();
    for entry in rdns.iter().flatten() {
        let Some(value) = string_to_utf8(entry.value.tag, entry.value.value) else { return Ok(None) };
        out.push(attribute_short_name(&entry.oid).map_or_else(|| entry.oid.clone(), str::to_string));
        out.push(value);
    }
    Ok(Some(out))
}

/// Key details of the legacy object: `["rsa", [modulus, exponent], bits, pubkey]`,
/// `["ec", [asn1Curve, nistCurve], bits, point]` or `["", [], 0, []]`.
#[op(name = "x509KeyDetails")]
fn x509_key_details(der: &[u8]) -> Result<(String, Vec<String>, u32, Vec<u8>), OpError> {
    let c = cert_of(der)?;
    let none = || (String::new(), Vec::new(), 0, Vec::new());
    let Some(spki) = parse_spki(c.spki.raw) else { return Ok(none()) };
    match spki.alg.as_str() {
        OID_RSA => {
            let Some((n, e)) = rsa_public(spki.key) else { return Ok(none()) };
            let n = strip_zeros(&n);
            let e = strip_zeros(&e);
            let mut modulus = hex_upper(n).trim_start_matches('0').to_string();
            if modulus.is_empty() {
                modulus.push('0');
            }
            let bits = n.first().map_or(0, |&b| (n.len() as u32 - 1) * 8 + (8 - b.leading_zeros()));
            let exponent = format!("0x{:x}", e.iter().take(8).fold(0u64, |acc, &b| (acc << 8) | b as u64));
            Ok(("rsa".into(), vec![modulus, exponent], bits, c.spki.raw.to_vec()))
        }
        OID_EC => {
            let Some(curve) = curve_of(spki.params) else { return Ok(none()) };
            let (sn, nist, bits) = match curve {
                Curve::P256 => ("prime256v1", "P-256", 256),
                Curve::P384 => ("secp384r1", "P-384", 384),
                Curve::P521 => ("secp521r1", "P-521", 521),
                Curve::K256 => ("secp256k1", "", 256),
            };
            Ok(("ec".into(), vec![sn.into(), nist.into()], bits, spki.key.to_vec()))
        }
        _ => Ok(none()),
    }
}

// ---- SPKAC --------------------------------------------------------------------------------------

/// The DER of a base64 SignedPublicKeyAndChallenge, trimmed like `EVP_DecodeBlock`.
fn spkac_decode(input: &[u8]) -> Option<Vec<u8>> {
    let start = input.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(input.len());
    let mut end = input.len();
    while end > start + 3 && !(input[end - 1].is_ascii_alphanumeric() || matches!(input[end - 1], b'+' | b'/' | b'=')) {
        end -= 1;
    }
    let text = &input[start..end];
    codec::base64_decode_strict(text, false, Padding::Optional).ok()
}

struct Spkac<'a> {
    pkac: &'a [u8],
    spki: &'a [u8],
    challenge: Tlv<'a>,
    sig_alg: Tlv<'a>,
    signature: &'a [u8],
}

fn spkac_parse(der: &[u8]) -> Option<Spkac<'_>> {
    let mut input = der;
    let seq = expect(&mut input, SEQ)?;
    let mut s = seq.value;
    let pkac = expect(&mut s, SEQ)?;
    let sig_alg = expect(&mut s, SEQ)?;
    let sig = expect(&mut s, BIT_STRING)?;
    let mut p = pkac.value;
    let spki = expect(&mut p, SEQ)?;
    let challenge = read_tlv(&mut p)?;
    Some(Spkac { pkac: pkac.raw, spki: spki.raw, challenge, sig_alg, signature: sig.value.get(1..)? })
}

fn spkac_too_large(input: &[u8]) -> Result<(), OpError> {
    if input.len() > i32::MAX as usize {
        return Err(OpError::range_error("spkac is too large").with_code("ERR_OUT_OF_RANGE"));
    }
    Ok(())
}

#[op(name = "certVerifySpkac")]
fn cert_verify_spkac(input: &[u8]) -> Result<bool, OpError> {
    spkac_too_large(input)?;
    let Some(der) = spkac_decode(input) else { return Ok(false) };
    let Some(s) = spkac_parse(&der) else { return Ok(false) };
    match parse_spki(s.spki) {
        Some(spki) if spki_is_valid(&spki) => Ok(verify_signature(&s.sig_alg, s.spki, s.pkac, s.signature)),
        _ => Ok(false),
    }
}

/// The SPKAC's public key as PEM, `null` when it cannot be decoded.
#[op(name = "certExportPublicKey")]
fn cert_export_public_key(input: &[u8]) -> Result<Option<Vec<u8>>, OpError> {
    spkac_too_large(input)?;
    let Some(der) = spkac_decode(input) else { return Ok(None) };
    let Some(s) = spkac_parse(&der) else { return Ok(None) };
    match parse_spki(s.spki) {
        Some(spki) if spki_is_valid(&spki) => Ok(Some(pem_encode("PUBLIC KEY", s.spki).into_bytes())),
        _ => Ok(None),
    }
}

#[op(name = "certExportChallenge")]
fn cert_export_challenge(input: &[u8]) -> Result<Option<Vec<u8>>, OpError> {
    spkac_too_large(input)?;
    let Some(der) = spkac_decode(input) else { return Ok(None) };
    let Some(s) = spkac_parse(&der) else { return Ok(None) };
    Ok(string_to_utf8(s.challenge.tag, s.challenge.value).map(String::into_bytes))
}
}
