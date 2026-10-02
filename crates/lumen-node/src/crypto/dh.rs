//! Diffie-Hellman: finite-field DH over constant-time `crypto-bigint` exponentiation, ECDH and the
//! stateless key agreement of `crypto.diffieHellman` (EC, X25519, X448, DH).

use lumen::embed::OpError;
use num_bigint_dig::BigUint;

use super::bignum::{is_prime, modpow, random_range};
use super::keys::{asn1, dh_group, safe_prime, with_ec_curve, x448_mul, AsymKey, EcCurve};


#[lumen_bind::module(name = "crypto")]
pub(crate) mod bindings {
use super::*;

const DH_CHECK_P_NOT_PRIME: u32 = 1;
const DH_CHECK_P_NOT_SAFE_PRIME: u32 = 2;
const DH_NOT_SUITABLE_GENERATOR: u32 = 8;
const DH_MODULUS_TOO_SMALL: u32 = 128;
const DH_MODULUS_TOO_LARGE: u32 = 256;
const DH_MIN_MODULUS_BITS: usize = 512;
const DH_MAX_MODULUS_BITS: usize = 10000;

fn failed(message: &'static str) -> OpError {
    OpError::error(message).with_code("ERR_CRYPTO_OPERATION_FAILED")
}

/// OpenSSL 3's provider error for peer keys over different groups / curves.
fn domain_mismatch() -> OpError {
    OpError::error("error:1C8000DE:Provider routines::mismatching domain parameters")
        .with_code("ERR_OSSL_MISMATCHING_DOMAIN_PARAMETERS")
}

fn num(bytes: &[u8]) -> BigUint {
    BigUint::from_bytes_be(bytes)
}

fn minimal(n: &BigUint) -> Vec<u8> {
    let bytes = n.to_bytes_be();
    let start = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len());
    bytes[start..].to_vec()
}

/// `base^x mod p`; the modulus must be odd.
fn exp_mod(base: &BigUint, x: &BigUint, p: &BigUint) -> Result<BigUint, OpError> {
    modpow(base, x, p).ok_or_else(|| failed("Key generation failed"))
}

fn small_mod(n: &BigUint, m: u32) -> u32 {
    let r = n % BigUint::from(m);
    r.to_bytes_be().iter().fold(0u32, |acc, &b| (acc << 8) | b as u32)
}

/// `DH_check`: the `verifyError` flags of the parameters.
#[op(name = "dhVerify")]
fn dh_verify(p: &[u8], g: &[u8]) -> u32 {
    let (p, g) = (num(p), num(g));
    let one = BigUint::from(1u8);
    let mut flags = 0;
    if p.bits() < DH_MIN_MODULUS_BITS {
        flags |= DH_MODULUS_TOO_SMALL;
    }
    if p.bits() > DH_MAX_MODULUS_BITS {
        flags |= DH_MODULUS_TOO_LARGE;
    }
    if g <= one || p <= BigUint::from(2u8) || g >= &p - &one {
        flags |= DH_NOT_SUITABLE_GENERATOR;
    } else if g == BigUint::from(2u8) {
        if !matches!(small_mod(&p, 24), 11 | 23) {
            flags |= DH_NOT_SUITABLE_GENERATOR;
        }
    } else if g == BigUint::from(5u8) && !matches!(small_mod(&p, 10), 3 | 7) {
        flags |= DH_NOT_SUITABLE_GENERATOR;
    }
    if !is_prime(&p, false) {
        flags |= DH_CHECK_P_NOT_PRIME;
    } else if !is_prime(&((&p - &one) >> 1usize), false) {
        flags |= DH_CHECK_P_NOT_SAFE_PRIME;
    }
    flags
}

/// A safe prime of `bits` bits that `g` (2 or 5) generates a subgroup of.
#[op(name = "dhGenPrime")]
fn dh_gen_prime(bits: u32, g: u32) -> Result<Vec<u8>, OpError> {
    loop {
        let p = safe_prime(bits)?;
        let suitable = match g {
            2 => matches!(small_mod(&p, 24), 11 | 23),
            5 => matches!(small_mod(&p, 10), 3 | 7),
            _ => true,
        };
        if suitable {
            return Ok(minimal(&p));
        }
    }
}

/// A key pair `[private, public]` for the parameters.
#[op(name = "dhGenKey")]
fn dh_gen_key(p: &[u8], g: &[u8], private: Option<Vec<u8>>) -> Result<(Vec<u8>, Vec<u8>), OpError> {
    let (p, g) = (num(p), num(g));
    if p.bits() < DH_MIN_MODULUS_BITS || p.bits() > DH_MAX_MODULUS_BITS {
        return Err(failed("Key generation failed"));
    }
    let x = match private {
        Some(x) => num(&x),
        None => random_range(&BigUint::from(2u8), &(&p - BigUint::from(2u8))).ok_or_else(|| failed("Key generation failed"))?,
    };
    let y = exp_mod(&g, &x, &p)?;
    Ok((minimal(&x), minimal(&y)))
}

#[op(name = "dhPublic")]
fn dh_public(p: &[u8], g: &[u8], x: &[u8]) -> Result<Vec<u8>, OpError> {
    Ok(minimal(&exp_mod(&num(g), &num(x), &num(p))?))
}

#[op(name = "dhCompute")]
fn dh_compute(p: &[u8], x: &[u8], peer: &[u8]) -> Result<Vec<u8>, OpError> {
    let (p, peer) = (num(p), num(peer));
    let one = BigUint::from(1u8);
    if peer <= one {
        return Err(OpError::range_error("Supplied key is too small").with_code("ERR_CRYPTO_INVALID_KEYLEN"));
    }
    if peer >= &p - &one {
        return Err(OpError::range_error("Supplied key is too large").with_code("ERR_CRYPTO_INVALID_KEYLEN"));
    }
    let secret = modpow(&peer, &num(x), &p).ok_or_else(|| failed("Failed to compute DH key"))?;
    Ok(asn1::pad_be(&secret.to_bytes_be(), p.bits().div_ceil(8)))
}

/// `[p, g]` of a MODP group.
#[op(name = "dhGroupParams")]
fn dh_group_params(name: &str) -> Option<(Vec<u8>, Vec<u8>)> {
    dh_group(name).map(|(p, g)| (minimal(&p), minimal(&g)))
}

/// The curves `createECDH` accepts: OpenSSL short names.
fn ecdh_curve(name: &str) -> Result<EcCurve, OpError> {
    match name {
        "prime256v1" | "secp384r1" | "secp521r1" | "secp256k1" => EcCurve::from_name(name),
        _ => None,
    }
    .ok_or_else(|| OpError::type_error("Invalid EC curve name").with_code("ERR_CRYPTO_INVALID_CURVE"))
}

#[op(name = "ecdhCurveKnown")]
fn ecdh_curve_known(name: &str) -> bool {
    ecdh_curve(name).is_ok()
}

/// `[private, public]` with the private scalar of the curve's field length.
#[op(name = "ecdhGenerate")]
fn ecdh_generate(curve: &str) -> Result<(Vec<u8>, Vec<u8>), OpError> {
    Ok(ecdh_curve(curve)?.generate())
}

fn invalid_private() -> OpError {
    OpError::range_error("Private key is not valid for specified curve.").with_code("ERR_CRYPTO_INVALID_KEYTYPE")
}

/// The uncompressed public point of a private scalar; throws when the scalar is out of range.
#[op(name = "ecdhPublic")]
fn ecdh_public(curve: &str, private: &[u8]) -> Result<Vec<u8>, OpError> {
    let curve = ecdh_curve(curve)?;
    if private.len() > curve.field_len() && private[..private.len() - curve.field_len()].iter().any(|&b| b != 0) {
        return Err(invalid_private());
    }
    curve.public_from_scalar(private).map_err(|_| invalid_private())
}

fn ec_agree(curve: EcCurve, private: &[u8], peer: &[u8]) -> Result<Vec<u8>, OpError> {
    let invalid_public = || OpError::error("Public key is not valid for specified curve").with_code("ERR_CRYPTO_ECDH_INVALID_PUBLIC_KEY");
    let scalar = asn1::pad_be(private, curve.field_len());
    with_ec_curve!(curve, C => {
        let sk = elliptic_curve::SecretKey::<C>::from_slice(&scalar).map_err(|_| failed("Failed to compute ECDH key"))?;
        let pk = elliptic_curve::PublicKey::<C>::from_sec1_bytes(peer).map_err(|_| invalid_public())?;
        let shared = elliptic_curve::ecdh::diffie_hellman(sk.to_nonzero_scalar(), pk.as_affine());
        Ok(shared.raw_secret_bytes().to_vec())
    })
}

#[op(name = "ecdhCompute")]
fn ecdh_compute(curve: &str, private: &[u8], peer: &[u8]) -> Result<Vec<u8>, OpError> {
    ec_agree(ecdh_curve(curve)?, private, peer)
}

const POINT_COMPRESSED: u32 = 2;
const POINT_HYBRID: u32 = 6;

fn convert_point(curve: EcCurve, key: &[u8], format: u32) -> Result<Vec<u8>, OpError> {
    let uncompressed = curve
        .normalize_point(key)
        .map_err(|_| failed("Failed to convert Buffer to EC_POINT"))?;
    let flen = curve.field_len();
    let odd = uncompressed.last().is_some_and(|b| b & 1 == 1);
    Ok(match format {
        POINT_COMPRESSED => {
            let mut out = vec![if odd { 3 } else { 2 }];
            out.extend_from_slice(&uncompressed[1..1 + flen]);
            out
        }
        POINT_HYBRID => {
            let mut out = uncompressed;
            out[0] = if odd { 7 } else { 6 };
            out
        }
        _ => uncompressed,
    })
}

/// A public point in `format` (`POINT_CONVERSION_*`), validating it on the curve.
#[op(name = "ecdhConvert")]
fn ecdh_convert(curve: &str, key: &[u8], format: u32) -> Result<Vec<u8>, OpError> {
    convert_point(ecdh_curve(curve)?, key, format)
}

/// `crypto.diffieHellman`: the shared secret of a private and a public (or private) handle.
#[op(name = "statelessDh")]
fn stateless_dh(private_kind: u32, private_der: &[u8], public_kind: u32, public_der: &[u8]) -> Result<Vec<u8>, OpError> {
    let private = AsymKey::from_handle(private_kind, private_der)?;
    let public = AsymKey::from_handle(public_kind, public_der)?;
    let mismatch = || failed("Failed to derive shared secret");
    match (&private, &public) {
        (AsymKey::Ec(a), AsymKey::Ec(b)) => {
            if a.curve != b.curve {
                return Err(domain_mismatch());
            }
            ec_agree(a.curve, a.d.as_deref().ok_or_else(mismatch)?, &b.point)
        }
        (AsymKey::X25519(a), AsymKey::X25519(b)) => {
            let secret = a.x25519_secret()?.diffie_hellman(&b.x25519_public()?);
            if !secret.was_contributory() {
                return Err(mismatch());
            }
            Ok(secret.to_bytes().to_vec())
        }
        (AsymKey::X448(a), AsymKey::X448(b)) => {
            let k = a.private_array::<56>()?;
            let u = b.public_array::<56>()?;
            let out = x448_mul(&k, &u);
            if out.iter().all(|&x| x == 0) {
                return Err(mismatch());
            }
            Ok(out.to_vec())
        }
        (AsymKey::Dh(a), AsymKey::Dh(b)) => {
            if a.p != b.p || a.g != b.g {
                return Err(domain_mismatch());
            }
            let x = a.x.as_ref().ok_or_else(mismatch)?;
            let secret = modpow(&b.y, x, &a.p).ok_or_else(mismatch)?;
            Ok(asn1::pad_be(&secret.to_bytes_be(), a.p.bits().div_ceil(8)))
        }
        _ => Err(mismatch()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::keys::dh_group;

    fn group(name: &str) -> (Vec<u8>, Vec<u8>) {
        let (p, g) = dh_group(name).unwrap();
        (minimal(&p), minimal(&g))
    }

    #[test]
    fn modp_groups_verify_clean() {
        for name in ["modp1", "modp2", "modp5", "modp14"] {
            let (p, g) = group(name);
            assert_eq!(dh_verify(&p, &g), 0, "{name}");
        }
    }

    #[test]
    fn verify_flags() {
        let (p, g) = group("modp5");
        let mut composite = p.clone();
        *composite.last_mut().unwrap() ^= 0x0e;
        assert_ne!(dh_verify(&composite, &g) & DH_CHECK_P_NOT_PRIME, 0);
        let not_safe = (BigUint::from(1u8) << 521usize) - BigUint::from(1u8);
        assert_eq!(dh_verify(&minimal(&not_safe), &[2]) & DH_CHECK_P_NOT_SAFE_PRIME, DH_CHECK_P_NOT_SAFE_PRIME);
        assert_ne!(dh_verify(&[0x17], &[2]) & DH_MODULUS_TOO_SMALL, 0);
        assert_ne!(dh_verify(&p, &[1]) & DH_NOT_SUITABLE_GENERATOR, 0);
    }

    #[test]
    fn key_agreement_matches_reference_exponentiation() {
        let (p, g) = group("modp2");
        let (x1, y1) = dh_gen_key(&p, &g, None).unwrap();
        let (x2, y2) = dh_gen_key(&p, &g, None).unwrap();
        let (pn, gn) = (num(&p), num(&g));
        assert_eq!(num(&y1), gn.modpow(&num(&x1), &pn));
        let s1 = dh_compute(&p, &x1, &y2).unwrap();
        let s2 = dh_compute(&p, &x2, &y1).unwrap();
        assert_eq!(s1, s2);
        assert_eq!(s1.len(), p.len());
        assert_eq!(num(&s1), num(&y2).modpow(&num(&x1), &pn));
        assert_eq!(dh_public(&p, &g, &x1).unwrap(), y1);
    }

    #[test]
    fn peer_range_is_enforced() {
        let (p, g) = group("modp1");
        let (x, _) = dh_gen_key(&p, &g, None).unwrap();
        assert!(dh_compute(&p, &x, &[1]).unwrap_err().to_string().contains("too small"));
        let p_minus_1 = minimal(&(num(&p) - BigUint::from(1u8)));
        assert!(dh_compute(&p, &x, &p_minus_1).unwrap_err().to_string().contains("too large"));
    }

    #[test]
    fn even_modulus_is_refused() {
        let mut p = group("modp1").0;
        *p.last_mut().unwrap() &= 0xfe;
        assert!(dh_gen_key(&p, &[2], None).is_err());
    }

    #[test]
    fn generated_prime_is_safe_and_suits_the_generator() {
        for g in [2u32, 5] {
            let p = num(&dh_gen_prime(48, g).unwrap());
            assert_eq!(p.bits(), 48);
            assert_eq!(dh_verify(&minimal(&p), &[g as u8]) & !DH_MODULUS_TOO_SMALL, 0, "g={g}");
        }
    }
}
}
