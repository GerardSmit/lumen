//! Validation of the `add`/`rem` constraints of prime generation that every backend shares.

use lumen_common::bigint::BigInt;

use crate::error::{CryptoError, Result};

fn int(be: &[u8]) -> BigInt {
    let mut words = Vec::with_capacity(be.len().div_ceil(8));
    for chunk in be.rchunks(8) {
        words.push(chunk.iter().fold(0u64, |acc, &b| acc << 8 | u64::from(b)));
    }
    BigInt::from_words(false, words)
}

/// Rejects constraints no prime can satisfy. `p = rem (mod add)` has a prime solution only when
/// `gcd(add, rem) == 1` (or `add == 1`); OpenSSL's search never terminates otherwise.
pub(crate) fn validate(safe: bool, add: &[u8], rem: Option<&[u8]>) -> Result<()> {
    let add = int(add);
    if add.is_zero() {
        return Err(CryptoError::range("invalid options.add").with_code("ERR_OUT_OF_RANGE"));
    }
    let rem = match rem {
        Some(r) => int(r),
        None if safe => BigInt::from_u64(3),
        None => BigInt::from_u64(1),
    };
    let rem = rem.rem(&add).expect("nonzero modulus");
    if add != BigInt::from_u64(1) && rem.gcd(&add) != BigInt::from_u64(1) {
        return Err(CryptoError::range("invalid options.rem").with_code("ERR_OUT_OF_RANGE"));
    }
    Ok(())
}
