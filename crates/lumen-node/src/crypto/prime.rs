//! Prime generation and testing (`generatePrime`, `checkPrime`).

use lumen::embed::{OpError, SendError};
use num_bigint_dig::BigUint;

use super::bignum::{generate_prime, is_prime, random_range};


#[lumen_bind::module(name = "crypto")]
pub(crate) mod bindings {
use super::*;

const MAX_ATTEMPTS: usize = 1 << 22;

fn bits_too_small() -> SendError {
    SendError::new("Error", "error:01800076:bignum routines::bits too small").with_code("ERR_OSSL_BN_BITS_TOO_SMALL")
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

fn generate(bits: u32, safe: bool, add: Option<&[u8]>, rem: Option<&[u8]>) -> Result<Vec<u8>, SendError> {
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
        return Err(SendError::new("RangeError", "invalid options.add").with_code("ERR_OUT_OF_RANGE"));
    }
    let rem = match rem {
        Some(r) => BigUint::from_bytes_be(r),
        None if safe => BigUint::from(3u8),
        None => one.clone(),
    };
    let rem = &rem % &add;
    if gcd(&add, &rem) != one && add != one {
        return Err(SendError::new("RangeError", "invalid options.rem").with_code("ERR_OUT_OF_RANGE"));
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
    Err(SendError::new("Error", "prime generation failed").with_code("ERR_CRYPTO_OPERATION_FAILED"))
}

/// A prime of exactly `bits` bits, optionally `safe` and congruent to `rem` modulo `add`.
#[op(name = "primeGenerate")]
fn prime_generate(bits: u32, safe: bool, add: Option<Vec<u8>>, rem: Option<Vec<u8>>) -> Result<Vec<u8>, OpError> {
    Ok(generate(bits, safe, add.as_deref(), rem.as_deref())?)
}

#[op(async, name = "primeGenerateAsync")]
fn prime_generate_async(bits: u32, safe: bool, add: Option<Vec<u8>>, rem: Option<Vec<u8>>) -> Result<Vec<u8>, SendError> {
    generate(bits, safe, add.as_deref(), rem.as_deref())
}

fn check(candidate: &[u8]) -> bool {
    is_prime(&BigUint::from_bytes_be(candidate), false)
}

#[op(name = "primeCheck")]
fn prime_check(candidate: &[u8], _checks: u32) -> bool {
    check(candidate)
}

#[op(async, name = "primeCheckAsync")]
fn prime_check_async(candidate: Vec<u8>, _checks: u32) -> Result<bool, SendError> {
    Ok(check(&candidate))
}
}
