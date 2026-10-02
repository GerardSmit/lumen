//! Digit-wise logical operations, rotate and shift.

use super::arith::{guard, invalid};
use super::coef::{Mem, MAX_DIGITS, R};
use super::{flag, Context, Decimal, Status};
use crate::bigint::BigInt;

fn is_logical(d: &Decimal) -> bool {
    d.is_finite() && !d.sign && d.exp == 0 && d.coef.to_string_radix(10).bytes().all(|c| c == b'0' || c == b'1')
}

/// The coefficient digits, left-padded with zeros or cut on the left to exactly `prec` digits.
fn fitted_digits(d: &Decimal, prec: usize) -> Vec<u8> {
    let s = d.coef.to_string_radix(10).into_bytes();
    if s.len() >= prec {
        return s[s.len() - prec..].to_vec();
    }
    let mut v = vec![b'0'; prec - s.len()];
    v.extend_from_slice(&s);
    v
}

fn from_digits(sign: bool, digits: &[u8], exp: i64) -> R<Decimal> {
    let start = digits.iter().position(|&c| c != b'0').unwrap_or(digits.len());
    let text = std::str::from_utf8(&digits[start..]).map_err(|_| Mem)?;
    let coef = if text.is_empty() { BigInt::zero() } else { BigInt::parse_dec(text).ok_or(Mem)? };
    Ok(Decimal::finite(sign, coef, exp))
}

fn prec_len(ctx: &Context) -> R<usize> {
    if ctx.prec as u64 > MAX_DIGITS {
        return Err(Mem);
    }
    Ok(ctx.prec as usize)
}

impl Decimal {
    pub fn logical_and(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        self.logical_op(other, ctx, st, |a, b| a & b)
    }

    pub fn logical_or(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        self.logical_op(other, ctx, st, |a, b| a | b)
    }

    pub fn logical_xor(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        self.logical_op(other, ctx, st, |a, b| a ^ b)
    }

    pub fn logical_invert(&self, ctx: &Context, st: &mut Status) -> Decimal {
        let ones = match prec_len(ctx) {
            Ok(n) => from_digits(false, &vec![b'1'; n], 0),
            Err(e) => Err(e),
        };
        match ones {
            Ok(ones) => self.logical_op(&ones, ctx, st, |a, b| a ^ b),
            Err(_) => guard(st, Err(Mem)),
        }
    }

    fn logical_op(&self, other: &Decimal, ctx: &Context, st: &mut Status, f: fn(u8, u8) -> u8) -> Decimal {
        if !is_logical(self) || !is_logical(other) {
            return invalid(st, flag::INVALID_OPERATION);
        }
        let r = prec_len(ctx).and_then(|n| {
            let (a, b) = (fitted_digits(self, n), fitted_digits(other, n));
            let out: Vec<u8> = a.iter().zip(&b).map(|(&x, &y)| b'0' + f(x - b'0', y - b'0')).collect();
            from_digits(false, &out, 0)
        });
        guard(st, r)
    }

    /// The shift count operand of `rotate` / `shift`: an integral decimal of exponent zero in
    /// `[-prec, prec]`.
    fn shift_count(other: &Decimal, ctx: &Context) -> Option<i64> {
        let n = other.small_integer()?;
        (-(ctx.prec as i128) <= n && n <= ctx.prec as i128).then_some(n as i64)
    }

    pub fn rotate(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        if let Some(n) = self.check_nans(Some(other), ctx, st) {
            return n;
        }
        let Some(count) = Self::shift_count(other, ctx) else { return invalid(st, flag::INVALID_OPERATION) };
        if self.is_infinite() {
            return self.clone();
        }
        let r = prec_len(ctx).and_then(|n| {
            let digits = fitted_digits(self, n);
            let t = count.rem_euclid(n as i64) as usize;
            let mut out = digits[t..].to_vec();
            out.extend_from_slice(&digits[..t]);
            from_digits(self.sign, &out, self.exp)
        });
        guard(st, r)
    }

    pub fn shift(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        if let Some(n) = self.check_nans(Some(other), ctx, st) {
            return n;
        }
        let Some(count) = Self::shift_count(other, ctx) else { return invalid(st, flag::INVALID_OPERATION) };
        if self.is_infinite() {
            return self.clone();
        }
        let r = prec_len(ctx).and_then(|n| {
            let digits = fitted_digits(self, n);
            let out = if count < 0 {
                digits[..n - count.unsigned_abs() as usize].to_vec()
            } else {
                let t = count as usize;
                let mut v = digits[t..].to_vec();
                v.extend(std::iter::repeat(b'0').take(t));
                v
            };
            from_digits(self.sign, &out, self.exp)
        });
        guard(st, r)
    }
}
