//! Signatures (RSA PKCS#1 v1.5 and PSS, DSA, ECDSA, Ed25519, Ed448) and RSA encryption padding,
//! all over RustCrypto. The `rsa` crate's paddings are generic over the digest, so `node:crypto`'s
//! digest names reach them through `AnyDigest`. RSA private-key operations are blinded.

use digest::{DynDigest, InvalidBufferSize};
use lumen::embed::OpError;
use num_bigint_dig::BigUint;
use rsa::traits::PublicKeyParts;
use rsa::{Oaep, Pkcs1v15Encrypt, Pkcs1v15Sign, Pss, RsaPrivateKey, RsaPublicKey};
use signature::hazmat::{PrehashSigner, PrehashVerifier, RandomizedPrehashSigner};
use signature::Signer;

use super::rng::SysRng;
use super::keys::{asn1, with_ec_curve, AsymKey, EcCurve, EcKey, PssParams, RsaKey};
use crate::hash::{self, Algo};


#[lumen_bind::module(name = "crypto")]
pub(crate) mod bindings {
use super::*;

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

/// A fixed-output digest of `node:crypto`'s repertoire as the `DynDigest` the `rsa` paddings are
/// generic over.
#[derive(Clone)]
struct AnyDigest {
    fresh: hash::Hasher,
    state: hash::Hasher,
}

impl AnyDigest {
    fn new(algo: Algo) -> Result<AnyDigest, OpError> {
        if algo.is_xof() || algo == Algo::Md5Sha1 {
            return Err(invalid_digest());
        }
        let fresh = hash::Hasher::new(algo);
        Ok(AnyDigest { state: fresh.clone(), fresh })
    }
}

fn copy_digest(h: hash::Hasher, out: &mut [u8]) -> Result<(), InvalidBufferSize> {
    let digest = h.finish();
    if digest.len() != out.len() {
        return Err(InvalidBufferSize);
    }
    out.copy_from_slice(&digest);
    Ok(())
}

impl DynDigest for AnyDigest {
    fn update(&mut self, data: &[u8]) {
        self.state.update(data);
    }

    fn finalize_into(self, out: &mut [u8]) -> Result<(), InvalidBufferSize> {
        copy_digest(self.state, out)
    }

    fn finalize_into_reset(&mut self, out: &mut [u8]) -> Result<(), InvalidBufferSize> {
        copy_digest(std::mem::replace(&mut self.state, self.fresh.clone()), out)
    }

    fn reset(&mut self) {
        self.state = self.fresh.clone();
    }

    fn output_size(&self) -> usize {
        self.fresh.algo().out_len()
    }

    fn box_clone(&self) -> Box<dyn DynDigest> {
        Box::new(self.clone())
    }
}

/// The OAEP label hash, precomputed: `Oaep` takes its label as a `String` but Node's is a byte
/// string, so the label goes in here instead and the padding just asks for the (empty-label) hash.
#[derive(Clone)]
struct LabelHash(Vec<u8>);

impl LabelHash {
    fn write(&self, out: &mut [u8]) -> Result<(), InvalidBufferSize> {
        if out.len() != self.0.len() {
            return Err(InvalidBufferSize);
        }
        out.copy_from_slice(&self.0);
        Ok(())
    }
}

impl DynDigest for LabelHash {
    fn update(&mut self, _data: &[u8]) {}

    fn finalize_into(self, out: &mut [u8]) -> Result<(), InvalidBufferSize> {
        self.write(out)
    }

    fn finalize_into_reset(&mut self, out: &mut [u8]) -> Result<(), InvalidBufferSize> {
        self.write(out)
    }

    fn reset(&mut self) {}

    fn output_size(&self) -> usize {
        self.0.len()
    }

    fn box_clone(&self) -> Box<dyn DynDigest> {
        Box::new(self.clone())
    }
}

fn mod_len(k: &RsaKey) -> usize {
    k.n.bits().div_ceil(8)
}

fn public_key(k: &RsaKey) -> Result<RsaPublicKey, OpError> {
    Ok(k.rsa_public()?)
}

fn private_key(k: &RsaKey) -> Result<RsaPrivateKey, OpError> {
    if k.private.is_none() {
        return Err(OpError::error("key is not a private key"));
    }
    Ok(k.rsa_private()?)
}

fn too_large_for_modulus() -> OpError {
    ossl("02000084", "rsa routines", "data too large for modulus", "ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_MODULUS")
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

fn pss(cfg: &PssConfig, salt_len: usize) -> Result<Pss, OpError> {
    Ok(Pss { blinded: true, digest: Box::new(AnyDigest::new(cfg.hash)?), salt_len })
}

fn mgf1_xor(algo: Algo, seed: &[u8], data: &mut [u8]) {
    for (counter, chunk) in data.chunks_mut(algo.out_len()).enumerate() {
        let mut block = seed.to_vec();
        block.extend_from_slice(&(counter as u32).to_be_bytes());
        for (d, m) in chunk.iter_mut().zip(hash::digest(algo, &block)) {
            *d ^= m;
        }
    }
}

/// RSASSA-PSS with an MGF1 digest other than the message digest, which the `rsa` crate's `Pss`
/// cannot express. The encoding only touches public values; the private-key operation is the
/// crate's blinded one.
fn pss_split_sign(k: &RsaKey, key: &RsaPrivateKey, cfg: &PssConfig, mhash: &[u8], salt_len: usize) -> Result<Vec<u8>, OpError> {
    let em_bits = k.n.bits() - 1;
    let em_len = em_bits.div_ceil(8);
    let h_len = cfg.hash.out_len();
    let mut salt = vec![0u8; salt_len];
    lumen_os::proc::entropy(&mut salt).map_err(|_| sign_failed())?;
    let h = hash::digest(cfg.hash, &[&[0u8; 8], mhash, &salt].concat());
    let mut db = vec![0u8; em_len - h_len - 1];
    let one = db.len() - salt_len - 1;
    db[one] = 1;
    db[one + 1..].copy_from_slice(&salt);
    mgf1_xor(cfg.mgf1, &h, &mut db);
    db[0] &= 0xff >> (8 * em_len - em_bits);
    let em = [db, h, vec![0xbc]].concat();
    let s = rsa::hazmat::rsa_decrypt_and_check(key, Some(&mut SysRng), &BigUint::from_bytes_be(&em)).map_err(|_| sign_failed())?;
    Ok(asn1::pad_be(&s.to_bytes_be(), mod_len(k)))
}

/// Verification counterpart of [`pss_split_sign`]; every value involved is public.
fn pss_split_verify(k: &RsaKey, key: &RsaPublicKey, sig: &[u8], cfg: &PssConfig, mhash: &[u8], salt_len: usize) -> bool {
    let em_bits = k.n.bits() - 1;
    let em_len = em_bits.div_ceil(8);
    let h_len = cfg.hash.out_len();
    let Ok(m) = rsa::hazmat::rsa_encrypt(key, &BigUint::from_bytes_be(sig)) else { return false };
    let m = m.to_bytes_be();
    if m.len() > em_len || em_len < h_len + salt_len + 2 {
        return false;
    }
    let em = asn1::pad_be(&m, em_len);
    if em[em_len - 1] != 0xbc {
        return false;
    }
    let (masked, rest) = em.split_at(em_len - h_len - 1);
    let h = &rest[..h_len];
    let top = 0xffu8 >> (8 * em_len - em_bits);
    if masked[0] & !top != 0 {
        return false;
    }
    let mut db = masked.to_vec();
    mgf1_xor(cfg.mgf1, h, &mut db);
    db[0] &= top;
    let ps = db.len() - salt_len - 1;
    if db[..ps].iter().any(|&b| b != 0) || db[ps] != 1 {
        return false;
    }
    hash::constant_time_eq(&hash::digest(cfg.hash, &[&[0u8; 8], mhash, &db[ps + 1..]].concat()), h)
}

fn pss_em_len(k: &RsaKey) -> usize {
    (k.n.bits() - 1).div_ceil(8)
}

/// RSASSA-PSS verification over `hash` (also the MGF1 digest) with a fixed salt length (X.509 signatures).
pub(crate) fn rsa_pss_verify(n: &[u8], e: &[u8], hash: Algo, salt: usize, data: &[u8], sig: &[u8]) -> bool {
    let Ok(key) = RsaPublicKey::new_with_max_size(BigUint::from_bytes_be(n), BigUint::from_bytes_be(e), 16384) else {
        return false;
    };
    let Ok(digest) = AnyDigest::new(hash) else { return false };
    let scheme = Pss { blinded: false, digest: Box::new(digest), salt_len: salt };
    sig.len() == key.size() && key.verify(scheme, &hash::digest(hash, data), sig).is_ok()
}

fn rsa_default_padding(k: &RsaKey) -> i32 {
    if k.pss.is_some() {
        RSA_PKCS1_PSS_PADDING
    } else {
        RSA_PKCS1_PADDING
    }
}

/// The PKCS#1 v1.5 scheme over `algo`: the `DigestInfo` header is the prefix of the hash.
fn pkcs1_sign_scheme(algo: Algo, hashed: &[u8]) -> Result<Pkcs1v15Sign, OpError> {
    let info = digest_info(algo, hashed)?;
    Ok(Pkcs1v15Sign { hash_len: Some(hashed.len()), prefix: info[..info.len() - hashed.len()].into() })
}

fn rsa_sign(k: &RsaKey, hash: Option<&str>, padding: Option<i32>, salt: Option<i32>, data: &[u8]) -> Result<Vec<u8>, OpError> {
    let padding = padding.unwrap_or_else(|| rsa_default_padding(k));
    match padding {
        RSA_PKCS1_PADDING if k.pss.is_none() => {
            let algo = hash.map(digest_algo).transpose()?.unwrap_or(Algo::Sha256);
            let hashed = hash::digest(algo, data);
            let scheme = pkcs1_sign_scheme(algo, &hashed)?;
            if mod_len(k) < scheme.prefix.len() + hashed.len() + 11 {
                return Err(ossl("02000070", "rsa routines", "digest too big for rsa key", "ERR_OSSL_RSA_DIGEST_TOO_BIG_FOR_RSA_KEY"));
            }
            private_key(k)?.sign_with_rng(&mut SysRng, scheme, &hashed).map_err(|_| sign_failed())
        }
        RSA_PKCS1_PSS_PADDING => {
            let cfg = pss_config(k, hash, salt)?;
            let h_len = cfg.hash.out_len();
            let max = pss_em_len(k).saturating_sub(h_len + 2);
            let salt_len = match cfg.salt {
                SALT_MAX_OR_AUTO => max,
                SALT_DIGEST => h_len,
                s if s >= 0 => s as usize,
                _ => return Err(invalid_salt()),
            };
            if salt_len > max {
                return Err(data_too_large());
            }
            let key = private_key(k)?;
            let mhash = hash::digest(cfg.hash, data);
            if cfg.hash != cfg.mgf1 {
                return pss_split_sign(k, &key, &cfg, &mhash, salt_len);
            }
            key.sign_with_rng(&mut SysRng, pss(&cfg, salt_len)?, &mhash).map_err(|_| sign_failed())
        }
        _ => Err(illegal_padding()),
    }
}

fn rsa_verify(k: &RsaKey, hash: Option<&str>, padding: Option<i32>, salt: Option<i32>, data: &[u8], sig: &[u8]) -> Result<bool, OpError> {
    let padding = padding.unwrap_or_else(|| rsa_default_padding(k));
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
    let key = public_key(k)?;
    if sig.len() != mod_len(k) || BigUint::from_bytes_be(sig) >= k.n {
        return Ok(false);
    }
    match (cfg, pkcs_algo) {
        (Some(cfg), _) => {
            let mhash = hash::digest(cfg.hash, data);
            let h_len = cfg.hash.out_len();
            let max = pss_em_len(k).saturating_sub(h_len + 2);
            let verify_with = |salt_len: usize| -> Result<bool, OpError> {
                if cfg.hash != cfg.mgf1 {
                    return Ok(pss_split_verify(k, &key, sig, &cfg, &mhash, salt_len));
                }
                Ok(key.verify(pss(&cfg, salt_len)?, &mhash, sig).is_ok())
            };
            match cfg.salt {
                SALT_MAX_OR_AUTO => {
                    // The salt length is not known; the digest-length salt is by far the most common.
                    for salt_len in std::iter::once(h_len).chain((0..=max).rev()).filter(|&s| s <= max) {
                        if verify_with(salt_len)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
                SALT_DIGEST => verify_with(h_len),
                s if s >= 0 => verify_with(s as usize),
                _ => Ok(false),
            }
        }
        (None, Some(algo)) => {
            let hashed = hash::digest(algo, data);
            Ok(key.verify(pkcs1_sign_scheme(algo, &hashed)?, &hashed, sig).is_ok())
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
            let sig = sk.sign_prehash_with_rng(&mut SysRng, &prehash).map_err(|_| sign_failed())?;
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

fn dsa_sign(k: &crate::crypto::keys::DsaKey, hashed: &[u8], p1363: bool) -> Result<Vec<u8>, OpError> {
    use signature::SignatureEncoding;
    let sk = k.dsa_signing()?;
    let sig = sk.sign_prehash_with_rng(&mut SysRng, hashed).map_err(|_| sign_failed())?;
    if p1363 {
        let len = k.q.bits().div_ceil(8);
        Ok([asn1::pad_be(&sig.r().to_bytes_be(), len), asn1::pad_be(&sig.s().to_bytes_be(), len)].concat())
    } else {
        Ok(sig.to_bytes().to_vec())
    }
}

fn dsa_verify(k: &crate::crypto::keys::DsaKey, hashed: &[u8], sig: &[u8], p1363: bool) -> bool {
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
#[op(name = "sigSign")]
fn sig_sign(kind: u32, der: &[u8], hash: Option<String>, padding: Option<i32>, salt: Option<i32>, p1363: bool, data: &[u8]) -> Result<Vec<u8>, OpError> {
    let key = AsymKey::from_handle(kind, der)?;
    sign_with(&key, hash.as_deref(), padding, salt, p1363, data)
}

#[op(name = "sigVerify")]
fn sig_verify(kind: u32, der: &[u8], hash: Option<String>, padding: Option<i32>, salt: Option<i32>, p1363: bool, data: &[u8], sig: &[u8]) -> Result<bool, OpError> {
    let key = AsymKey::from_handle(kind, der)?;
    verify_with(&key, hash.as_deref(), padding, salt, p1363, data, sig)
}

fn oaep_hash(name: Option<&str>) -> Result<Algo, OpError> {
    match name {
        None => Ok(Algo::Sha1),
        Some(n) => Algo::from_name(n)
            .filter(|a| !a.is_xof() && *a != Algo::Md5Sha1)
            .ok_or_else(|| OpError::error("Digest method not supported").with_code("ERR_OSSL_EVP_UNSUPPORTED")),
    }
}

fn oaep_scheme(algo: Algo, label: &[u8]) -> Result<Oaep, OpError> {
    Ok(Oaep {
        digest: Box::new(LabelHash(hash::digest(algo, label))),
        mgf_digest: Box::new(AnyDigest::new(algo)?),
        label: None,
    })
}

/// Checks the input length against the modulus the way OpenSSL reports it; returns the modulus length.
fn check_block_len(k: &RsaKey, input: &[u8]) -> Result<usize, OpError> {
    let k_len = mod_len(k);
    if input.len() > k_len {
        return Err(data_too_large());
    }
    if input.len() < k_len {
        return Err(data_too_small());
    }
    if BigUint::from_bytes_be(input) >= k.n {
        return Err(too_large_for_modulus());
    }
    Ok(k_len)
}

fn oaep_encrypt(k: &RsaKey, algo: Algo, label: &[u8], msg: &[u8]) -> Result<Vec<u8>, OpError> {
    if msg.len() + 2 * algo.out_len() + 2 > mod_len(k) {
        return Err(data_too_large());
    }
    public_key(k)?.encrypt(&mut SysRng, oaep_scheme(algo, label)?, msg).map_err(|_| oaep_decoding())
}

fn oaep_decrypt(k: &RsaKey, algo: Algo, label: &[u8], input: &[u8]) -> Result<Vec<u8>, OpError> {
    check_block_len(k, input)?;
    // Every failure of the padding checks surfaces as the same error.
    private_key(k)?.decrypt_blinded(&mut SysRng, oaep_scheme(algo, label)?, input).map_err(|_| oaep_decoding())
}

fn pkcs1_encrypt(k: &RsaKey, msg: &[u8]) -> Result<Vec<u8>, OpError> {
    if msg.len() + 11 > mod_len(k) {
        return Err(data_too_large());
    }
    public_key(k)?.encrypt(&mut SysRng, Pkcs1v15Encrypt, msg).map_err(|_| padding_check_failed())
}

fn pkcs1_decrypt(k: &RsaKey, input: &[u8]) -> Result<Vec<u8>, OpError> {
    check_block_len(k, input)?;
    private_key(k)?.decrypt_blinded(&mut SysRng, Pkcs1v15Encrypt, input).map_err(|_| padding_check_failed())
}

/// `privateEncrypt`: the block-type-1 padded message under the private key (a signature without
/// `DigestInfo`).
fn pkcs1_private_encrypt(k: &RsaKey, msg: &[u8]) -> Result<Vec<u8>, OpError> {
    if msg.len() + 11 > mod_len(k) {
        return Err(data_too_large());
    }
    private_key(k)?.sign_with_rng(&mut SysRng, Pkcs1v15Sign::new_unprefixed(), msg).map_err(|_| sign_failed())
}

/// `publicDecrypt`: recovers the message of a block-type-1 padded signature. Everything involved
/// is public, so the unpadding needs no constant-time care.
fn pkcs1_public_decrypt(k: &RsaKey, input: &[u8]) -> Result<Vec<u8>, OpError> {
    let k_len = check_block_len(k, input)?;
    let m = rsa::hazmat::rsa_encrypt(&public_key(k)?, &BigUint::from_bytes_be(input)).map_err(|_| padding_check_failed())?;
    let em = asn1::pad_be(&m.to_bytes_be(), k_len);
    if em[0] != 0 || em[1] != 1 {
        return Err(padding_check_failed());
    }
    let body = &em[2..];
    let sep = body.iter().position(|&b| b != 0xff).ok_or_else(padding_check_failed)?;
    if sep < 8 || body[sep] != 0 {
        return Err(padding_check_failed());
    }
    Ok(body[sep + 1..].to_vec())
}

fn raw_transform(k: &RsaKey, private: bool, input: &[u8]) -> Result<Vec<u8>, OpError> {
    let k_len = check_block_len(k, input)?;
    let c = BigUint::from_bytes_be(input);
    let out = if private {
        rsa::hazmat::rsa_decrypt_and_check(&private_key(k)?, Some(&mut SysRng), &c).map_err(|_| sign_failed())?
    } else {
        rsa::hazmat::rsa_encrypt(&public_key(k)?, &c).map_err(|_| sign_failed())?
    };
    Ok(asn1::pad_be(&out.to_bytes_be(), k_len))
}

/// `publicEncrypt` (0), `privateDecrypt` (1), `privateEncrypt` (2) and `publicDecrypt` (3).
#[op(name = "rsaCipher")]
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
        (0, RSA_PKCS1_PADDING) => pkcs1_encrypt(k, input),
        (1, RSA_PKCS1_PADDING) => pkcs1_decrypt(k, input),
        (2, RSA_PKCS1_PADDING) => pkcs1_private_encrypt(k, input),
        (3, RSA_PKCS1_PADDING) => pkcs1_public_decrypt(k, input),
        (_, RSA_NO_PADDING) => raw_transform(k, private_key_op, input),
        _ => Err(illegal_padding()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    // 1024-bit key and the vectors below were produced by `openssl` 3 (genpkey, dgst -sign,
    // pkeyutl -encrypt / -sign) over MSG and PLAIN.
    const KEY: &str = "MIICdQIBADANBgkqhkiG9w0BAQEFAASCAl8wggJbAgEAAoGBAJtNDLz0UW46tFwXTJtyHEdrOhK2sf9U50jA1RmUmXL78rYSfjOHX7aTE6bAs+5nli6qXC2UhJokBGN6zHviQu/INLejVSD7Tp6nPMrvtkh82vl5IY6sLX3F5TqY7Rp7Bjf+yysg8drMe5LbjqMAJjdM5QtzlKIZNyDk8EUkWF/rAgMBAAECgYBoIfnwmUIgz2wwc88CTDl6CgQemDIyKxQKTIKXbHSYDShpvWyx0Iv1OBltLrl3mi2xjLnSNkvTr2Lh8W07hDOs2PN/hYX1/eRmnld1NPGCbJFG4eYnSiDwNz7jet5JUtm0IrwRzkQHmqJMxDIqg/OBY8aIfj70Ytz8BnVEiXLQgQJBAMvUhNGd1teAX64/H4Boelwmw70FXvjVLLDj1E9NqII7fhOJGosD1ksSu1CFgBdiGa7dw/ylxzKntIfgPq/ALSkCQQDDDMXxy3LxdWQZw+fbA9jbqST8NXxdH/U5FTDSWdcBHdQVwqrhZT64/3jZ2b8vnepPRE55XBTR3Bg4y+sPL7LzAkB42R+GSFbAnlQcM0CyGT+ysykKQMz2Ky28EtglzJ1D2ZH+cyNRmIzNJeX4763qLzea/dDdUkywM85NYR7JhN9BAkAvORl3mBVFJnHM1yR8XysSy5nbwitQ9JrPbjT6yKuIZqthdVcf6P5NlfSxccmbArWm6VfChCu6P3pRzfUkIR1HAkBuuZDNn0dfPCVg6JlOY4DRrSnz2rjq0QicshjzacDdGJQsroOz7pyKBXq4mnZTkp3GbiZCNOOA25aV2jMTMvj5";
    const MSG: &[u8] = b"lumen rsa vectors";
    const PLAIN: &[u8] = b"oaep secret";
    const PKCS1_SHA256: &str = "4684dcaf60d92662eba75673a2b818fd35da7aa04350add326c0ed80967fc8bdbeae98fb62411e8adaffd30f280f23cbc05b1d77ca68ffcf7246a16175eb156b24fa31af7a524aa9063528c3e9be026a41e36840d3dfa2b131d4162f9d8347615c5164a88badb8915585534d46d5b5f48e38dc2b2151bf739a98a5b00bd3ccf5";
    const PSS_SALT20: &str = "22a3b885c038fe97a2f6a75f0e8ffc77ad0e3cfe76f997d0d47ba3ab7c24f61943d19d7fb3e891be7c3618369a0725e040057b661320d698dbc62a84eac19aeb7dafc433890f8cd224d13fbccd6adbb79dd85514e7242e1ba6c62813db977763dc4a03a7f59f2d6092601fad1e1f64fa7086f4bbc5f14dec7e5bf90d0d3fb96d";
    const PSS_MGF1_SHA1: &str = "0c0323a4283101cd2d59917922e26234accd56f475bf20248017eee43376e82877cc288486a1c0cdbd864f6e89d1602fceb4b189dd283a25acdc6b6ad1ebc9e6d8e102d8ded6a950a513885eb2d4acdbea61f4887ed305b97c7c1e0efabde1164f8c6d830909ecce77622992bd2971d9454aab0dc6cd66ee750b145a199d218d";
    const OAEP_SHA256_LABEL: &str = "6ec8febf285d7c1e9caaa605ad27adae951ce69d5cbd892facf6cb3d7ba953c28ca0c355dd462f4fcd8b35a477ad8cfe84f7353c9ed04fb609c00b68418cce206de3f84f66bb4bc401f321b6d0ff764893030f6f67d492ca5fb284de022b7cdfc541bda0bb642b28284d08618054fbdcdcc7adadb6a381da402a4fa2c0e075d5";
    const PKCS1_ENCRYPTED: &str = "8773e3682e0aa4ad7925face33acea657ff41ab4f7b5ae9badbf90e26670f7b4afb3d7b4b51faba3d051b653b4fbac31d7c41dfc58262ceeadb1c0b0244229734b9a86ffe28f46dec17b918dacb3f4b80132750c3b25eb3f8ad31b200dbc6305d533a0b3b858dbd77f58e8497a3b8afe8dd8863e9c70b8a0911bb7cb3989ba7d";
    const PRIVATE_ENCRYPTED: &str = "8402076ded2788f9645b875445f0d7d696a540e20b69834f7b408a7e38867adc713dfcae5e82648ffd1451c483e8026f80127fd9b0a78321f3ede7e791919018a1b37e021be5c8a2e7d7ec9b90bf978f91fe8632fef63bf0d5399f4d101df2aeca5b01fb46e70db13e87d9c0f3f2a51383df2ad8be224449673df8667391f81b";

    fn key() -> RsaKey {
        let der = base64::engine::general_purpose::STANDARD.decode(KEY).unwrap();
        match AsymKey::from_pkcs8_der(&der).unwrap() {
            AsymKey::Rsa(k) => k,
            _ => unreachable!(),
        }
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn err_text(r: Result<Vec<u8>, OpError>) -> String {
        r.unwrap_err().to_string()
    }

    #[test]
    fn pkcs1_v15_signature_matches_openssl() {
        let k = key();
        assert_eq!(rsa_sign(&k, Some("sha256"), None, None, MSG).unwrap(), hex(PKCS1_SHA256));
        assert!(rsa_verify(&k, Some("sha256"), None, None, MSG, &hex(PKCS1_SHA256)).unwrap());
        assert!(!rsa_verify(&k, Some("sha256"), None, None, b"other", &hex(PKCS1_SHA256)).unwrap());
        assert!(!rsa_verify(&k, Some("sha512"), None, None, MSG, &hex(PKCS1_SHA256)).unwrap());
        let mut bad = hex(PKCS1_SHA256);
        bad[40] ^= 0x10;
        assert!(!rsa_verify(&k, Some("sha256"), None, None, MSG, &bad).unwrap());
    }

    #[test]
    fn pkcs1_v15_signs_every_supported_digest() {
        let k = key();
        for name in ["sha1", "sha224", "sha384", "sha512", "sha3-256", "ripemd160", "md5", "sm3", "md5-sha1"] {
            let sig = rsa_sign(&k, Some(name), None, None, MSG).unwrap();
            assert!(rsa_verify(&k, Some(name), None, None, MSG, &sig).unwrap(), "{name}");
        }
    }

    #[test]
    fn pss_signature_from_openssl_verifies() {
        let k = key();
        let sig = hex(PSS_SALT20);
        let pss = Some(RSA_PKCS1_PSS_PADDING);
        assert!(rsa_verify(&k, Some("sha256"), pss, Some(20), MSG, &sig).unwrap());
        assert!(rsa_verify(&k, Some("sha256"), pss, None, MSG, &sig).unwrap());
        assert!(rsa_verify(&k, Some("sha256"), pss, Some(SALT_MAX_OR_AUTO), MSG, &sig).unwrap());
        assert!(!rsa_verify(&k, Some("sha256"), pss, Some(SALT_DIGEST), MSG, &sig).unwrap());
        assert!(!rsa_verify(&k, Some("sha256"), pss, Some(21), MSG, &sig).unwrap());
        assert!(!rsa_verify(&k, Some("sha256"), pss, None, b"other", &sig).unwrap());
    }

    #[test]
    fn pss_roundtrip_salt_lengths() {
        let k = key();
        let pss = Some(RSA_PKCS1_PSS_PADDING);
        let max = 128 - 32 - 2;
        for (salt, expect_len) in [(Some(0), 0), (Some(SALT_DIGEST), 32), (None, max), (Some(7), 7)] {
            let sig = rsa_sign(&k, Some("sha256"), pss, salt, MSG).unwrap();
            assert!(rsa_verify(&k, Some("sha256"), pss, None, MSG, &sig).unwrap());
            assert!(rsa_verify(&k, Some("sha256"), pss, Some(expect_len), MSG, &sig).unwrap());
            assert!(!rsa_verify(&k, Some("sha256"), pss, Some(expect_len + 1), MSG, &sig).unwrap());
        }
        assert!(rsa_sign(&k, Some("sha256"), pss, Some(max + 1), MSG).is_err());
    }

    #[test]
    fn pss_with_distinct_mgf1_digest() {
        let mut k = key();
        k.pss = Some(Some(PssParams { hash: "sha256", mgf1_hash: "sha1", salt_length: 0 }));
        assert!(rsa_verify(&k, None, None, Some(SALT_MAX_OR_AUTO), MSG, &hex(PSS_MGF1_SHA1)).unwrap());
        assert!(!rsa_verify(&k, None, None, Some(SALT_MAX_OR_AUTO), b"other", &hex(PSS_MGF1_SHA1)).unwrap());
        let sig = rsa_sign(&k, None, None, Some(16), MSG).unwrap();
        assert!(rsa_verify(&k, None, None, Some(16), MSG, &sig).unwrap());
        assert!(rsa_verify(&k, None, None, Some(SALT_MAX_OR_AUTO), MSG, &sig).unwrap());
        assert!(!rsa_verify(&k, None, None, Some(15), MSG, &sig).unwrap());
    }

    #[test]
    fn oaep_decrypts_openssl_ciphertext_with_label() {
        let k = key();
        let label = [0x00, 0xff, 0x10];
        assert_eq!(oaep_decrypt(&k, Algo::Sha256, &label, &hex(OAEP_SHA256_LABEL)).unwrap(), PLAIN);
    }

    #[test]
    fn oaep_failures_are_indistinguishable() {
        let k = key();
        let ct = hex(OAEP_SHA256_LABEL);
        let wrong_label = err_text(oaep_decrypt(&k, Algo::Sha256, b"other", &ct));
        let wrong_hash = err_text(oaep_decrypt(&k, Algo::Sha1, &[0x00, 0xff, 0x10], &ct));
        let mut flipped = ct.clone();
        flipped[77] ^= 1;
        let corrupted = err_text(oaep_decrypt(&k, Algo::Sha256, &[0x00, 0xff, 0x10], &flipped));
        let pkcs1_ct = err_text(oaep_decrypt(&k, Algo::Sha256, &[], &hex(PKCS1_ENCRYPTED)));
        assert!(wrong_label.contains("oaep decoding error"), "{wrong_label}");
        assert_eq!(wrong_label, wrong_hash);
        assert_eq!(wrong_label, corrupted);
        assert_eq!(wrong_label, pkcs1_ct);
    }

    #[test]
    fn oaep_roundtrip_digests_and_labels() {
        let k = key();
        for algo in [Algo::Sha1, Algo::Sha224, Algo::Sha256, Algo::Sha384, Algo::Sha3_256, Algo::Ripemd160] {
            let ct = oaep_encrypt(&k, algo, b"label", b"hello").unwrap();
            assert_eq!(oaep_decrypt(&k, algo, b"label", &ct).unwrap(), b"hello");
            assert!(oaep_decrypt(&k, algo, b"", &ct).is_err());
        }
        assert!(oaep_encrypt(&k, Algo::Sha256, &[], &[0u8; 100]).is_err());
    }

    #[test]
    fn pkcs1_v15_encryption_matches_openssl() {
        let k = key();
        assert_eq!(pkcs1_decrypt(&k, &hex(PKCS1_ENCRYPTED)).unwrap(), PLAIN);
        let ct = pkcs1_encrypt(&k, PLAIN).unwrap();
        assert_eq!(pkcs1_decrypt(&k, &ct).unwrap(), PLAIN);
        let bad = err_text(pkcs1_decrypt(&k, &hex(OAEP_SHA256_LABEL)));
        let mut flipped = hex(PKCS1_ENCRYPTED);
        flipped[3] ^= 0x40;
        assert!(bad.contains("padding check failed"), "{bad}");
        assert_eq!(bad, err_text(pkcs1_decrypt(&k, &flipped)));
    }

    #[test]
    fn private_encrypt_public_decrypt() {
        let k = key();
        assert_eq!(pkcs1_private_encrypt(&k, PLAIN).unwrap(), hex(PRIVATE_ENCRYPTED));
        assert_eq!(pkcs1_public_decrypt(&k, &hex(PRIVATE_ENCRYPTED)).unwrap(), PLAIN);
        assert!(pkcs1_public_decrypt(&k, &hex(PKCS1_ENCRYPTED)).is_err());
    }

    #[test]
    fn raw_rsa_roundtrip_and_range() {
        let k = key();
        let mut block = vec![0x5au8; 128];
        block[0] = 0;
        let ct = raw_transform(&k, false, &block).unwrap();
        assert_eq!(raw_transform(&k, true, &ct).unwrap(), block);
        let too_big = vec![0xffu8; 128];
        assert!(err_text(raw_transform(&k, false, &too_big)).contains("too large for modulus"));
        assert!(err_text(raw_transform(&k, false, &block[..100])).contains("too small"));
    }
}
}
