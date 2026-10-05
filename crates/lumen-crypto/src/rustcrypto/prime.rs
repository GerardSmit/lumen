//! Prime generation and testing over `crypto-primes`.

use num_bigint_dig::BigUint;

use super::bignum::{generate_prime, is_prime, random_range};
use crate::error::{CryptoError, Result};

const MAX_ATTEMPTS: usize = 1 << 22;

fn bits_too_small() -> CryptoError {
    CryptoError::openssl("01800076", "bignum routines", "bits too small")
}

fn gcd(a: &BigUint, b: &BigUint) -> BigUint {
    let (mut a, mut b) = (a.clone(), b.clone());
    let zero = BigUint::from(0u8);
    while b != zero {
        let r = &a % &b;
        a = std::mem::replace(&mut b, r);
    }
    a
}

pub(super) fn generate(bits: u32, safe: bool, add: Option<&[u8]>, rem: Option<&[u8]>) -> Result<Vec<u8>> {
    if bits < 2 || (safe && bits < 3) {
        return Err(bits_too_small());
    }
    let bits = bits as usize;
    let Some(add) = add.map(BigUint::from_bytes_be) else {
        return Ok(generate_prime(bits as u32, safe).to_bytes_be());
    };
    let zero = BigUint::from(0u8);
    let one = BigUint::from(1u8);
    if add == zero {
        return Err(CryptoError::range("invalid options.add").with_code("ERR_OUT_OF_RANGE"));
    }
    let rem = match rem {
        Some(r) => BigUint::from_bytes_be(r),
        None if safe => BigUint::from(3u8),
        None => one.clone(),
    };
    let rem = &rem % &add;
    if gcd(&add, &rem) != one && add != one {
        return Err(CryptoError::range("invalid options.rem").with_code("ERR_OUT_OF_RANGE"));
    }
    let top = &one << (bits - 1);
    for _ in 0..MAX_ATTEMPTS {
        let c = random_range(&top, &(&top << 1usize)).expect("non-empty range");
        let c = &c - (&c % &add) + &rem;
        if c.bits() != bits || !is_prime(&c, safe) {
            continue;
        }
        return Ok(c.to_bytes_be());
    }
    Err(CryptoError::failed("prime generation failed"))
}

/// The number of rounds is not configurable here: `crypto-primes` picks its own.
pub(super) fn check(candidate: &[u8]) -> Result<bool> {
    Ok(is_prime(&BigUint::from_bytes_be(candidate), false))
}
