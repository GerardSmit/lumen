//! RSA helpers every backend shares: OpenSSL-worded errors for the common failures, the PKCS#1
//! `DigestInfo` prefixes, length checks against the modulus and unpadding of block type 1.

use crate::error::{CryptoError, Result};
use crate::{trim_be, Algo, RsaPublicKey};

pub(crate) fn ossl(hex: &str, library: &str, reason: &str) -> CryptoError {
    CryptoError::openssl(hex, library, reason)
}

pub(crate) fn decoder_unsupported() -> CryptoError {
    ossl("1E08010C", "DECODER routines", "unsupported")
}

pub(crate) fn data_too_large() -> CryptoError {
    ossl("0200006E", "rsa routines", "data too large for key size")
}

pub(crate) fn data_too_small() -> CryptoError {
    ossl("0200007A", "rsa routines", "data too small for key size")
}

pub(crate) fn too_large_for_modulus() -> CryptoError {
    ossl("02000084", "rsa routines", "data too large for modulus")
}

pub(crate) fn illegal_padding() -> CryptoError {
    ossl("1C8000A5", "Provider routines", "illegal or unsupported padding mode")
}

pub(crate) fn invalid_digest() -> CryptoError {
    ossl("1C80007A", "Provider routines", "invalid digest")
}

pub(crate) fn padding_check_failed() -> CryptoError {
    ossl("02000072", "rsa routines", "padding check failed")
}

pub(crate) fn oaep_decoding() -> CryptoError {
    ossl("02000079", "rsa routines", "oaep decoding error")
}

pub(crate) fn digest_too_big() -> CryptoError {
    ossl("02000070", "rsa routines", "digest too big for rsa key")
}

pub(crate) fn sign_failed() -> CryptoError {
    CryptoError::failed("Failed to sign")
}

/// The number of significant bits of a big-endian integer.
pub(crate) fn bit_len(n: &[u8]) -> usize {
    let n = trim_be(n);
    match n.first() {
        None => 0,
        Some(top) => n.len() * 8 - top.leading_zeros() as usize,
    }
}

/// The length in bytes of the modulus.
pub(crate) fn mod_len(n: &[u8]) -> usize {
    bit_len(n).div_ceil(8)
}

/// Whether `a >= b` as integers.
pub(crate) fn be_ge(a: &[u8], b: &[u8]) -> bool {
    let (a, b) = (trim_be(a), trim_be(b));
    a.len() > b.len() || (a.len() == b.len() && a >= b)
}

/// Checks the input of a raw RSA operation against the modulus the way OpenSSL reports it;
/// returns the modulus length.
pub(crate) fn check_block_len(k: &RsaPublicKey, input: &[u8]) -> Result<usize> {
    let k_len = mod_len(&k.n);
    if input.len() > k_len {
        return Err(data_too_large());
    }
    if input.len() < k_len {
        return Err(data_too_small());
    }
    if be_ge(input, &k.n) {
        return Err(too_large_for_modulus());
    }
    Ok(k_len)
}

/// The OID content bytes of a digest, for the PKCS#1 `DigestInfo`.
fn digest_oid(algo: Algo) -> Option<&'static [u8]> {
    const SHA2: [u8; 8] = [0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02];
    macro_rules! nist {
        ($last:expr) => {{
            const OID: [u8; 9] = [SHA2[0], SHA2[1], SHA2[2], SHA2[3], SHA2[4], SHA2[5], SHA2[6], SHA2[7], $last];
            &OID
        }};
    }
    Some(match algo {
        Algo::Md5 => &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x02, 0x05],
        Algo::Sha1 => &[0x2b, 0x0e, 0x03, 0x02, 0x1a],
        Algo::Sha224 => nist!(0x04),
        Algo::Sha256 => nist!(0x01),
        Algo::Sha384 => nist!(0x02),
        Algo::Sha512 => nist!(0x03),
        Algo::Sha512_224 => nist!(0x05),
        Algo::Sha512_256 => nist!(0x06),
        Algo::Sha3_224 => nist!(0x07),
        Algo::Sha3_256 => nist!(0x08),
        Algo::Sha3_384 => nist!(0x09),
        Algo::Sha3_512 => nist!(0x0a),
        Algo::Ripemd160 => &[0x2b, 0x24, 0x03, 0x02, 0x01],
        Algo::Sm3 => &[0x2a, 0x81, 0x1c, 0xcf, 0x55, 0x01, 0x83, 0x11],
        _ => return None,
    })
}

/// The DER `DigestInfo` header that precedes the digest in a PKCS#1 v1.5 signature (empty for
/// `md5-sha1`, which signs the bare concatenation).
pub(crate) fn digest_info_prefix(algo: Algo) -> Result<Vec<u8>> {
    if algo == Algo::Md5Sha1 {
        return Ok(Vec::new());
    }
    let oid = digest_oid(algo).ok_or_else(invalid_digest)?;
    let algorithm_len = oid.len() + 4;
    let total = algorithm_len + 4 + algo.out_len();
    let mut out = vec![0x30, total as u8, 0x30, algorithm_len as u8, 0x06, oid.len() as u8];
    out.extend_from_slice(oid);
    out.extend_from_slice(&[0x05, 0x00, 0x04, algo.out_len() as u8]);
    Ok(out)
}

/// The `DigestInfo` block a PKCS#1 v1.5 signature over `hashed` carries.
pub(crate) fn digest_info(algo: Algo, hashed: &[u8]) -> Result<Vec<u8>> {
    let mut block = digest_info_prefix(algo)?;
    block.extend_from_slice(hashed);
    Ok(block)
}

/// Recovers the message of a block-type-1 padded block (`00 01 FF..FF 00 msg`). Everything involved
/// is public, so the unpadding needs no constant-time care.
pub(crate) fn unpad_type1(em: &[u8]) -> Result<Vec<u8>> {
    if em.len() < 2 || em[0] != 0 || em[1] != 1 {
        return Err(padding_check_failed());
    }
    let body = &em[2..];
    let sep = body.iter().position(|&b| b != 0xff).ok_or_else(padding_check_failed)?;
    if sep < 8 || body[sep] != 0 {
        return Err(padding_check_failed());
    }
    Ok(body[sep + 1..].to_vec())
}
