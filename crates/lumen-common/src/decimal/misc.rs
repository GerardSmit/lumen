//! Quantization, integral rounding, scaling, neighbours, total ordering and classification.

use super::arith::{cmp_values, guard, invalid, ordering_int, zero_of};
use super::coef::{self, big, pow10, R};
use super::{flag, sat, Class, Context, Decimal, Kind, Rounding, Status};
use crate::bigint::BigInt;
use std::cmp::Ordering;

fn nan_rank(d: &Decimal) -> u8 {
    match d.kind {
        Kind::NaN => 1,
        Kind::SNaN => 2,
        _ => 0,
    }
}

/// The ordering of `compare_total` for any two values.
pub(crate) fn total_order(a: &Decimal, b: &Decimal) -> Ordering {
    match (a.sign, b.sign) {
        (true, false) => return Ordering::Less,
        (false, true) => return Ordering::Greater,
        _ => {}
    }
    let sign = a.sign;
    let flip = |o: Ordering| if sign { o.reverse() } else { o };
    let (an, bn) = (nan_rank(a), nan_rank(b));
    if an != 0 || bn != 0 {
        if an == bn {
            return flip(a.coef.cmp(&b.coef));
        }
        let greater = if sign { Ordering::Less } else { Ordering::Greater };
        let less = greater.reverse();
        // A quiet NaN orders beyond a signaling NaN, which orders beyond every number.
        if an == 1 {
            return greater;
        }
        if bn == 1 {
            return less;
        }
        if an == 2 {
            return greater;
        }
        return less;
    }
    match cmp_values(a, b) {
        Ordering::Equal => {}
        o => return o,
    }
    flip(a.exp.cmp(&b.exp))
}

impl Decimal {
    pub fn is_normal(&self, ctx: &Context) -> bool {
        self.is_finite() && !self.is_zero() && ctx.emin as i128 <= self.adjusted()
    }

    pub fn is_subnormal(&self, ctx: &Context) -> bool {
        self.is_finite() && !self.is_zero() && self.adjusted() < ctx.emin as i128
    }

    pub fn number_class(&self, ctx: &Context) -> Class {
        match self.kind {
            Kind::SNaN => Class::SNaN,
            Kind::NaN => Class::NaN,
            Kind::Infinity => {
                if self.sign {
                    Class::NegInfinity
                } else {
                    Class::PosInfinity
                }
            }
            Kind::Finite => {
                let sub = self.is_subnormal(ctx);
                match (self.is_zero(), sub, self.sign) {
                    (true, _, true) => Class::NegZero,
                    (true, _, false) => Class::PosZero,
                    (false, true, true) => Class::NegSubnormal,
                    (false, true, false) => Class::PosSubnormal,
                    (false, false, true) => Class::NegNormal,
                    (false, false, false) => Class::PosNormal,
                }
            }
        }
    }

    pub fn radix() -> Decimal {
        Decimal::from_i64(10)
    }

    /// `compare_total` as a decimal (`-1`, `0`, `1`).
    pub fn compare_total(&self, other: &Decimal) -> Decimal {
        Decimal::from_i64(ordering_int(self.compare_total_order(other)))
    }

    pub fn compare_total_mag(&self, other: &Decimal) -> Decimal {
        Decimal::from_i64(ordering_int(self.compare_total_mag_order(other)))
    }

    /// Sets the exponent to that of `exp`, rounding with `mode`.
    pub fn quantize(&self, exp: &Decimal, mode: Rounding, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.quantize_impl(exp, mode, ctx, st);
        guard(st, r)
    }

    fn quantize_impl(&self, exp: &Decimal, mode: Rounding, ctx: &Context, st: &mut Status) -> R<Decimal> {
        if self.is_special() || exp.is_special() {
            if let Some(n) = self.check_nans(Some(exp), ctx, st) {
                return Ok(n);
            }
            if exp.is_infinite() || self.is_infinite() {
                if exp.is_infinite() && self.is_infinite() {
                    return Ok(self.clone());
                }
                return Ok(invalid(st, flag::INVALID_OPERATION));
            }
        }
        let target = exp.exp as i128;
        if !(ctx.etiny() as i128 <= target && target <= ctx.emax as i128) {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        if self.is_zero() {
            return zero_of(self.sign, target).fix(ctx, st);
        }
        let adj = self.adjusted();
        if adj > ctx.emax as i128 || adj - target + 1 > ctx.prec as i128 {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        let ans = self.rescale(target, mode)?;
        if ans.adjusted() > ctx.emax as i128 || ans.digits() as i128 > ctx.prec as i128 {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        if !ans.is_zero() && ans.adjusted() < ctx.emin as i128 {
            *st |= flag::SUBNORMAL;
        }
        if ans.exp > self.exp {
            if cmp_values(&ans, self) != Ordering::Equal {
                *st |= flag::INEXACT;
            }
            *st |= flag::ROUNDED;
        }
        ans.fix(ctx, st)
    }

    /// Rounds to an integer, raising Inexact and Rounded when digits are discarded.
    pub fn to_integral_exact(&self, mode: Rounding, ctx: &Context, st: &mut Status) -> Decimal {
        if self.is_special() {
            return self.check_nans(None, ctx, st).unwrap_or_else(|| self.clone());
        }
        if self.exp >= 0 {
            return self.clone();
        }
        if self.is_zero() {
            return zero_of(self.sign, 0);
        }
        let r = self.rescale(0, mode);
        let ans = guard(st, r);
        if cmp_values(&ans, self) != Ordering::Equal {
            *st |= flag::INEXACT;
        }
        *st |= flag::ROUNDED;
        ans
    }

    /// Rounds to an integer without raising Inexact or Rounded.
    pub fn to_integral_value(&self, mode: Rounding, ctx: &Context, st: &mut Status) -> Decimal {
        if self.is_special() {
            return self.check_nans(None, ctx, st).unwrap_or_else(|| self.clone());
        }
        if self.exp >= 0 {
            return self.clone();
        }
        let r = self.rescale(0, mode);
        guard(st, r)
    }

    /// The integer nearest to a finite value under `mode`.
    pub fn to_bigint_rounded(&self, mode: Rounding) -> R<BigInt> {
        self.rescale(0, mode)?.to_bigint_trunc()
    }

    /// The value with trailing zeros removed (zero becomes `0e0`).
    pub fn normalize(&self, ctx: &Context, st: &mut Status) -> Decimal {
        if self.is_special() {
            if let Some(n) = self.check_nans(None, ctx, st) {
                return n;
            }
        }
        let dup = self.fit(ctx, st);
        if dup.is_infinite() {
            return dup;
        }
        if dup.is_zero() {
            return zero_of(dup.sign, 0);
        }
        let exp_max = (if ctx.clamp { ctx.etop() } else { ctx.emax }) as i128;
        let room = (exp_max - dup.exp as i128).max(0) as u64;
        let (c, n) = coef::strip_zeros(&dup.coef, room);
        Decimal::finite(dup.sign, c, sat(dup.exp as i128 + n as i128))
    }

    /// The exponent of the most significant digit, as a decimal.
    pub fn logb(&self, ctx: &Context, st: &mut Status) -> Decimal {
        if let Some(n) = self.check_nans(None, ctx, st) {
            return n;
        }
        if self.is_infinite() {
            return Decimal::infinity(false);
        }
        if self.is_zero() {
            *st |= flag::DIVISION_BY_ZERO;
            return Decimal::infinity(true);
        }
        Decimal::from_bigint(&BigInt::from_i128(self.adjusted())).fit(ctx, st)
    }

    /// `self * 10**other` for an integral `other` of moderate size.
    pub fn scaleb(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        if let Some(n) = self.check_nans(Some(other), ctx, st) {
            return n;
        }
        let Some(n) = other.small_integer() else { return invalid(st, flag::INVALID_OPERATION) };
        let lim = 2 * (ctx.emax as i128 + ctx.prec as i128);
        if !(-lim <= n && n <= lim) {
            return invalid(st, flag::INVALID_OPERATION);
        }
        if self.is_infinite() {
            return self.clone();
        }
        Decimal::finite(self.sign, self.coef.clone(), sat(self.exp as i128 + n)).fit(ctx, st)
    }

    /// The value of a finite operand with exponent zero as an `i128` (the shape `scaleb`, `rotate`
    /// and `shift` demand of their second operand).
    pub(crate) fn small_integer(&self) -> Option<i128> {
        if !self.is_finite() || self.exp != 0 {
            return None;
        }
        let v = self.coef.to_i128()?;
        Some(if self.sign { -v } else { v })
    }

    /// The largest representable number below `self`.
    pub fn next_minus(&self, ctx: &Context, st: &mut Status) -> Decimal {
        self.next_dir(false, ctx, st)
    }

    /// The smallest representable number above `self`.
    pub fn next_plus(&self, ctx: &Context, st: &mut Status) -> Decimal {
        self.next_dir(true, ctx, st)
    }

    fn next_dir(&self, up: bool, ctx: &Context, st: &mut Status) -> Decimal {
        if let Some(n) = self.check_nans(None, ctx, st) {
            return n;
        }
        if self.is_infinite() {
            if self.sign == up {
                let nines = match pow10(ctx.prec as u64) {
                    Ok(p) => p.sub(&big(1)),
                    Err(_) => {
                        *st |= flag::MALLOC_ERROR;
                        return Decimal::quiet_nan();
                    }
                };
                return Decimal::finite(self.sign, nines, ctx.etop());
            }
            return self.clone();
        }
        let mut work = ctx.clone();
        work.round = if up { Rounding::Ceiling } else { Rounding::Floor };
        let mut ignored: Status = 0;
        let fitted = self.fit(&work, &mut ignored);
        if cmp_values(&fitted, self) != Ordering::Equal {
            return fitted;
        }
        let tiny = Decimal::finite(false, big(1), sat(ctx.etiny() as i128 - 1));
        let r = if up { self.add(&tiny, &work, &mut ignored) } else { self.sub(&tiny, &work, &mut ignored) };
        if ignored & flag::MALLOC_ERROR != 0 {
            *st |= flag::MALLOC_ERROR;
        }
        r
    }

    /// The neighbour of `self` in the direction of `other`.
    pub fn next_toward(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        if let Some(n) = self.check_nans(Some(other), ctx, st) {
            return n;
        }
        let ans = match cmp_values(self, other) {
            Ordering::Equal => return self.copy_sign(other),
            Ordering::Less => self.next_plus(ctx, st),
            Ordering::Greater => self.next_minus(ctx, st),
        };
        if ans.is_infinite() {
            *st |= flag::OVERFLOW | flag::INEXACT | flag::ROUNDED;
        } else if ans.adjusted() < ctx.emin as i128 {
            *st |= flag::UNDERFLOW | flag::SUBNORMAL | flag::INEXACT | flag::ROUNDED;
            if ans.is_zero() {
                *st |= flag::CLAMPED;
            }
        }
        ans
    }
}
