//! Constant-time modular exponentiation (`crypto-bigint`) and prime generation / testing
//! (`crypto-primes`) over the `num-bigint-dig` integers the key model uses. Secret exponents only
//! ever go through [`modpow`]; the public side (moduli, generators, peer values) is reduced and
//! compared with ordinary integer arithmetic.

use crypto_bigint::modular::{BoxedMontyForm, BoxedMontyParams};
use crypto_bigint::{BoxedUint, NonZero, Odd, RandomMod};
use crypto_primes::hazmat::{SetBits, SmallFactorsSieveFactory};
use crypto_primes::{is_prime as primes_is_prime, random_prime, sieve_and_find, Flavor};
use num_bigint_dig::BigUint;

use crate::rng::SysRng;

fn limb_bits(bytes: usize) -> u32 {
    (bytes.max(1) as u32 * 8).next_multiple_of(64)
}

fn boxed(n: &BigUint, bits: u32) -> BoxedUint {
    BoxedUint::from_be_slice(&n.to_bytes_be(), bits).expect("value fits the chosen precision")
}

fn big(n: &BoxedUint) -> BigUint {
    BigUint::from_bytes_be(&n.to_be_bytes())
}

/// `base^exp mod modulus` in time independent of the value of `exp` (only its byte length, padded up
/// to the modulus length, is visible). `None` for an even or zero modulus.
pub(crate) fn modpow(base: &BigUint, exp: &BigUint, modulus: &BigUint) -> Option<BigUint> {
    let m_len = modulus.to_bytes_be().len();
    let bits = limb_bits(m_len);
    let odd: Odd<BoxedUint> = Option::from(boxed(modulus, bits).to_odd())?;
    let params = BoxedMontyParams::new(odd);
    let base = BoxedMontyForm::new(boxed(&(base % modulus), bits), &params);
    let exp_bits = limb_bits(exp.to_bytes_be().len().max(m_len));
    Some(big(&base.pow(&boxed(exp, exp_bits)).retrieve()))
}

/// A uniform value in `[low, high)`.
pub(crate) fn random_range(low: &BigUint, high: &BigUint) -> Option<BigUint> {
    if high <= low {
        return None;
    }
    let span = high - low;
    let bits = limb_bits(span.to_bytes_be().len());
    let span: NonZero<BoxedUint> = Option::from(boxed(&span, bits).to_nz())?;
    Some(big(&BoxedUint::random_mod_vartime(&mut SysRng, &span)) + low)
}

fn flavor(safe: bool) -> Flavor {
    if safe {
        Flavor::Safe
    } else {
        Flavor::Any
    }
}

/// Whether `n` is prime (`safe`: and `(n - 1) / 2` is prime too).
pub(crate) fn is_prime(n: &BigUint, safe: bool) -> bool {
    let bits = limb_bits(n.to_bytes_be().len());
    primes_is_prime(flavor(safe), &boxed(n, bits))
}

/// A random prime of exactly `bits` bits (`bits >= 2`, `>= 3` for safe primes). Like OpenSSL's
/// `BN_generate_prime_ex`, the two top bits of a plain prime are set, so that the product of two
/// such primes has the expected size.
pub(crate) fn generate_prime(bits: u32, safe: bool) -> BigUint {
    if safe {
        return big(&random_prime::<BoxedUint, _>(&mut SysRng, Flavor::Safe, bits));
    }
    let factory = SmallFactorsSieveFactory::<BoxedUint>::new(Flavor::Any, bits, SetBits::TwoMsb).expect("bit length of at least 2");
    let found = sieve_and_find(&mut SysRng, factory, |_, candidate| primes_is_prime(Flavor::Any, candidate));
    big(&found.expect("candidates of this size exist").expect("a prime exists in the range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(s: &str) -> BigUint {
        BigUint::parse_bytes(s.as_bytes(), 16).unwrap()
    }

    #[test]
    fn modpow_matches_reference() {
        let p = n("fffffffffffffffffffffffffffffffeffffffffffffffff");
        let base = n("123456789abcdef0123456789abcdef0123456789abcdef");
        let exp = n("deadbeefcafebabe0123456789abcdef");
        assert_eq!(modpow(&base, &exp, &p).unwrap(), base.modpow(&exp, &p));
        assert_eq!(modpow(&(&p + &base), &exp, &p).unwrap(), base.modpow(&exp, &p));
        assert_eq!(modpow(&base, &BigUint::from(0u8), &p).unwrap(), BigUint::from(1u8));
        assert_eq!(modpow(&base, &(&p * &p + &exp), &p).unwrap(), base.modpow(&(&p * &p + &exp), &p));
        assert_eq!(modpow(&BigUint::from(5u8), &BigUint::from(6u8), &BigUint::from(23u8)).unwrap(), BigUint::from(8u8));
    }

    #[test]
    fn modpow_rejects_even_or_zero_modulus() {
        assert!(modpow(&BigUint::from(3u8), &BigUint::from(5u8), &BigUint::from(100u8)).is_none());
        assert!(modpow(&BigUint::from(3u8), &BigUint::from(5u8), &BigUint::from(0u8)).is_none());
    }

    #[test]
    fn random_range_stays_inside() {
        let (low, high) = (BigUint::from(2u8), n("10000000000000000000000001"));
        for _ in 0..64 {
            let v = random_range(&low, &high).unwrap();
            assert!(v >= low && v < high);
        }
        assert!(random_range(&high, &low).is_none());
        assert_eq!(random_range(&BigUint::from(7u8), &BigUint::from(8u8)).unwrap(), BigUint::from(7u8));
    }

    #[test]
    fn primality() {
        let mersenne_127 = (BigUint::from(1u8) << 127usize) - BigUint::from(1u8);
        assert!(is_prime(&mersenne_127, false));
        assert!(!is_prime(&(&mersenne_127 + BigUint::from(2u8)), false));
        for composite in [0u32, 1, 4, 561, 1105, 41041, 3215031751] {
            assert!(!is_prime(&BigUint::from(composite), false), "{composite}");
        }
        for prime in [2u32, 3, 5, 7919, 2147483647] {
            assert!(is_prime(&BigUint::from(prime), false), "{prime}");
        }
        assert!(is_prime(&BigUint::from(23u8), true));
        assert!(!is_prime(&BigUint::from(29u8), true));
        assert!(!is_prime(&mersenne_127, true));
    }

    #[test]
    fn generated_primes_have_the_requested_shape() {
        for bits in [2u32, 3, 8, 64, 257] {
            let p = generate_prime(bits, false);
            assert_eq!(p.bits(), bits as usize);
            assert!(is_prime(&p, false));
            if bits >= 3 {
                assert!(p >= (BigUint::from(3u8) << (bits as usize - 2)), "two top bits set for {bits}");
            }
        }
        for bits in [3u32, 4, 5, 6, 96] {
            let p = generate_prime(bits, true);
            assert_eq!(p.bits(), bits as usize);
            assert!(is_prime(&p, false));
            assert!(is_prime(&((&p - BigUint::from(1u8)) >> 1usize), false));
        }
    }
}
