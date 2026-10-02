//! RSA over the `rsa` crate: PKCS#1 v1.5, OAEP, raw and PSS. The crate's paddings are generic over
//! the digest, so `Algo`s reach them through [`AnyDigest`]. Private-key operations are blinded.
//! Errors imitate OpenSSL's text for the common failures.

use digest::{DynDigest, InvalidBufferSize};
use num_bigint_dig::BigUint;
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{Oaep, Pkcs1v15Encrypt, Pkcs1v15Sign, Pss};

use super::rng::SysRng;
use crate::error::{CryptoError, Result};
use crate::{pad_be, Algo, PssSalt, RsaPadding, RsaPrivateKey, RsaPublicKey, RsaScheme};
use lumen_common::hash;

fn ossl(hex: &str, library: &str, reason: &str) -> CryptoError {
    CryptoError::openssl(hex, library, reason)
}

pub(super) fn decoder_unsupported() -> CryptoError {
    ossl("1E08010C", "DECODER routines", "unsupported")
}

fn data_too_large() -> CryptoError {
    ossl("0200006E", "rsa routines", "data too large for key size")
}

fn data_too_small() -> CryptoError {
    ossl("0200007A", "rsa routines", "data too small for key size")
}

fn too_large_for_modulus() -> CryptoError {
    ossl("02000084", "rsa routines", "data too large for modulus")
}

fn illegal_padding() -> CryptoError {
    ossl("1C8000A5", "Provider routines", "illegal or unsupported padding mode")
}

fn invalid_digest() -> CryptoError {
    ossl("1C80007A", "Provider routines", "invalid digest")
}

fn padding_check_failed() -> CryptoError {
    ossl("02000072", "rsa routines", "padding check failed")
}

fn oaep_decoding() -> CryptoError {
    ossl("02000079", "rsa routines", "oaep decoding error")
}

fn sign_failed() -> CryptoError {
    CryptoError::failed("Failed to sign")
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
fn digest_info_prefix(algo: Algo) -> Result<Vec<u8>> {
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

/// A fixed-output digest of `Algo`'s repertoire as the `DynDigest` the `rsa` paddings are generic
/// over.
#[derive(Clone)]
struct AnyDigest {
    fresh: hash::Hasher,
    state: hash::Hasher,
}

impl AnyDigest {
    fn new(algo: Algo) -> Result<AnyDigest> {
        if algo.is_xof() || algo == Algo::Md5Sha1 {
            return Err(invalid_digest());
        }
        let fresh = hash::Hasher::new(algo);
        Ok(AnyDigest { state: fresh.clone(), fresh })
    }
}

fn copy_digest(h: hash::Hasher, out: &mut [u8]) -> std::result::Result<(), InvalidBufferSize> {
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

    fn finalize_into(self, out: &mut [u8]) -> std::result::Result<(), InvalidBufferSize> {
        copy_digest(self.state, out)
    }

    fn finalize_into_reset(&mut self, out: &mut [u8]) -> std::result::Result<(), InvalidBufferSize> {
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

/// The OAEP label hash, precomputed: `Oaep` takes its label as a `String` but the label is a byte
/// string, so it goes in here instead and the padding just asks for the (empty-label) hash.
#[derive(Clone)]
struct LabelHash(Vec<u8>);

impl LabelHash {
    fn write(&self, out: &mut [u8]) -> std::result::Result<(), InvalidBufferSize> {
        if out.len() != self.0.len() {
            return Err(InvalidBufferSize);
        }
        out.copy_from_slice(&self.0);
        Ok(())
    }
}

impl DynDigest for LabelHash {
    fn update(&mut self, _data: &[u8]) {}

    fn finalize_into(self, out: &mut [u8]) -> std::result::Result<(), InvalidBufferSize> {
        self.write(out)
    }

    fn finalize_into_reset(&mut self, out: &mut [u8]) -> std::result::Result<(), InvalidBufferSize> {
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

fn big(bytes: &[u8]) -> BigUint {
    BigUint::from_bytes_be(bytes)
}

fn mod_len(k: &RsaPublicKey) -> usize {
    big(&k.n).bits().div_ceil(8)
}

fn public_key(k: &RsaPublicKey) -> Result<rsa::RsaPublicKey> {
    rsa::RsaPublicKey::new_with_max_size(big(&k.n), big(&k.e), 16384).map_err(|_| decoder_unsupported())
}

fn private_key(k: &RsaPrivateKey) -> Result<rsa::RsaPrivateKey> {
    rsa::RsaPrivateKey::from_components(big(&k.public.n), big(&k.public.e), big(&k.d), vec![big(&k.p), big(&k.q)])
        .map_err(|_| decoder_unsupported())
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

fn pss_em_len(n: &BigUint) -> usize {
    (n.bits() - 1).div_ceil(8)
}

/// RSASSA-PSS with an MGF1 digest other than the message digest, which the `rsa` crate's `Pss`
/// cannot express. The encoding only touches public values; the private-key operation is the
/// crate's blinded one.
fn pss_split_sign(n: &BigUint, key: &rsa::RsaPrivateKey, hash_algo: Algo, mgf1: Algo, mhash: &[u8], salt_len: usize) -> Result<Vec<u8>> {
    let em_bits = n.bits() - 1;
    let em_len = em_bits.div_ceil(8);
    let h_len = hash_algo.out_len();
    let mut salt = vec![0u8; salt_len];
    lumen_os::proc::entropy(&mut salt).map_err(|_| sign_failed())?;
    let h = hash::digest(hash_algo, &[&[0u8; 8], mhash, &salt].concat());
    let mut db = vec![0u8; em_len - h_len - 1];
    let one = db.len() - salt_len - 1;
    db[one] = 1;
    db[one + 1..].copy_from_slice(&salt);
    mgf1_xor(mgf1, &h, &mut db);
    db[0] &= 0xff >> (8 * em_len - em_bits);
    let em = [db, h, vec![0xbc]].concat();
    let s = rsa::hazmat::rsa_decrypt_and_check(key, Some(&mut SysRng), &BigUint::from_bytes_be(&em)).map_err(|_| sign_failed())?;
    Ok(pad_be(&s.to_bytes_be(), n.bits().div_ceil(8)))
}

/// Verification counterpart of [`pss_split_sign`]; every value involved is public.
fn pss_split_verify(n: &BigUint, key: &rsa::RsaPublicKey, sig: &[u8], hash_algo: Algo, mgf1: Algo, mhash: &[u8], salt_len: usize) -> bool {
    let em_bits = n.bits() - 1;
    let em_len = em_bits.div_ceil(8);
    let h_len = hash_algo.out_len();
    let Ok(m) = rsa::hazmat::rsa_encrypt(key, &BigUint::from_bytes_be(sig)) else { return false };
    let m = m.to_bytes_be();
    if m.len() > em_len || em_len < h_len + salt_len + 2 {
        return false;
    }
    let em = pad_be(&m, em_len);
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
    mgf1_xor(mgf1, h, &mut db);
    db[0] &= top;
    let ps = db.len() - salt_len - 1;
    if db[..ps].iter().any(|&b| b != 0) || db[ps] != 1 {
        return false;
    }
    hash::constant_time_eq(&hash::digest(hash_algo, &[&[0u8; 8], mhash, &db[ps + 1..]].concat()), h)
}

fn pss(hash_algo: Algo, salt_len: usize) -> Result<Pss> {
    Ok(Pss { blinded: true, digest: Box::new(AnyDigest::new(hash_algo)?), salt_len })
}

pub(super) fn sign(k: &RsaPrivateKey, scheme: &RsaScheme, hashed: &[u8]) -> Result<Vec<u8>> {
    let key = private_key(k)?;
    match scheme {
        RsaScheme::Pkcs1 { hash } => {
            let prefix = digest_info_prefix(*hash)?;
            if mod_len(&k.public) < prefix.len() + hashed.len() + 11 {
                return Err(ossl("02000070", "rsa routines", "digest too big for rsa key"));
            }
            let padding = Pkcs1v15Sign { hash_len: Some(hashed.len()), prefix: prefix.into() };
            key.sign_with_rng(&mut SysRng, padding, hashed).map_err(|_| sign_failed())
        }
        RsaScheme::Pss { hash, mgf1, salt } => {
            let n = big(&k.public.n);
            let h_len = hash.out_len();
            let max = pss_em_len(&n).saturating_sub(h_len + 2);
            let salt_len = match salt {
                PssSalt::MaxOrAuto => max,
                PssSalt::Digest => h_len,
                PssSalt::Length(s) => *s as usize,
            };
            if salt_len > max {
                return Err(data_too_large());
            }
            if hash != mgf1 {
                return pss_split_sign(&n, &key, *hash, *mgf1, hashed, salt_len);
            }
            key.sign_with_rng(&mut SysRng, pss(*hash, salt_len)?, hashed).map_err(|_| sign_failed())
        }
    }
}

pub(super) fn verify(k: &RsaPublicKey, scheme: &RsaScheme, hashed: &[u8], sig: &[u8]) -> Result<bool> {
    let n = big(&k.n);
    if n.bits() < 2 {
        return Ok(false);
    }
    let key = rsa::RsaPublicKey::new_unchecked(n.clone(), big(&k.e));
    if sig.len() != mod_len(k) || big(sig) >= n {
        return Ok(false);
    }
    match scheme {
        RsaScheme::Pkcs1 { hash } => {
            let prefix = digest_info_prefix(*hash)?;
            let padding = Pkcs1v15Sign { hash_len: Some(hashed.len()), prefix: prefix.into() };
            Ok(key.verify(padding, hashed, sig).is_ok())
        }
        RsaScheme::Pss { hash, mgf1, salt } => {
            let h_len = hash.out_len();
            let max = pss_em_len(&n).saturating_sub(h_len + 2);
            let verify_with = |salt_len: usize| -> Result<bool> {
                if hash != mgf1 {
                    return Ok(pss_split_verify(&n, &key, sig, *hash, *mgf1, hashed, salt_len));
                }
                Ok(key.verify(pss(*hash, salt_len)?, hashed, sig).is_ok())
            };
            match salt {
                PssSalt::MaxOrAuto => {
                    // The salt length is not known; the digest-length salt is by far the most common.
                    for salt_len in std::iter::once(h_len).chain((0..=max).rev()).filter(|&s| s <= max) {
                        if verify_with(salt_len)? {
                            return Ok(true);
                        }
                    }
                    Ok(false)
                }
                PssSalt::Digest => verify_with(h_len),
                PssSalt::Length(s) => verify_with(*s as usize),
            }
        }
    }
}

fn oaep_scheme(hash_algo: Algo, mgf1: Algo, label: &[u8]) -> Result<Oaep> {
    Ok(Oaep {
        digest: Box::new(LabelHash(hash::digest(hash_algo, label))),
        mgf_digest: Box::new(AnyDigest::new(mgf1)?),
        label: None,
    })
}

fn oaep_digest(algo: Algo) -> Result<Algo> {
    if algo.is_xof() || algo == Algo::Md5Sha1 {
        return Err(CryptoError::error("Digest method not supported").with_code("ERR_OSSL_EVP_UNSUPPORTED"));
    }
    Ok(algo)
}

/// Checks the input length against the modulus the way OpenSSL reports it; returns the modulus length.
fn check_block_len(k: &RsaPublicKey, input: &[u8]) -> Result<usize> {
    let k_len = mod_len(k);
    if input.len() > k_len {
        return Err(data_too_large());
    }
    if input.len() < k_len {
        return Err(data_too_small());
    }
    if big(input) >= big(&k.n) {
        return Err(too_large_for_modulus());
    }
    Ok(k_len)
}

pub(super) fn encrypt(k: &RsaPublicKey, padding: &RsaPadding, msg: &[u8]) -> Result<Vec<u8>> {
    match padding {
        RsaPadding::Oaep { hash, mgf1, label } => {
            let (hash, mgf1) = (oaep_digest(*hash)?, oaep_digest(*mgf1)?);
            if msg.len() + 2 * hash.out_len() + 2 > mod_len(k) {
                return Err(data_too_large());
            }
            public_key(k)?.encrypt(&mut SysRng, oaep_scheme(hash, mgf1, label)?, msg).map_err(|_| oaep_decoding())
        }
        RsaPadding::Pkcs1 => {
            if msg.len() + 11 > mod_len(k) {
                return Err(data_too_large());
            }
            public_key(k)?.encrypt(&mut SysRng, Pkcs1v15Encrypt, msg).map_err(|_| padding_check_failed())
        }
        RsaPadding::None => {
            let k_len = check_block_len(k, msg)?;
            let out = rsa::hazmat::rsa_encrypt(&public_key(k)?, &big(msg)).map_err(|_| sign_failed())?;
            Ok(pad_be(&out.to_bytes_be(), k_len))
        }
    }
}

pub(super) fn decrypt(k: &RsaPrivateKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
    let k_len = check_block_len(&k.public, input)?;
    let key = private_key(k)?;
    match padding {
        // Every failure of the padding checks surfaces as the same error.
        RsaPadding::Oaep { hash, mgf1, label } => {
            let scheme = oaep_scheme(oaep_digest(*hash)?, oaep_digest(*mgf1)?, label)?;
            key.decrypt_blinded(&mut SysRng, scheme, input).map_err(|_| oaep_decoding())
        }
        RsaPadding::Pkcs1 => key.decrypt_blinded(&mut SysRng, Pkcs1v15Encrypt, input).map_err(|_| padding_check_failed()),
        RsaPadding::None => {
            let out = rsa::hazmat::rsa_decrypt_and_check(&key, Some(&mut SysRng), &big(input)).map_err(|_| sign_failed())?;
            Ok(pad_be(&out.to_bytes_be(), k_len))
        }
    }
}

pub(super) fn private_encrypt(k: &RsaPrivateKey, padding: &RsaPadding, msg: &[u8]) -> Result<Vec<u8>> {
    match padding {
        RsaPadding::Pkcs1 => {
            if msg.len() + 11 > mod_len(&k.public) {
                return Err(data_too_large());
            }
            private_key(k)?.sign_with_rng(&mut SysRng, Pkcs1v15Sign::new_unprefixed(), msg).map_err(|_| sign_failed())
        }
        RsaPadding::None => {
            let k_len = check_block_len(&k.public, msg)?;
            let out = rsa::hazmat::rsa_decrypt_and_check(&private_key(k)?, Some(&mut SysRng), &big(msg)).map_err(|_| sign_failed())?;
            Ok(pad_be(&out.to_bytes_be(), k_len))
        }
        RsaPadding::Oaep { .. } => Err(illegal_padding()),
    }
}

/// Recovers the message of a block-type-1 padded signature. Everything involved is public, so the
/// unpadding needs no constant-time care.
pub(super) fn public_decrypt(k: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
    let k_len = check_block_len(k, input)?;
    let m = rsa::hazmat::rsa_encrypt(&public_key(k)?, &big(input)).map_err(|_| padding_check_failed())?;
    let em = pad_be(&m.to_bytes_be(), k_len);
    match padding {
        RsaPadding::None => Ok(em),
        RsaPadding::Pkcs1 => {
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
        RsaPadding::Oaep { .. } => Err(illegal_padding()),
    }
}

pub(super) fn generate(bits: u32, e: &[u8]) -> Result<RsaPrivateKey> {
    let exponent = big(e);
    if exponent < BigUint::from(3u8) || e.last().is_none_or(|b| b & 1 == 0) || exponent.bits() > 32 {
        return Err(ossl("1C80006F", "Provider routines", "invalid public exponent").with_code("ERR_OSSL_PUB_EXPONENT_OUT_OF_RANGE"));
    }
    if bits < 512 {
        return Err(ossl("1C80006B", "Provider routines", "key size too small"));
    }
    let k = rsa::RsaPrivateKey::new_with_exp(&mut SysRng, bits as usize, &exponent)
        .map_err(|err| CryptoError::error(format!("RSA key generation failed: {err}")))?;
    let primes = k.primes();
    let (p, q) = (primes[0].clone(), primes[1].clone());
    let one = BigUint::from(1u8);
    let d = k.d().clone();
    let dp = k.dp().cloned().unwrap_or_else(|| &d % (&p - &one));
    let dq = k.dq().cloned().unwrap_or_else(|| &d % (&q - &one));
    let qi = k.crt_coefficient().unwrap_or_default();
    Ok(RsaPrivateKey {
        public: RsaPublicKey { n: k.n().to_bytes_be(), e: k.e().to_bytes_be() },
        d: d.to_bytes_be(),
        p: p.to_bytes_be(),
        q: q.to_bytes_be(),
        dp: dp.to_bytes_be(),
        dq: dq.to_bytes_be(),
        qi: qi.to_bytes_be(),
    })
}
