//! Signatures (RSA PKCS#1 v1.5 and PSS, DSA, ECDSA, Ed25519, Ed448) and RSA encryption padding.
//! The padding schemes are written out over the RSA primitive so that any digest `node:crypto`
//! names can be used; the curve and DSA arithmetic is RustCrypto's.

use lumen::embed::{OpDesc, OpError};
use num_bigint_dig::BigUint;
use rand_core::{OsRng, RngCore};
use signature::hazmat::{PrehashSigner, PrehashVerifier, RandomizedPrehashSigner};
use signature::Signer;

use super::keys::{asn1, with_ec_curve, AsymKey, EcCurve, EcKey, PssParams, RsaKey};
use crate::hash::{self, Algo};

const RSA_PKCS1_PADDING: i32 = 1;
const RSA_NO_PADDING: i32 = 3;
const RSA_PKCS1_OAEP_PADDING: i32 = 4;
const RSA_PKCS1_PSS_PADDING: i32 = 6;
const SALT_DIGEST: i32 = -1;
const SALT_MAX_OR_AUTO: i32 = -2;

fn ossl(hex: &str, library: &str, reason: &str, code: &'static str) -> OpError {
    OpError::error(format!("error:{hex}:{library}::{reason}")).with_code(code)
}

fn data_too_large() -> OpError {
    ossl("0200006E", "rsa routines", "data too large for key size", "ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_KEY_SIZE")
}

fn data_too_small() -> OpError {
    ossl("0200007A", "rsa routines", "data too small for key size", "ERR_OSSL_RSA_DATA_TOO_SMALL_FOR_KEY_SIZE")
}

fn illegal_padding() -> OpError {
    ossl("1C8000A5", "Provider routines", "illegal or unsupported padding mode", "ERR_OSSL_ILLEGAL_OR_UNSUPPORTED_PADDING_MODE")
}

fn invalid_digest() -> OpError {
    ossl("1C80007A", "Provider routines", "invalid digest", "ERR_OSSL_INVALID_DIGEST")
}

fn digest_not_allowed() -> OpError {
    ossl("1C8000AE", "Provider routines", "digest not allowed", "ERR_OSSL_DIGEST_NOT_ALLOWED")
}

fn invalid_salt() -> OpError {
    ossl("1C8000A8", "Provider routines", "invalid salt length", "ERR_OSSL_INVALID_SALT_LENGTH")
}

fn padding_check_failed() -> OpError {
    ossl("02000072", "rsa routines", "padding check failed", "ERR_OSSL_RSA_PADDING_CHECK_FAILED")
}

fn oaep_decoding() -> OpError {
    ossl("02000079", "rsa routines", "oaep decoding error", "ERR_OSSL_RSA_OAEP_DECODING_ERROR")
}

fn unsupported_keytype() -> OpError {
    ossl(
        "03000096",
        "digital envelope routines",
        "operation not supported for this keytype",
        "ERR_OSSL_EVP_OPERATION_NOT_SUPPORTED_FOR_THIS_KEYTYPE",
    )
}

fn sign_failed() -> OpError {
    OpError::error("Failed to sign").with_code("ERR_CRYPTO_OPERATION_FAILED")
}

fn digest_algo(name: &str) -> Result<Algo, OpError> {
    Algo::from_name(name)
        .ok_or_else(|| OpError::type_error(format!("Invalid digest: {name}")).with_code("ERR_CRYPTO_INVALID_DIGEST"))
}

fn digest_oid(algo: Algo) -> Option<&'static str> {
    Some(match algo {
        Algo::Md5 => "1.2.840.113549.2.5",
        Algo::Sha1 => "1.3.14.3.2.26",
        Algo::Sha224 => "2.16.840.1.101.3.4.2.4",
        Algo::Sha256 => "2.16.840.1.101.3.4.2.1",
        Algo::Sha384 => "2.16.840.1.101.3.4.2.2",
        Algo::Sha512 => "2.16.840.1.101.3.4.2.3",
        Algo::Sha512_224 => "2.16.840.1.101.3.4.2.5",
        Algo::Sha512_256 => "2.16.840.1.101.3.4.2.6",
        Algo::Sha3_224 => "2.16.840.1.101.3.4.2.7",
        Algo::Sha3_256 => "2.16.840.1.101.3.4.2.8",
        Algo::Sha3_384 => "2.16.840.1.101.3.4.2.9",
        Algo::Sha3_512 => "2.16.840.1.101.3.4.2.10",
        Algo::Ripemd160 => "1.3.36.3.2.1",
        Algo::Sm3 => "1.2.156.10197.1.401",
        _ => return None,
    })
}

/// The DER `DigestInfo` of a PKCS#1 v1.5 signature (`md5-sha1` signs the bare concatenation).
fn digest_info(algo: Algo, hashed: &[u8]) -> Result<Vec<u8>, OpError> {
    if algo == Algo::Md5Sha1 {
        return Ok(hashed.to_vec());
    }
    let oid = digest_oid(algo).ok_or_else(invalid_digest)?;
    let oid = der::asn1::ObjectIdentifier::new(oid).map_err(|_| invalid_digest())?;
    let id = asn1::seq(&[&asn1::oid(&oid), &asn1::null()]);
    Ok(asn1::seq(&[&id, &asn1::octets(hashed)]))
}

fn mgf1(algo: Algo, seed: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + algo.out_len());
    let mut counter = 0u32;
    while out.len() < len {
        let mut block = seed.to_vec();
        block.extend_from_slice(&counter.to_be_bytes());
        out.extend(hash::digest(algo, &block));
        counter += 1;
    }
    out.truncate(len);
    out
}

fn xor_into(a: &mut [u8], b: &[u8]) {
    for (x, y) in a.iter_mut().zip(b) {
        *x ^= y;
    }
}

fn mod_len(k: &RsaKey) -> usize {
    k.n.bits().div_ceil(8)
}

fn rsa_public_op(k: &RsaKey, m: &BigUint) -> BigUint {
    m.modpow(&k.e, &k.n)
}

fn rsa_private_op(k: &RsaKey, c: &BigUint) -> Result<BigUint, OpError> {
    let parts = k.private.as_ref().ok_or_else(|| OpError::error("key is not a private key"))?;
    let zero = BigUint::from(0u8);
    if parts.p == zero || parts.q == zero || parts.dp == zero || parts.dq == zero {
        return Ok(c.modpow(&parts.d, &k.n));
    }
    let m1 = c.modpow(&parts.dp, &parts.p);
    let m2 = c.modpow(&parts.dq, &parts.q);
    let diff = if m1 >= m2 { &m1 - &m2 } else { &m1 + &parts.p - (&m2 % &parts.p) };
    let h = (&parts.qi * diff) % &parts.p;
    Ok(m2 + h * &parts.q)
}

fn random_nonzero(len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    OsRng.fill_bytes(&mut v);
    for b in v.iter_mut() {
        while *b == 0 {
            let mut one = [0u8; 1];
            OsRng.fill_bytes(&mut one);
            *b = one[0];
        }
    }
    v
}

fn rsa_run(k: &RsaKey, private: bool, input: &BigUint) -> Result<BigUint, OpError> {
    if input >= &k.n {
        return Err(ossl("02000084", "rsa routines", "data too large for modulus", "ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_MODULUS"));
    }
    if private {
        rsa_private_op(k, input)
    } else {
        Ok(rsa_public_op(k, input))
    }
}

struct PssConfig {
    hash: Algo,
    mgf1: Algo,
    salt: i32,
}

fn pss_config(k: &RsaKey, hash: Option<&str>, salt: Option<i32>) -> Result<PssConfig, OpError> {
    let requested = hash.map(digest_algo).transpose()?;
    let restriction: Option<PssParams> = k.pss.flatten();
    if let Some(r) = restriction {
        let key_hash = digest_algo(r.hash)?;
        if requested.is_some_and(|h| h != key_hash) {
            return Err(digest_not_allowed());
        }
        let salt = match salt {
            Some(s) if s >= 0 && (s as u32) < r.salt_length => return Err(invalid_salt()),
            Some(s) => s,
            None => r.salt_length as i32,
        };
        return Ok(PssConfig { hash: key_hash, mgf1: digest_algo(r.mgf1_hash)?, salt });
    }
    let hash = requested.unwrap_or(Algo::Sha256);
    let salt = salt.unwrap_or(SALT_MAX_OR_AUTO);
        Ok(PssConfig { hash, mgf1: hash, salt })
}

fn pss_encode(em_bits: usize, mhash: &[u8], cfg: &PssConfig, signing_salt: usize) -> Result<Vec<u8>, OpError> {
    let em_len = em_bits.div_ceil(8);
    let h_len = cfg.hash.out_len();
    if em_len < h_len + signing_salt + 2 {
        return Err(data_too_large());
    }
    let mut salt = vec![0u8; signing_salt];
    OsRng.fill_bytes(&mut salt);
    let mut m = vec![0u8; 8];
    m.extend_from_slice(mhash);
    m.extend_from_slice(&salt);
    let h = hash::digest(cfg.hash, &m);
    let mut db = vec![0u8; em_len - h_len - 1];
    let one = db.len() - signing_salt - 1;
    db[one] = 1;
    db[one + 1..].copy_from_slice(&salt);
    let mask = mgf1(cfg.mgf1, &h, db.len());
    xor_into(&mut db, &mask);
    db[0] &= 0xff >> (8 * em_len - em_bits);
    db.extend_from_slice(&h);
    db.push(0xbc);
    Ok(db)
}

fn pss_verify(em_bits: usize, em: &[u8], mhash: &[u8], cfg: &PssConfig) -> bool {
    let em_len = em_bits.div_ceil(8);
    let h_len = cfg.hash.out_len();
    if em.len() != em_len || em_len < h_len + 2 || em[em_len - 1] != 0xbc {
        return false;
    }
    let (masked, rest) = em.split_at(em_len - h_len - 1);
    let h = &rest[..h_len];
    let top_mask = 0xffu8 >> (8 * em_len - em_bits);
    if masked[0] & !top_mask != 0 {
        return false;
    }
    let mut db = masked.to_vec();
    let mask = mgf1(cfg.mgf1, h, db.len());
    xor_into(&mut db, &mask);
    db[0] &= top_mask;
    let Some(one) = db.iter().position(|&b| b != 0) else { return false };
    if db[one] != 1 {
        return false;
    }
    let salt = &db[one + 1..];
    let expected_salt = match cfg.salt {
        SALT_MAX_OR_AUTO => salt.len(),
        SALT_DIGEST => h_len,
        s => s as usize,
    };
    if salt.len() != expected_salt {
        return false;
    }
    let mut m = vec![0u8; 8];
    m.extend_from_slice(mhash);
    m.extend_from_slice(salt);
    hash::digest(cfg.hash, &m) == h
}

fn rsa_default_padding(k: &RsaKey) -> i32 {
    if k.pss.is_some() {
        RSA_PKCS1_PSS_PADDING
    } else {
        RSA_PKCS1_PADDING
    }
}

fn rsa_sign(k: &RsaKey, hash: Option<&str>, padding: Option<i32>, salt: Option<i32>, data: &[u8]) -> Result<Vec<u8>, OpError> {
    let padding = padding.unwrap_or_else(|| rsa_default_padding(k));
    let k_len = mod_len(k);
    let em = match padding {
        RSA_PKCS1_PADDING if k.pss.is_none() => {
            let algo = hash.map(digest_algo).transpose()?.unwrap_or(Algo::Sha256);
            let t = digest_info(algo, &hash::digest(algo, data))?;
            if k_len < t.len() + 11 {
                return Err(ossl("02000070", "rsa routines", "digest too big for rsa key", "ERR_OSSL_RSA_DIGEST_TOO_BIG_FOR_RSA_KEY"));
            }
            let mut em = vec![0x00, 0x01];
            em.resize(k_len - t.len() - 1, 0xff);
            em.push(0);
            em.extend_from_slice(&t);
            em
        }
        RSA_PKCS1_PSS_PADDING => {
            let cfg = pss_config(k, hash, salt)?;
            let em_bits = k.n.bits() - 1;
            let em_len = em_bits.div_ceil(8);
            let max = em_len.saturating_sub(cfg.hash.out_len() + 2);
            let salt_len = match cfg.salt {
                SALT_MAX_OR_AUTO => max,
                SALT_DIGEST => cfg.hash.out_len(),
                s if s >= 0 => s as usize,
                _ => return Err(invalid_salt()),
            };
            pss_encode(em_bits, &hash::digest(cfg.hash, data), &cfg, salt_len)?
        }
        _ => return Err(illegal_padding()),
    };
    let s = rsa_private_op(k, &BigUint::from_bytes_be(&em))?;
    Ok(asn1::pad_be(&s.to_bytes_be(), k_len))
}

fn rsa_verify(k: &RsaKey, hash: Option<&str>, padding: Option<i32>, salt: Option<i32>, data: &[u8], sig: &[u8]) -> Result<bool, OpError> {
    let padding = padding.unwrap_or_else(|| rsa_default_padding(k));
    let k_len = mod_len(k);
    match padding {
        RSA_PKCS1_PADDING | RSA_PKCS1_PSS_PADDING => {}
        _ => return Err(illegal_padding()),
    }
    if padding == RSA_PKCS1_PADDING && k.pss.is_some() {
        return Err(illegal_padding());
    }
    let cfg = if padding == RSA_PKCS1_PSS_PADDING { Some(pss_config(k, hash, salt)?) } else { None };
    let pkcs_algo = match &cfg {
        None => Some(hash.map(digest_algo).transpose()?.unwrap_or(Algo::Sha256)),
        Some(_) => None,
    };
    if sig.len() != k_len {
        return Ok(false);
    }
    let s = BigUint::from_bytes_be(sig);
    if s >= k.n {
        return Ok(false);
    }
    let m = rsa_public_op(k, &s);
    match (cfg, pkcs_algo) {
        (Some(cfg), _) => {
            let em_bits = k.n.bits() - 1;
            let em_len = em_bits.div_ceil(8);
            let mb = m.to_bytes_be();
            if mb.len() > em_len {
                return Ok(false);
            }
            let em = asn1::pad_be(&mb, em_len);
            Ok(pss_verify(em_bits, &em, &hash::digest(cfg.hash, data), &cfg))
        }
        (None, Some(algo)) => {
            let t = digest_info(algo, &hash::digest(algo, data))?;
            if k_len < t.len() + 11 {
                return Ok(false);
            }
            let mut expected = vec![0x00, 0x01];
            expected.resize(k_len - t.len() - 1, 0xff);
            expected.push(0);
            expected.extend_from_slice(&t);
            Ok(asn1::pad_be(&m.to_bytes_be(), k_len) == expected)
        }
        _ => Ok(false),
    }
}

fn der_to_rs(der: &[u8], len: usize) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut outer = asn1::Reader::new(der);
    let mut seq = outer.sequence()?;
    outer.finish()?;
    let r = seq.uint()?;
    let s = seq.uint()?;
    seq.finish()?;
    if r.len() > len || s.len() > len {
        return None;
    }
    Some((asn1::pad_be(r, len), asn1::pad_be(s, len)))
}

fn rs_to_der(r: &[u8], s: &[u8]) -> Vec<u8> {
    asn1::seq(&[&asn1::uint(r), &asn1::uint(s)])
}

fn ec_prehash(hash: &[u8], field_len: usize) -> Vec<u8> {
    if hash.len() >= field_len {
        return hash.to_vec();
    }
    let mut out = vec![0u8; field_len - hash.len()];
    out.extend_from_slice(hash);
    out
}

fn ecdsa_sign(k: &EcKey, hashed: &[u8], p1363: bool) -> Result<Vec<u8>, OpError> {
    let d = k.d.as_ref().ok_or_else(sign_failed)?;
    let flen = k.curve.field_len();
    let prehash = ec_prehash(hashed, flen);
    let scalar = asn1::pad_be(d, flen);
    macro_rules! sign_with {
        ($C:ty) => {{
            let sk = ecdsa::SigningKey::<$C>::from_slice(&scalar).map_err(|_| sign_failed())?;
            let sig: ecdsa::Signature<$C> = sk.sign_prehash(&prehash).map_err(|_| sign_failed())?;
            sig.to_bytes().to_vec()
        }};
    }
    let rs = match k.curve {
        EcCurve::P256 => sign_with!(p256::NistP256),
        EcCurve::P384 => sign_with!(p384::NistP384),
        EcCurve::Secp256k1 => sign_with!(k256::Secp256k1),
        EcCurve::P521 => {
            let sk = p521::ecdsa::SigningKey::from_slice(&scalar).map_err(|_| sign_failed())?;
            let sig = sk.sign_prehash_with_rng(&mut OsRng, &prehash).map_err(|_| sign_failed())?;
            sig.to_bytes().to_vec()
        }
    };
    if p1363 {
        Ok(rs)
    } else {
        Ok(rs_to_der(&rs[..flen], &rs[flen..]))
    }
}

fn ecdsa_verify(k: &EcKey, hashed: &[u8], sig: &[u8], p1363: bool) -> bool {
    let flen = k.curve.field_len();
    let rs = if p1363 {
        if sig.len() != 2 * flen {
            return false;
        }
        sig.to_vec()
    } else {
        match der_to_rs(sig, flen) {
            Some((r, s)) => [r, s].concat(),
            None => return false,
        }
    };
    let prehash = ec_prehash(hashed, flen);
    with_ec_curve!(k.curve, C => {
        let Ok(vk) = ecdsa::VerifyingKey::<C>::from_sec1_bytes(&k.point) else { return false };
        let Ok(sig) = ecdsa::Signature::<C>::from_slice(&rs) else { return false };
        let sig = sig.normalize_s().unwrap_or(sig);
        vk.verify_prehash(&prehash, &sig).is_ok()
    })
}

fn dsa_sign(k: &super::keys::DsaKey, hashed: &[u8], p1363: bool) -> Result<Vec<u8>, OpError> {
    use signature::SignatureEncoding;
    let sk = k.dsa_signing()?;
    let sig = sk.sign_prehash_with_rng(&mut OsRng, hashed).map_err(|_| sign_failed())?;
    if p1363 {
        let len = k.q.bits().div_ceil(8);
        Ok([asn1::pad_be(&sig.r().to_bytes_be(), len), asn1::pad_be(&sig.s().to_bytes_be(), len)].concat())
    } else {
        Ok(sig.to_bytes().to_vec())
    }
}

fn dsa_verify(k: &super::keys::DsaKey, hashed: &[u8], sig: &[u8], p1363: bool) -> bool {
    let Ok(vk) = k.dsa_verifying() else { return false };
    let len = k.q.bits().div_ceil(8);
    let (r, s) = if p1363 {
        if sig.len() != 2 * len {
            return false;
        }
        (BigUint::from_bytes_be(&sig[..len]), BigUint::from_bytes_be(&sig[len..]))
    } else {
        let mut outer = asn1::Reader::new(sig);
        let Some(mut seq) = outer.sequence() else { return false };
        if outer.finish().is_none() {
            return false;
        }
        let (Some(r), Some(s)) = (seq.biguint(), seq.biguint()) else { return false };
        (r, s)
    };
    let Ok(sig) = dsa::Signature::from_components(r, s) else { return false };
    vk.verify_prehash(hashed, &sig).is_ok()
}

fn default_hash(hash: Option<&str>) -> Result<Algo, OpError> {
    hash.map(digest_algo).transpose().map(|h| h.unwrap_or(Algo::Sha256))
}

fn sign_with(key: &AsymKey, hash: Option<&str>, padding: Option<i32>, salt: Option<i32>, p1363: bool, data: &[u8]) -> Result<Vec<u8>, OpError> {
    match key {
        AsymKey::Rsa(k) => rsa_sign(k, hash, padding, salt, data),
        AsymKey::Ec(k) => {
            let algo = default_hash(hash)?;
            ecdsa_sign(k, &hash::digest(algo, data), p1363)
        }
        AsymKey::Dsa(k) => {
            let algo = default_hash(hash)?;
            dsa_sign(k, &hash::digest(algo, data), p1363)
        }
        AsymKey::Ed25519(k) => {
            if hash.is_some() {
                return Err(invalid_digest());
            }
            Ok(k.ed25519_signing()?.sign(data).to_bytes().to_vec())
        }
        AsymKey::Ed448(k) => {
            if hash.is_some() {
                return Err(invalid_digest());
            }
            let sig = k.ed448_signing()?.try_sign(data).map_err(|_| sign_failed())?;
            Ok(sig.to_bytes().to_vec())
        }
        _ => Err(unsupported_keytype()),
    }
}

fn verify_with(key: &AsymKey, hash: Option<&str>, padding: Option<i32>, salt: Option<i32>, p1363: bool, data: &[u8], sig: &[u8]) -> Result<bool, OpError> {
    match key {
        AsymKey::Rsa(k) => rsa_verify(k, hash, padding, salt, data, sig),
        AsymKey::Ec(k) => {
            let algo = default_hash(hash)?;
            Ok(ecdsa_verify(k, &hash::digest(algo, data), sig, p1363))
        }
        AsymKey::Dsa(k) => {
            let algo = default_hash(hash)?;
            Ok(dsa_verify(k, &hash::digest(algo, data), sig, p1363))
        }
        AsymKey::Ed25519(k) => {
            if hash.is_some() {
                return Err(invalid_digest());
            }
            let Ok(sig) = ed25519_dalek::Signature::from_slice(sig) else { return Ok(false) };
            Ok(k.ed25519_verifying()?.verify_strict(data, &sig).is_ok())
        }
        AsymKey::Ed448(k) => {
            if hash.is_some() {
                return Err(invalid_digest());
            }
            let Ok(sig) = ed448_goldilocks_plus::Signature::try_from(sig) else { return Ok(false) };
            Ok(k.ed448_verifying()?.verify_raw(&sig, data).is_ok())
        }
        _ => Err(unsupported_keytype()),
    }
}

/// `sign` of `Sign` / `crypto.sign`: the signature of `data` (DER, or `r || s` for `p1363`).
#[lumen::op(name = "sigSign")]
fn sig_sign(kind: u32, der: &[u8], hash: Option<String>, padding: Option<i32>, salt: Option<i32>, p1363: bool, data: &[u8]) -> Result<Vec<u8>, OpError> {
    let key = AsymKey::from_handle(kind, der)?;
    sign_with(&key, hash.as_deref(), padding, salt, p1363, data)
}

#[lumen::op(name = "sigVerify")]
fn sig_verify(kind: u32, der: &[u8], hash: Option<String>, padding: Option<i32>, salt: Option<i32>, p1363: bool, data: &[u8], sig: &[u8]) -> Result<bool, OpError> {
    let key = AsymKey::from_handle(kind, der)?;
    verify_with(&key, hash.as_deref(), padding, salt, p1363, data, sig)
}

fn oaep_hash(name: Option<&str>) -> Result<Algo, OpError> {
    match name {
        None => Ok(Algo::Sha1),
        Some(n) => Algo::from_name(n)
            .filter(|a| !a.is_xof())
            .ok_or_else(|| OpError::error("Digest method not supported").with_code("ERR_OSSL_EVP_UNSUPPORTED")),
    }
}

fn oaep_encrypt(k: &RsaKey, algo: Algo, label: &[u8], msg: &[u8]) -> Result<Vec<u8>, OpError> {
    let k_len = mod_len(k);
    let h_len = algo.out_len();
    if msg.len() + 2 * h_len + 2 > k_len {
        return Err(data_too_large());
    }
    let mut db = hash::digest(algo, label);
    db.resize(k_len - msg.len() - h_len - 2, 0);
    db.push(1);
    db.extend_from_slice(msg);
    let mut seed = vec![0u8; h_len];
    OsRng.fill_bytes(&mut seed);
    let db_mask = mgf1(algo, &seed, db.len());
    xor_into(&mut db, &db_mask);
    let seed_mask = mgf1(algo, &db, h_len);
    xor_into(&mut seed, &seed_mask);
    let mut em = vec![0u8];
    em.extend(seed);
    em.extend(db);
    let c = rsa_run(k, false, &BigUint::from_bytes_be(&em))?;
    Ok(asn1::pad_be(&c.to_bytes_be(), k_len))
}

fn oaep_decrypt(k: &RsaKey, algo: Algo, label: &[u8], input: &[u8]) -> Result<Vec<u8>, OpError> {
    let k_len = mod_len(k);
    let h_len = algo.out_len();
    if input.len() > k_len {
        return Err(data_too_large());
    }
    if input.len() < k_len {
        return Err(data_too_small());
    }
    if k_len < 2 * h_len + 2 {
        return Err(oaep_decoding());
    }
    let m = rsa_run(k, true, &BigUint::from_bytes_be(input))?;
    let em = asn1::pad_be(&m.to_bytes_be(), k_len);
    let (y, rest) = (em[0], &em[1..]);
    let mut seed = rest[..h_len].to_vec();
    let mut db = rest[h_len..].to_vec();
    xor_into(&mut seed, &mgf1(algo, &db, h_len));
    let mask = mgf1(algo, &seed, db.len());
    xor_into(&mut db, &mask);
    let l_hash = hash::digest(algo, label);
    let tail = &db[h_len..];
    match tail.iter().position(|&b| b != 0) {
        Some(i) if tail[i] == 1 && y == 0 && db[..h_len] == l_hash[..] => Ok(tail[i + 1..].to_vec()),
        _ => Err(oaep_decoding()),
    }
}

fn pkcs1_encrypt(k: &RsaKey, private: bool, msg: &[u8]) -> Result<Vec<u8>, OpError> {
    let k_len = mod_len(k);
    if msg.len() + 11 > k_len {
        return Err(data_too_large());
    }
    let ps_len = k_len - msg.len() - 3;
    let mut em = vec![0x00, if private { 0x01 } else { 0x02 }];
    if private {
        em.resize(2 + ps_len, 0xff);
    } else {
        em.extend(random_nonzero(ps_len));
    }
    em.push(0);
    em.extend_from_slice(msg);
    let c = rsa_run(k, private, &BigUint::from_bytes_be(&em))?;
    Ok(asn1::pad_be(&c.to_bytes_be(), k_len))
}

fn pkcs1_decrypt(k: &RsaKey, private: bool, input: &[u8]) -> Result<Vec<u8>, OpError> {
    let k_len = mod_len(k);
    if input.len() > k_len {
        return Err(data_too_large());
    }
    if input.len() < k_len {
        return Err(data_too_small());
    }
    let m = rsa_run(k, private, &BigUint::from_bytes_be(input))?;
    let em = asn1::pad_be(&m.to_bytes_be(), k_len);
    let block_type = if private { 2 } else { 1 };
    if em[0] != 0 || em[1] != block_type {
        return Err(padding_check_failed());
    }
    let body = &em[2..];
    let sep = body.iter().position(|&b| b == 0).ok_or_else(padding_check_failed)?;
    if sep < 8 || (!private && body[..sep].iter().any(|&b| b != 0xff)) {
        return Err(padding_check_failed());
    }
    Ok(body[sep + 1..].to_vec())
}

fn raw_transform(k: &RsaKey, private: bool, input: &[u8]) -> Result<Vec<u8>, OpError> {
    let k_len = mod_len(k);
    if input.len() > k_len {
        return Err(data_too_large());
    }
    if input.len() < k_len {
        return Err(data_too_small());
    }
    let out = rsa_run(k, private, &BigUint::from_bytes_be(input))?;
    Ok(asn1::pad_be(&out.to_bytes_be(), k_len))
}

/// `publicEncrypt` (0), `privateDecrypt` (1), `privateEncrypt` (2) and `publicDecrypt` (3).
#[lumen::op(name = "rsaCipher")]
fn rsa_cipher(
    operation: u32,
    kind: u32,
    der: &[u8],
    input: &[u8],
    padding: i32,
    oaep: Option<String>,
    label: Option<Vec<u8>>,
) -> Result<Vec<u8>, OpError> {
    let key = AsymKey::from_handle(kind, der)?;
    let AsymKey::Rsa(k) = &key else { return Err(unsupported_keytype()) };
    let private_key_op = matches!(operation, 1 | 2);
    if private_key_op && k.private.is_none() {
        return Err(OpError::error("key is not a private key"));
    }
    match (operation, padding) {
        (0, RSA_PKCS1_OAEP_PADDING) => oaep_encrypt(k, oaep_hash(oaep.as_deref())?, label.as_deref().unwrap_or(&[]), input),
        (1, RSA_PKCS1_OAEP_PADDING) => oaep_decrypt(k, oaep_hash(oaep.as_deref())?, label.as_deref().unwrap_or(&[]), input),
        (0, RSA_PKCS1_PADDING) => pkcs1_encrypt(k, false, input),
        (1, RSA_PKCS1_PADDING) => pkcs1_decrypt(k, true, input),
        (2, RSA_PKCS1_PADDING) => pkcs1_encrypt(k, true, input),
        (3, RSA_PKCS1_PADDING) => pkcs1_decrypt(k, false, input),
        (_, RSA_NO_PADDING) => raw_transform(k, private_key_op, input),
        _ => Err(illegal_padding()),
    }
}

pub const OPS: &[&OpDesc] = lumen::ops![sig_sign, sig_verify, rsa_cipher];
