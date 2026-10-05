//! The little DER the system backends exchange with their APIs: PKCS#1 RSA keys (Security.framework)
//! and `Dss-Sig-Value` (CNG hands DSA signatures over as `r || s`).

use crate::trim_be;
#[cfg(crypto_apple)]
use crate::{RsaPrivateKey, RsaPublicKey};

fn push_len(out: &mut Vec<u8>, len: usize) {
    if len < 0x80 {
        out.push(len as u8);
    } else {
        let bytes = len.to_be_bytes();
        let bytes = trim_be(&bytes);
        out.push(0x80 | bytes.len() as u8);
        out.extend_from_slice(bytes);
    }
}

fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    push_len(&mut out, content.len());
    out.extend_from_slice(content);
    out
}

fn integer(value: &[u8]) -> Vec<u8> {
    let value = trim_be(value);
    let mut content = Vec::with_capacity(value.len() + 1);
    if value.first().is_none_or(|b| b & 0x80 != 0) {
        content.push(0);
    }
    content.extend_from_slice(value);
    tlv(0x02, &content)
}

/// Splits one element off `input`: its tag, its content and what follows.
fn read(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, rest) = if first < 0x80 {
        (first as usize, rest)
    } else {
        let count = (first & 0x7f) as usize;
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        (rest[..count].iter().fold(0usize, |acc, b| (acc << 8) | *b as usize), &rest[count..])
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

/// The integers of a DER `SEQUENCE` of `INTEGER`s, without leading zeros.
fn integers(der: &[u8]) -> Option<Vec<Vec<u8>>> {
    let (0x30, mut rest, trailing) = read(der)? else { return None };
    if !trailing.is_empty() {
        return None;
    }
    let mut ints = Vec::new();
    while !rest.is_empty() {
        let (0x02, value, next) = read(rest)? else { return None };
        ints.push(trim_be(value).to_vec());
        rest = next;
    }
    Some(ints)
}

#[cfg(crypto_apple)]
pub(crate) fn rsa_public(key: &RsaPublicKey) -> Vec<u8> {
    tlv(0x30, &[integer(&key.n), integer(&key.e)].concat())
}

#[cfg(crypto_apple)]
pub(crate) fn rsa_private(key: &RsaPrivateKey) -> Vec<u8> {
    let parts = [
        integer(&[0]),
        integer(&key.public.n),
        integer(&key.public.e),
        integer(&key.d),
        integer(&key.p),
        integer(&key.q),
        integer(&key.dp),
        integer(&key.dq),
        integer(&key.qi),
    ];
    tlv(0x30, &parts.concat())
}

#[cfg(crypto_apple)]
pub(crate) fn parse_rsa_private(der: &[u8]) -> Option<RsaPrivateKey> {
    let [_version, n, e, d, p, q, dp, dq, qi] = <[Vec<u8>; 9]>::try_from(integers(der)?).ok()?;
    Some(RsaPrivateKey { public: RsaPublicKey { n, e }, d, p, q, dp, dq, qi })
}

#[cfg_attr(not(crypto_cng), allow(dead_code))]
pub(crate) fn dss_signature(r: &[u8], s: &[u8]) -> Vec<u8> {
    tlv(0x30, &[integer(r), integer(s)].concat())
}

#[cfg_attr(not(crypto_cng), allow(dead_code))]
pub(crate) fn parse_dss_signature(der: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let [r, s] = <[Vec<u8>; 2]>::try_from(integers(der)?).ok()?;
    Some((r, s))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dss_signature_roundtrip() {
        let der = dss_signature(&[0x80, 1], &[0, 0, 5]);
        assert_eq!(der, [0x30, 0x08, 0x02, 0x03, 0x00, 0x80, 0x01, 0x02, 0x01, 0x05]);
        assert_eq!(parse_dss_signature(&der), Some((vec![0x80, 1], vec![5])));
        assert_eq!(parse_dss_signature(&der[..9]), None);
    }
}
