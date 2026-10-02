//! Minimal DER reading and writing for key containers (SPKI, PKCS#8, PKCS#1, SEC1, DSA, DH).
//! Only shaping lives here: tag/length framing, integers and OIDs. Every key operation is done by
//! the RustCrypto crates.

use der::asn1::ObjectIdentifier;
use num_bigint_dig::BigUint;

pub use lumen_crypto::pad_be;

pub const TAG_INTEGER: u8 = 0x02;
pub const TAG_BIT_STRING: u8 = 0x03;
pub const TAG_OCTET_STRING: u8 = 0x04;
pub const TAG_NULL: u8 = 0x05;
pub const TAG_OID: u8 = 0x06;
pub const TAG_SEQUENCE: u8 = 0x30;

pub const OID_RSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
pub const OID_RSA_PSS: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.10");
pub const OID_MGF1: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.8");
pub const OID_DSA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10040.4.1");
pub const OID_EC: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.2.1");
pub const OID_PRIME_FIELD: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10045.1.1");
pub const OID_ED25519: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.112");
pub const OID_ED448: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.113");
pub const OID_X25519: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.110");
pub const OID_X448: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.3.101.111");
pub const OID_DH: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.3.1");
pub const OID_DHX: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.10046.2.1");

/// A forward-only DER reader over one level of nesting.
#[derive(Clone, Copy)]
pub struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data }
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn peek_tag(&self) -> Option<u8> {
        self.data.first().copied()
    }

    /// The next element as `(tag, content)`.
    pub fn read(&mut self) -> Option<(u8, &'a [u8])> {
        let (tag, content, rest) = split_tlv(self.data)?;
        self.data = rest;
        Some((tag, content))
    }

    /// The next element, whole (tag and length included).
    pub fn read_raw(&mut self) -> Option<&'a [u8]> {
        let before = self.data;
        let (_, _, rest) = split_tlv(self.data)?;
        self.data = rest;
        Some(&before[..before.len() - rest.len()])
    }

    pub fn expect(&mut self, tag: u8) -> Option<&'a [u8]> {
        match self.read()? {
            (t, c) if t == tag => Some(c),
            _ => None,
        }
    }

    /// The content of the next element if it has `tag`; nothing is consumed otherwise.
    pub fn optional(&mut self, tag: u8) -> Option<&'a [u8]> {
        if self.peek_tag() == Some(tag) {
            self.read().map(|(_, c)| c)
        } else {
            None
        }
    }

    pub fn sequence(&mut self) -> Option<Reader<'a>> {
        self.expect(TAG_SEQUENCE).map(Reader::new)
    }

    /// A non-negative INTEGER as big-endian magnitude bytes (no leading zeros).
    pub fn uint(&mut self) -> Option<&'a [u8]> {
        let c = self.expect(TAG_INTEGER)?;
        if c.is_empty() || c[0] & 0x80 != 0 {
            return None;
        }
        let mut i = 0;
        while i + 1 < c.len() && c[i] == 0 {
            i += 1;
        }
        Some(&c[i..])
    }

    pub fn biguint(&mut self) -> Option<BigUint> {
        self.uint().map(BigUint::from_bytes_be)
    }

    pub fn small_uint(&mut self) -> Option<u64> {
        let b = self.uint()?;
        if b.len() > 8 {
            return None;
        }
        Some(b.iter().fold(0u64, |acc, x| (acc << 8) | *x as u64))
    }

    pub fn oid(&mut self) -> Option<ObjectIdentifier> {
        let c = self.expect(TAG_OID)?;
        ObjectIdentifier::from_bytes(c).ok()
    }

    /// A BIT STRING without unused bits.
    pub fn bit_string(&mut self) -> Option<&'a [u8]> {
        let c = self.expect(TAG_BIT_STRING)?;
        match c.split_first() {
            Some((0, rest)) => Some(rest),
            _ => None,
        }
    }

    pub fn finish(self) -> Option<()> {
        self.data.is_empty().then_some(())
    }
}

fn split_tlv(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *data.first()?;
    if tag & 0x1f == 0x1f {
        return None;
    }
    let first = *data.get(1)? as usize;
    let (len, header) = if first < 0x80 {
        (first, 2)
    } else {
        let n = first & 0x7f;
        if n == 0 || n > 4 {
            return None;
        }
        let bytes = data.get(2..2 + n)?;
        let len = bytes.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize);
        (len, 2 + n)
    };
    let end = header.checked_add(len)?;
    let content = data.get(header..end)?;
    Some((tag, content, &data[end..]))
}

/// The element starting at `data`'s first byte, whole, if `data` holds exactly one element.
pub fn single(data: &[u8]) -> Option<(u8, &[u8])> {
    let (tag, content, rest) = split_tlv(data)?;
    rest.is_empty().then_some((tag, content))
}

pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(content.len() + 6);
    out.push(tag);
    let len = content.len();
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push(0x80 | (bytes.len() - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

pub fn seq(parts: &[&[u8]]) -> Vec<u8> {
    tlv(TAG_SEQUENCE, &parts.concat())
}

/// A non-negative INTEGER from big-endian magnitude bytes.
pub fn uint(be: &[u8]) -> Vec<u8> {
    let start = be.iter().position(|b| *b != 0).unwrap_or(be.len());
    let mag = &be[start..];
    if mag.is_empty() {
        return tlv(TAG_INTEGER, &[0]);
    }
    if mag[0] & 0x80 != 0 {
        let mut c = Vec::with_capacity(mag.len() + 1);
        c.push(0);
        c.extend_from_slice(mag);
        tlv(TAG_INTEGER, &c)
    } else {
        tlv(TAG_INTEGER, mag)
    }
}

pub fn biguint(n: &BigUint) -> Vec<u8> {
    uint(&n.to_bytes_be())
}

pub fn small_uint(n: u64) -> Vec<u8> {
    uint(&n.to_be_bytes())
}

pub fn oid(o: &ObjectIdentifier) -> Vec<u8> {
    tlv(TAG_OID, o.as_bytes())
}

pub fn null() -> Vec<u8> {
    vec![TAG_NULL, 0]
}

pub fn octets(c: &[u8]) -> Vec<u8> {
    tlv(TAG_OCTET_STRING, c)
}

pub fn bit_string(c: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(c.len() + 1);
    v.push(0);
    v.extend_from_slice(c);
    tlv(TAG_BIT_STRING, &v)
}

/// A context-specific constructed (explicit) tag `[n]`.
pub fn explicit(n: u8, content: &[u8]) -> Vec<u8> {
    tlv(0xa0 | n, content)
}

