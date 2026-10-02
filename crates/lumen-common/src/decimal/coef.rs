//! Digit-level helpers on non-negative [`BigInt`] coefficients.

use super::Rounding;
use crate::bigint::BigInt;
use std::cell::RefCell;
use std::cmp::Ordering;

/// An operation needed a coefficient beyond the allocation ceiling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mem;

pub type R<T> = Result<T, Mem>;

/// The most digits a coefficient can have (BigInt's size ceiling, in decimal digits).
pub const MAX_DIGITS: u64 = 323_000_000;

const SMALL: usize = 1024;

thread_local! {
    static POW10: RefCell<Vec<Option<BigInt>>> = RefCell::new(vec![None; SMALL]);
}

fn raw_pow10(n: u64) -> BigInt {
    BigInt::from_u64(10).pow(&BigInt::from_u64(n)).unwrap_or_else(|_| BigInt::zero())
}

/// `10^n`, or [`Mem`] when it would exceed the allocation ceiling.
pub fn pow10(n: u64) -> R<BigInt> {
    if n >= MAX_DIGITS {
        return Err(Mem);
    }
    if (n as usize) < SMALL {
        return Ok(POW10.with(|c| {
            let mut c = c.borrow_mut();
            c[n as usize].get_or_insert_with(|| raw_pow10(n)).clone()
        }));
    }
    Ok(raw_pow10(n))
}

pub fn is_odd(c: &BigInt) -> bool {
    let (_, w) = c.words();
    w.first().is_some_and(|w| w & 1 == 1)
}

const LOG10_2: f64 = 0.301_029_995_663_981_2;

/// The number of decimal digits of `c` (1 for zero).
pub fn ndigits(c: &BigInt) -> u64 {
    if let Some(v) = c.to_i128() {
        return v.unsigned_abs().checked_ilog10().map_or(1, |l| l as u64 + 1);
    }
    let bl = c.bit_len() as f64;
    let lo = ((bl - 1.0) * LOG10_2 - 1e-6).floor() as u64;
    let hi = (bl * LOG10_2 + 1e-6).floor() as u64;
    if lo == hi {
        return lo + 1;
    }
    let mut k = hi + 1;
    let a = c.abs();
    while k > lo + 1 && a.cmp(&raw_pow10(k - 1)) == Ordering::Less {
        k -= 1;
    }
    k
}

/// The number of trailing decimal zeros of a non-zero coefficient.
pub fn trailing_zeros10(c: &BigInt) -> u64 {
    if c.is_zero() {
        return 0;
    }
    if let Some(mut v) = c.to_i128() {
        let mut n = 0;
        while v % 10 == 0 {
            v /= 10;
            n += 1;
        }
        return n;
    }
    // Binary search on the largest k with 10^k | c, bounded by the trailing binary zeros.
    let bound = c.trailing_zeros() as u64;
    let (mut lo, mut hi) = (0u64, bound.min(ndigits(c) - 1));
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let divisible = pow10(mid).ok().and_then(|p| c.rem(&p)).is_some_and(|r| r.is_zero());
        if divisible {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// `c * 10^n`.
pub fn mul_pow10(c: &BigInt, n: u64) -> R<BigInt> {
    if c.is_zero() || n == 0 {
        return Ok(c.clone());
    }
    if ndigits(c).saturating_add(n) > MAX_DIGITS {
        return Err(Mem);
    }
    Ok(c.mul(&pow10(n)?))
}

/// `(c / 10^n, c % 10^n)` for a non-negative `c`.
pub fn divmod_pow10(c: &BigInt, n: u64) -> (BigInt, BigInt) {
    if n == 0 {
        return (c.clone(), BigInt::zero());
    }
    if n > ndigits(c) {
        return (BigInt::zero(), c.clone());
    }
    let p = raw_pow10(n);
    c.divmod_floor(&p).unwrap_or_else(|| (BigInt::zero(), c.clone()))
}

/// Divides the non-negative `c` by `10^k` and rounds the quotient: returns the rounded quotient
/// and whether digits were discarded (inexact).
pub fn round_div_pow10(c: &BigInt, k: u64, negative: bool, mode: Rounding) -> R<(BigInt, bool)> {
    if k == 0 {
        return Ok((c.clone(), false));
    }
    if c.is_zero() {
        return Ok((BigInt::zero(), false));
    }
    let nd = ndigits(c);
    if k > nd {
        let up = match mode {
            Rounding::Up => true,
            Rounding::Ceiling => !negative,
            Rounding::Floor => negative,
            Rounding::Up05 => true,
            _ => false,
        };
        return Ok((BigInt::from_u64(up as u64), true));
    }
    if let (Some(v), true) = (c.to_i128(), k <= 38) {
        let v = v as u128;
        let d = 10u128.pow(k as u32);
        let (q, r) = (v / d, v % d);
        let half = if r == 0 { Ordering::Less } else { (r.checked_mul(2).unwrap_or(u128::MAX)).cmp(&d) };
        let inc = decide(mode, negative, r != 0, half, q & 1 == 1, q % 5 == 0);
        let q = BigInt::from_i128(q as i128);
        return Ok((if inc { q.add(&BigInt::from_u64(1)) } else { q }, r != 0));
    }
    let d = pow10(k)?;
    let (q, r) = c.divmod_floor(&d).ok_or(Mem)?;
    if r.is_zero() {
        return Ok((q, false));
    }
    let half = r.add(&r).cmp(&d);
    let inc = decide(mode, negative, true, half, is_odd(&q), q.rem(&BigInt::from_u64(5)).is_some_and(|m| m.is_zero()));
    Ok((if inc { q.add(&BigInt::from_u64(1)) } else { q }, true))
}

/// `half` compares twice the discarded tail with the unit of the last kept digit.
fn decide(mode: Rounding, negative: bool, nonzero: bool, half: Ordering, odd: bool, mult5: bool) -> bool {
    if !nonzero {
        return false;
    }
    match mode {
        Rounding::Up => true,
        Rounding::Down => false,
        Rounding::Ceiling => !negative,
        Rounding::Floor => negative,
        Rounding::HalfUp => half != Ordering::Less,
        Rounding::HalfDown => half == Ordering::Greater,
        Rounding::HalfEven => half == Ordering::Greater || (half == Ordering::Equal && odd),
        Rounding::Up05 => mult5,
    }
}

/// Removes up to `limit` trailing zeros: returns the stripped coefficient and the count removed.
pub fn strip_zeros(c: &BigInt, limit: u64) -> (BigInt, u64) {
    if c.is_zero() || limit == 0 {
        return (c.clone(), 0);
    }
    let n = trailing_zeros10(c).min(limit);
    if n == 0 {
        return (c.clone(), 0);
    }
    (divmod_pow10(c, n).0, n)
}

pub fn big(v: u64) -> BigInt {
    BigInt::from_u64(v)
}
