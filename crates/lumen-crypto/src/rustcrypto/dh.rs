//! Finite-field Diffie-Hellman over constant-time `crypto-bigint` exponentiation. The modulus must
//! be odd (OpenSSL accepts an even one; this backend refuses it).

use num_bigint_dig::BigUint;

use super::bignum::{generate_prime, is_prime, modpow, random_range};
use crate::error::{CryptoError, Result};
use crate::{
    pad_be, trim_be, DhParams, DH_CHECK_PUBKEY_TOO_LARGE, DH_CHECK_PUBKEY_TOO_SMALL, DH_CHECK_P_NOT_PRIME, DH_CHECK_P_NOT_SAFE_PRIME,
    DH_MODULUS_TOO_LARGE, DH_MODULUS_TOO_SMALL, DH_NOT_SUITABLE_GENERATOR,
};

const DH_MIN_MODULUS_BITS: usize = 512;
const DH_MAX_MODULUS_BITS: usize = 10000;

fn failed(message: &str) -> CryptoError {
    CryptoError::failed(message)
}

fn num(bytes: &[u8]) -> BigUint {
    BigUint::from_bytes_be(bytes)
}

fn minimal(n: &BigUint) -> Vec<u8> {
    trim_be(&n.to_bytes_be()).to_vec()
}

fn small_mod(n: &BigUint, m: u32) -> u32 {
    let r = n % BigUint::from(m);
    r.to_bytes_be().iter().fold(0u32, |acc, &b| (acc << 8) | b as u32)
}

pub(super) fn check(params: &DhParams) -> Result<u32> {
    let (p, g) = (num(&params.p), num(&params.g));
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
    Ok(flags)
}

pub(super) fn check_public(params: &DhParams, public: &[u8]) -> Result<u32> {
    let (p, peer) = (num(&params.p), num(public));
    let one = BigUint::from(1u8);
    let mut flags = 0;
    if peer <= one {
        flags |= DH_CHECK_PUBKEY_TOO_SMALL;
    }
    if peer >= &p - &one {
        flags |= DH_CHECK_PUBKEY_TOO_LARGE;
    }
    Ok(flags)
}

pub(super) fn generate_prime_for(bits: u32, generator: u32) -> Result<Vec<u8>> {
    if bits < 2 {
        return Err(CryptoError::openssl("0280007E", "Diffie-Hellman routines", "modulus too small"));
    }
    loop {
        let p = if bits < 3 { BigUint::from(3u8) } else { generate_prime(bits, true) };
        let suitable = match generator {
            2 => matches!(small_mod(&p, 24), 11 | 23),
            5 => matches!(small_mod(&p, 10), 3 | 7),
            _ => true,
        };
        if suitable {
            return Ok(minimal(&p));
        }
    }
}

pub(super) fn public(params: &DhParams, private: &[u8]) -> Result<Vec<u8>> {
    let y = modpow(&num(&params.g), &num(private), &num(&params.p)).ok_or_else(|| failed("Key generation failed"))?;
    Ok(minimal(&y))
}

pub(super) fn generate_key(params: &DhParams, private: Option<&[u8]>) -> Result<(Vec<u8>, Vec<u8>)> {
    let p = num(&params.p);
    if p.bits() < DH_MIN_MODULUS_BITS || p.bits() > DH_MAX_MODULUS_BITS {
        return Err(failed("Key generation failed"));
    }
    let x = match private {
        Some(x) => minimal(&num(x)),
        None => minimal(
            &random_range(&BigUint::from(2u8), &(&p - BigUint::from(2u8))).ok_or_else(|| failed("Key generation failed"))?,
        ),
    };
    let y = public(params, &x)?;
    Ok((x, y))
}

pub(super) fn compute(params: &DhParams, private: &[u8], peer: &[u8]) -> Result<Vec<u8>> {
    let p = num(&params.p);
    let secret = modpow(&num(peer), &num(private), &p).ok_or_else(|| failed("Failed to compute DH key"))?;
    Ok(pad_be(&secret.to_bytes_be(), p.bits().div_ceil(8)))
}
