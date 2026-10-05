//! Key pair generation (`generateKeyPair`): RSA / RSA-PSS, DSA and finite-field DH through
//! `lumen_crypto`'s backend; EC (the curve crates) and Ed25519 / Ed448 / X25519 / X448 on RustCrypto.

use num_bigint_dig::BigUint;

use super::curves::EcCurve;
use super::model::{okp_public, AsymKey, DhKey, DsaKey, EcKey, OkpKey, PssParams, RsaKey};
use super::{KResult, SendError};
use crate::crypto::send_error;

/// An RSA key of `bits` bits with public exponent `e`; `pss` makes it an RSASSA-PSS key (with
/// parameter restrictions when given).
pub fn rsa(bits: u32, e: u32, pss: Option<Option<PssParams>>) -> KResult<AsymKey> {
    let k = lumen_crypto::backend()
        .rsa_generate(bits, &e.to_be_bytes())
        .map_err(send_error)?;
    Ok(AsymKey::Rsa(RsaKey::from_parts(&k, pss)))
}

/// A DSA key with an `l`-bit prime; `divisor` is the bit length of `q` (the backend's default when
/// `None`).
pub fn dsa(l: u32, divisor: Option<u32>) -> KResult<AsymKey> {
    let k = lumen_crypto::backend()
        .dsa_generate(l, divisor)
        .map_err(send_error)?;
    Ok(AsymKey::Dsa(DsaKey::from_parts(&k)))
}

pub fn ec(curve: EcCurve, explicit: bool) -> AsymKey {
    let (d, point) = curve.generate();
    AsymKey::Ec(EcKey {
        curve,
        point,
        d: Some(d),
        explicit,
    })
}

/// An Ed25519 / Ed448 / X25519 / X448 key (`kind` as `asymmetricKeyType`).
pub fn okp(kind: &str) -> KResult<AsymKey> {
    let (len, ctor): (usize, fn(OkpKey) -> AsymKey) = match kind {
        "ed25519" => (32, AsymKey::Ed25519),
        "ed448" => (57, AsymKey::Ed448),
        "x25519" => (32, AsymKey::X25519),
        "x448" => (56, AsymKey::X448),
        _ => return Err(SendError::new("Error", "Unsupported key type")),
    };
    let mut private = vec![0u8; len];
    lumen_os::proc::entropy(&mut private).map_err(|e| SendError::new("Error", e.to_string()))?;
    let public = okp_public(kind, &private)?;
    Ok(ctor(OkpKey {
        public,
        private: Some(private),
    }))
}

/// A DH key over `(p, g)` with a random private value.
pub fn dh(p: BigUint, g: BigUint) -> KResult<AsymKey> {
    let params = lumen_crypto::DhParams {
        p: p.to_bytes_be(),
        g: g.to_bytes_be(),
    };
    let (x, y) = lumen_crypto::backend()
        .dh_generate_key(&params, None)
        .map_err(send_error)?;
    let num = |b: &[u8]| BigUint::from_bytes_be(b);
    Ok(AsymKey::Dh(DhKey {
        p,
        g,
        q: None,
        y: num(&y),
        x: Some(num(&x)),
    }))
}

/// A safe prime of `bits` bits that `g` generates a subgroup of.
pub fn safe_prime(bits: u32, g: u32) -> KResult<BigUint> {
    let p = lumen_crypto::backend()
        .dh_generate_prime(bits, g)
        .map_err(send_error)?;
    Ok(BigUint::from_bytes_be(&p))
}
