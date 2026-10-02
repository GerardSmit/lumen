//! DSA over the `dsa` crate, and FIPS 186 style domain parameter generation.

use num_bigint_dig::BigUint;
use signature::hazmat::{PrehashVerifier, RandomizedPrehashSigner};
use signature::SignatureEncoding;

use super::bignum::{generate_prime, is_prime, modpow, random_range};
use crate::rng::SysRng;
use crate::rsa_util::decoder_unsupported;
use crate::error::{CryptoError, Result};
use crate::{DsaParams, DsaPrivateKey, DsaPublicKey};

fn big(bytes: &[u8]) -> BigUint {
    BigUint::from_bytes_be(bytes)
}

fn components(params: &DsaParams) -> Result<dsa::Components> {
    dsa::Components::from_components(big(&params.p), big(&params.q), big(&params.g)).map_err(|_| decoder_unsupported())
}

fn verifying_key(key: &DsaPublicKey) -> Result<dsa::VerifyingKey> {
    dsa::VerifyingKey::from_components(components(&key.params)?, big(&key.y)).map_err(|_| decoder_unsupported())
}

pub(super) fn sign(key: &DsaPrivateKey, hashed: &[u8]) -> Result<Vec<u8>> {
    let sk = dsa::SigningKey::from_components(verifying_key(&key.public)?, big(&key.x)).map_err(|_| decoder_unsupported())?;
    let sig = sk.sign_prehash_with_rng(&mut SysRng, hashed).map_err(|_| CryptoError::failed("Failed to sign"))?;
    Ok(sig.to_bytes().to_vec())
}

pub(super) fn verify(key: &DsaPublicKey, hashed: &[u8], signature: &[u8]) -> Result<bool> {
    let Ok(vk) = verifying_key(key) else { return Ok(false) };
    let Ok(sig) = dsa::Signature::try_from(signature) else { return Ok(false) };
    Ok(vk.verify_prehash(hashed, &sig).is_ok())
}

/// Domain parameters with an `l`-bit `p` and `n`-bit `q` (FIPS 186 A.1.1-style search, with the
/// `dsa` crate's generator for its standard sizes).
fn parameters(l: u32, n: u32) -> Result<(BigUint, BigUint, BigUint)> {
    #[allow(deprecated)]
    let standard = match (l, n) {
        (1024, 160) => Some(dsa::KeySize::DSA_1024_160),
        (2048, 224) => Some(dsa::KeySize::DSA_2048_224),
        (2048, 256) => Some(dsa::KeySize::DSA_2048_256),
        (3072, 256) => Some(dsa::KeySize::DSA_3072_256),
        _ => None,
    };
    if let Some(size) = standard {
        let c = dsa::Components::generate(&mut SysRng, size);
        return Ok((c.p().clone(), c.q().clone(), c.g().clone()));
    }
    if n < 2 || n >= l || l < 512 {
        return Err(CryptoError::openssl("1C800069", "Provider routines", "invalid key length"));
    }
    let one = BigUint::from(1u8);
    let two = BigUint::from(2u8);
    let p_min = BigUint::from(1u8) << (l as usize - 1);
    let p_max = BigUint::from(1u8) << l as usize;
    loop {
        let q = generate_prime(n, false);
        let two_q = &two * &q;
        for _ in 0..4 * l {
            let m = random_range(&p_min, &p_max).expect("non-empty range");
            let p = &m - (&m % &two_q) + &one;
            if p.bits() != l as usize || !is_prime(&p, false) {
                continue;
            }
            let e = (&p - &one) / &q;
            let mut h = two.clone();
            let g = loop {
                let g = modpow(&h, &e, &p).expect("p is odd");
                if g != one {
                    break g;
                }
                h += &one;
            };
            return Ok((p, q, g));
        }
    }
}

/// A key with an `l`-bit prime; `divisor_bits` is the bit length of `q` (OpenSSL's default when
/// `None`: 256 for `l >= 2048`, else 160).
pub(super) fn generate(l: u32, divisor_bits: Option<u32>) -> Result<DsaPrivateKey> {
    let n = divisor_bits.unwrap_or(if l >= 2048 { 256 } else { 160 });
    let (p, q, g) = parameters(l, n)?;
    let components = dsa::Components::from_components(p.clone(), q.clone(), g.clone())
        .map_err(|_| CryptoError::error("DSA parameter generation failed"))?;
    let sk = dsa::SigningKey::generate(&mut SysRng, components);
    Ok(DsaPrivateKey {
        public: DsaPublicKey {
            params: DsaParams { p: p.to_bytes_be(), q: q.to_bytes_be(), g: g.to_bytes_be() },
            y: sk.verifying_key().y().to_bytes_be(),
        },
        x: sk.x().to_bytes_be(),
    })
}
