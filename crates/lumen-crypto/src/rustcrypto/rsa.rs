//! RSA over the `rsa` crate: PKCS#1 v1.5, OAEP, raw and PSS. The crate's paddings are generic over
//! the digest, so `Algo`s reach them through [`AnyDigest`]. Private-key operations are blinded.
//! Errors imitate OpenSSL's text for the common failures.

use digest::{DynDigest, InvalidBufferSize};
use num_bigint_dig::BigUint;
use rsa::traits::{PrivateKeyParts, PublicKeyParts};
use rsa::{Oaep, Pkcs1v15Encrypt, Pkcs1v15Sign, Pss};

use crate::rng::SysRng;
use crate::error::{CryptoError, Result};
use crate::rsa_util::{
    check_block_len, data_too_large, decoder_unsupported, digest_info_prefix, digest_too_big, illegal_padding, invalid_digest, mod_len,
    oaep_decoding, ossl, padding_check_failed, sign_failed, unpad_type1,
};
use crate::{pad_be, pss as pss_encoding, Algo, PssSalt, RsaPadding, RsaPrivateKey, RsaPublicKey, RsaScheme};
use lumen_common::hash;

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

fn public_key(k: &RsaPublicKey) -> Result<rsa::RsaPublicKey> {
    rsa::RsaPublicKey::new_with_max_size(big(&k.n), big(&k.e), 16384).map_err(|_| decoder_unsupported())
}

fn private_key(k: &RsaPrivateKey) -> Result<rsa::RsaPrivateKey> {
    rsa::RsaPrivateKey::from_components(big(&k.public.n), big(&k.public.e), big(&k.d), vec![big(&k.p), big(&k.q)])
        .map_err(|_| decoder_unsupported())
}

fn rsa_private_op(key: &rsa::RsaPrivateKey) -> impl FnOnce(&[u8]) -> Result<Vec<u8>> + '_ {
    move |em| {
        let s = rsa::hazmat::rsa_decrypt_and_check(key, Some(&mut SysRng), &big(em)).map_err(|_| sign_failed())?;
        Ok(s.to_bytes_be())
    }
}

fn rsa_public_op(key: &rsa::RsaPublicKey, sig: &[u8]) -> Option<Vec<u8>> {
    rsa::hazmat::rsa_encrypt(key, &big(sig)).ok().map(|m| m.to_bytes_be())
}

fn pss(hash_algo: Algo, salt_len: usize) -> Result<Pss> {
    Ok(Pss { blinded: true, digest: Box::new(AnyDigest::new(hash_algo)?), salt_len })
}

pub(super) fn sign(k: &RsaPrivateKey, scheme: &RsaScheme, hashed: &[u8]) -> Result<Vec<u8>> {
    let key = private_key(k)?;
    match scheme {
        RsaScheme::Pkcs1 { hash } => {
            let prefix = digest_info_prefix(*hash)?;
            if mod_len(&k.public.n) < prefix.len() + hashed.len() + 11 {
                return Err(digest_too_big());
            }
            let padding = Pkcs1v15Sign { hash_len: Some(hashed.len()), prefix: prefix.into() };
            key.sign_with_rng(&mut SysRng, padding, hashed).map_err(|_| sign_failed())
        }
        RsaScheme::Pss { hash, mgf1, salt } => {
            let salt_len = pss_encoding::sign_salt_len(&k.public.n, *hash, *salt)?;
            if hash != mgf1 {
                return pss_encoding::sign(&k.public.n, *hash, *mgf1, hashed, salt_len, rsa_private_op(&key));
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
    if sig.len() != mod_len(&k.n) || big(sig) >= n {
        return Ok(false);
    }
    match scheme {
        RsaScheme::Pkcs1 { hash } => {
            let prefix = digest_info_prefix(*hash)?;
            let padding = Pkcs1v15Sign { hash_len: Some(hashed.len()), prefix: prefix.into() };
            Ok(key.verify(padding, hashed, sig).is_ok())
        }
        RsaScheme::Pss { hash, mgf1, salt } => match (salt, hash == mgf1) {
            (PssSalt::Digest, true) => Ok(key.verify(pss(*hash, hash.out_len())?, hashed, sig).is_ok()),
            (PssSalt::Length(len), true) => Ok(key.verify(pss(*hash, *len as usize)?, hashed, sig).is_ok()),
            _ => Ok(rsa_public_op(&key, sig).is_some_and(|em| pss_encoding::verify(&em, &k.n, *hash, *mgf1, hashed, *salt))),
        },
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

pub(super) fn encrypt(k: &RsaPublicKey, padding: &RsaPadding, msg: &[u8]) -> Result<Vec<u8>> {
    match padding {
        RsaPadding::Oaep { hash, mgf1, label } => {
            let (hash, mgf1) = (oaep_digest(*hash)?, oaep_digest(*mgf1)?);
            if msg.len() + 2 * hash.out_len() + 2 > mod_len(&k.n) {
                return Err(data_too_large());
            }
            public_key(k)?.encrypt(&mut SysRng, oaep_scheme(hash, mgf1, label)?, msg).map_err(|_| oaep_decoding())
        }
        RsaPadding::Pkcs1 => {
            if msg.len() + 11 > mod_len(&k.n) {
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
            if msg.len() + 11 > mod_len(&k.public.n) {
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

/// Recovers the message of a block-type-1 padded signature.
pub(super) fn public_decrypt(k: &RsaPublicKey, padding: &RsaPadding, input: &[u8]) -> Result<Vec<u8>> {
    let k_len = check_block_len(k, input)?;
    let m = rsa::hazmat::rsa_encrypt(&public_key(k)?, &big(input)).map_err(|_| padding_check_failed())?;
    let em = pad_be(&m.to_bytes_be(), k_len);
    match padding {
        RsaPadding::None => Ok(em),
        RsaPadding::Pkcs1 => unpad_type1(&em),
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
