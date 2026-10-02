//! Rounding to the context (`fit`), comparison and the basic arithmetic.

use super::coef::{self, big, mul_pow10, ndigits, pow10, round_div_pow10, Mem, R};
use super::{flag, sat, Context, Decimal, Kind, Rounding, Status};
use crate::bigint::BigInt;
use std::cmp::Ordering;

/// An invalid operation: the result is a NaN.
pub(crate) fn invalid(st: &mut Status, bit: u32) -> Decimal {
    *st |= bit;
    Decimal::quiet_nan()
}

/// Maps a memory failure to the status word and a NaN.
pub(crate) fn guard(st: &mut Status, r: R<Decimal>) -> Decimal {
    match r {
        Ok(d) => d,
        Err(Mem) => {
            *st |= flag::MALLOC_ERROR;
            Decimal::quiet_nan()
        }
    }
}

pub(crate) fn guard2(st: &mut Status, r: R<(Decimal, Decimal)>) -> (Decimal, Decimal) {
    match r {
        Ok(d) => d,
        Err(Mem) => {
            *st |= flag::MALLOC_ERROR;
            (Decimal::quiet_nan(), Decimal::quiet_nan())
        }
    }
}

pub(crate) fn zero_of(sign: bool, exp: i128) -> Decimal {
    Decimal::finite(sign, BigInt::zero(), sat(exp))
}

/// Numeric comparison of two non-NaN values.
pub(crate) fn cmp_values(a: &Decimal, b: &Decimal) -> Ordering {
    let sa = side(a);
    let sb = side(b);
    if sa != sb {
        return sa.cmp(&sb);
    }
    if sa == 0 {
        return Ordering::Equal;
    }
    if a.is_infinite() || b.is_infinite() {
        let (ia, ib) = (a.is_infinite(), b.is_infinite());
        return match (ia, ib) {
            (true, true) => Ordering::Equal,
            (true, false) => sa.cmp(&0),
            _ => 0.cmp(&sa),
        };
    }
    let mag = cmp_mag_finite(a, b);
    if sa < 0 {
        mag.reverse()
    } else {
        mag
    }
}

fn side(a: &Decimal) -> i32 {
    if a.is_zero() {
        0
    } else if a.sign {
        -1
    } else {
        1
    }
}

/// Compares the magnitudes of two non-zero finite values.
fn cmp_mag_finite(a: &Decimal, b: &Decimal) -> Ordering {
    let (aa, ab) = (a.adjusted(), b.adjusted());
    if aa != ab {
        return aa.cmp(&ab);
    }
    let diff = a.exp as i128 - b.exp as i128;
    if diff >= 0 {
        let pa = mul_pow10(&a.coef, diff as u64).unwrap_or_else(|_| a.coef.clone());
        pa.cmp(&b.coef)
    } else {
        let pb = mul_pow10(&b.coef, (-diff) as u64).unwrap_or_else(|_| b.coef.clone());
        a.coef.cmp(&pb)
    }
}

impl Decimal {
    /// A quiet NaN with the payload reduced to what the context allows.
    pub(crate) fn fix_nan(&self, ctx: &Context) -> Decimal {
        let max_len = (ctx.prec - ctx.clamp as i64).max(0) as u64;
        if !self.coef.is_zero() && self.digits() > max_len {
            let (_, r) = coef::divmod_pow10(&self.coef, max_len);
            return Decimal { coef: r, ..self.clone() };
        }
        self.clone()
    }

    fn quiet(&self) -> Decimal {
        Decimal { kind: Kind::NaN, ..self.clone() }
    }

    /// The NaN-propagation rules: a signaling NaN raises Invalid and becomes quiet; otherwise the
    /// first quiet NaN wins. `None` when neither operand is a NaN.
    pub(crate) fn check_nans(&self, other: Option<&Decimal>, ctx: &Context, st: &mut Status) -> Option<Decimal> {
        let o = other.filter(|o| o.is_nan());
        if !self.is_nan() && o.is_none() {
            return None;
        }
        if self.is_snan() {
            *st |= flag::INVALID_OPERATION;
            return Some(self.quiet().fix_nan(ctx));
        }
        if let Some(o) = o {
            if o.is_snan() {
                *st |= flag::INVALID_OPERATION;
                return Some(o.quiet().fix_nan(ctx));
            }
        }
        if self.is_nan() {
            return Some(self.fix_nan(ctx));
        }
        o.map(|o| o.fix_nan(ctx))
    }

    pub(crate) fn check_nans3(&self, b: &Decimal, c: &Decimal, ctx: &Context, st: &mut Status) -> Option<Decimal> {
        let ops = [self, b, c];
        if let Some(s) = ops.iter().find(|d| d.is_snan()) {
            *st |= flag::INVALID_OPERATION;
            return Some(s.quiet().fix_nan(ctx));
        }
        ops.iter().find(|d| d.is_nan()).map(|d| d.fix_nan(ctx))
    }

    /// Like `check_nans` for the signaling comparisons: every NaN raises Invalid.
    pub(crate) fn compare_check_nans(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Option<Decimal> {
        let pick = [self, other].into_iter().find(|d| d.is_snan()).or_else(|| [self, other].into_iter().find(|d| d.is_qnan()));
        pick.map(|d| {
            *st |= flag::INVALID_OPERATION;
            d.quiet().fix_nan(ctx)
        })
    }

    fn overflow_result(sign: bool, ctx: &Context) -> R<Decimal> {
        let inf = match ctx.round {
            Rounding::HalfUp | Rounding::HalfEven | Rounding::HalfDown | Rounding::Up => true,
            Rounding::Ceiling => !sign,
            Rounding::Floor => sign,
            Rounding::Down | Rounding::Up05 => false,
        };
        if inf {
            return Ok(Decimal::infinity(sign));
        }
        let nines = pow10(ctx.prec as u64)?.sub(&big(1));
        Ok(Decimal::finite(sign, nines, sat(ctx.emax as i128 - ctx.prec as i128 + 1)))
    }

    /// Rounds to the context's precision and exponent range, raising the conditions of the
    /// specification (`finalize` in libmpdec, `_fix` in `_pydecimal`).
    pub(crate) fn fix(&self, ctx: &Context, st: &mut Status) -> R<Decimal> {
        match self.kind {
            Kind::NaN | Kind::SNaN => return Ok(self.fix_nan(ctx)),
            Kind::Infinity => return Ok(self.clone()),
            Kind::Finite => {}
        }
        let etiny = ctx.etiny() as i128;
        let etop = ctx.etop() as i128;
        let exp = self.exp as i128;
        if self.coef.is_zero() {
            let exp_max = if ctx.clamp { etop } else { ctx.emax as i128 };
            let new_exp = exp.max(etiny).min(exp_max);
            if new_exp != exp {
                *st |= flag::CLAMPED;
                return Ok(zero_of(self.sign, new_exp));
            }
            return Ok(self.clone());
        }
        let nd = self.digits() as i128;
        let prec = ctx.prec as i128;
        let mut exp_min = nd + exp - prec;
        if exp_min > etop {
            *st |= flag::OVERFLOW | flag::INEXACT | flag::ROUNDED;
            return Self::overflow_result(self.sign, ctx);
        }
        let subnormal = exp_min < etiny;
        if subnormal {
            exp_min = etiny;
        }
        if exp < exp_min {
            let k = (exp_min - exp) as u64;
            let (mut q, inexact) = round_div_pow10(&self.coef, k, self.sign, ctx.round)?;
            if ndigits(&q) as i128 > prec {
                q = coef::divmod_pow10(&q, 1).0;
                exp_min += 1;
            }
            let ans = if exp_min > etop {
                *st |= flag::OVERFLOW;
                Self::overflow_result(self.sign, ctx)?
            } else {
                Decimal::finite(self.sign, q, sat(exp_min))
            };
            if inexact && subnormal {
                *st |= flag::UNDERFLOW;
            }
            if subnormal {
                *st |= flag::SUBNORMAL;
            }
            if inexact {
                *st |= flag::INEXACT;
            }
            *st |= flag::ROUNDED;
            if ans.is_zero() {
                *st |= flag::CLAMPED;
            }
            return Ok(ans);
        }
        if subnormal {
            *st |= flag::SUBNORMAL;
            return Ok(self.clone());
        }
        if ctx.clamp && exp > etop {
            *st |= flag::CLAMPED;
            let padded = mul_pow10(&self.coef, (exp - etop) as u64)?;
            return Ok(Decimal::finite(self.sign, padded, sat(etop)));
        }
        Ok(self.clone())
    }

    /// Rounds to the context (the result of a unary `plus` without the zero-sign handling).
    pub fn fit(&self, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.fix(ctx, st);
        guard(st, r)
    }

    /// Rescales to exponent `exp` using `mode`, without raising any condition (`_rescale`).
    pub(crate) fn rescale(&self, exp: i128, mode: Rounding) -> R<Decimal> {
        if self.is_special() {
            return Ok(self.clone());
        }
        if self.coef.is_zero() {
            return Ok(zero_of(self.sign, exp));
        }
        let cur = self.exp as i128;
        if cur >= exp {
            let c = mul_pow10(&self.coef, (cur - exp) as u64)?;
            return Ok(Decimal::finite(self.sign, c, sat(exp)));
        }
        let (q, _) = round_div_pow10(&self.coef, (exp - cur) as u64, self.sign, mode)?;
        Ok(Decimal::finite(self.sign, q, sat(exp)))
    }

    /// Rounds a non-zero finite value to `places` significant digits, quietly (`_round`).
    pub(crate) fn round_places(&self, places: u64, mode: Rounding) -> R<Decimal> {
        if self.is_special() || self.coef.is_zero() {
            return Ok(self.clone());
        }
        let adj = self.adjusted();
        let ans = self.rescale(adj + 1 - places as i128, mode)?;
        if ans.adjusted() != adj {
            return ans.rescale(ans.adjusted() + 1 - places as i128, mode);
        }
        Ok(ans)
    }

    /// `+self` rounded to the context.
    pub fn plus(&self, ctx: &Context, st: &mut Status) -> Decimal {
        if let Some(n) = self.check_nans(None, ctx, st) {
            return n;
        }
        let base = if self.is_zero() && ctx.round != Rounding::Floor { self.copy_abs() } else { self.clone() };
        base.fit(ctx, st)
    }

    /// `-self` rounded to the context.
    pub fn minus(&self, ctx: &Context, st: &mut Status) -> Decimal {
        if let Some(n) = self.check_nans(None, ctx, st) {
            return n;
        }
        let base = if self.is_zero() && ctx.round != Rounding::Floor { self.copy_abs() } else { self.copy_negate() };
        base.fit(ctx, st)
    }

    /// `abs(self)` rounded to the context.
    pub fn abs(&self, ctx: &Context, st: &mut Status) -> Decimal {
        if let Some(n) = self.check_nans(None, ctx, st) {
            return n;
        }
        if self.sign {
            self.minus(ctx, st)
        } else {
            self.plus(ctx, st)
        }
    }

    pub fn add(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.add_impl(other, ctx, st);
        guard(st, r)
    }

    pub fn sub(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        if let Some(n) = self.check_nans(Some(other), ctx, st) {
            return n;
        }
        self.add(&other.copy_negate(), ctx, st)
    }

    fn add_impl(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> R<Decimal> {
        if self.is_special() || other.is_special() {
            if let Some(n) = self.check_nans(Some(other), ctx, st) {
                return Ok(n);
            }
            if self.is_infinite() {
                if self.sign != other.sign && other.is_infinite() {
                    return Ok(invalid(st, flag::INVALID_OPERATION));
                }
                return Ok(self.clone());
            }
            if other.is_infinite() {
                return Ok(other.clone());
            }
        }
        let exp = self.exp.min(other.exp) as i128;
        let negative_zero = ctx.round == Rounding::Floor && self.sign != other.sign;
        if self.is_zero() && other.is_zero() {
            let sign = (self.sign && other.sign) || negative_zero;
            return zero_of(sign, exp).fix(ctx, st);
        }
        if self.is_zero() {
            let e = exp.max(other.exp as i128 - ctx.prec as i128 - 1);
            return other.rescale(e, ctx.round)?.fix(ctx, st);
        }
        if other.is_zero() {
            let e = exp.max(self.exp as i128 - ctx.prec as i128 - 1);
            return self.rescale(e, ctx.round)?.fix(ctx, st);
        }
        let (c1, c2, e) = align(self, other, ctx.prec)?;
        let (sign, mag) = if self.sign != other.sign {
            match c1.cmp(&c2) {
                Ordering::Equal => return zero_of(negative_zero, exp).fix(ctx, st),
                Ordering::Greater => (self.sign, c1.sub(&c2)),
                Ordering::Less => (other.sign, c2.sub(&c1)),
            }
        } else {
            (self.sign, c1.add(&c2))
        };
        Decimal::finite(sign, mag, e).fix(ctx, st)
    }

    pub fn mul(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.mul_impl(other, ctx, st);
        guard(st, r)
    }

    fn mul_impl(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> R<Decimal> {
        let sign = self.sign ^ other.sign;
        if self.is_special() || other.is_special() {
            if let Some(n) = self.check_nans(Some(other), ctx, st) {
                return Ok(n);
            }
            if self.is_infinite() {
                if other.is_zero() {
                    return Ok(invalid(st, flag::INVALID_OPERATION));
                }
                return Ok(Decimal::infinity(sign));
            }
            if other.is_infinite() {
                if self.is_zero() {
                    return Ok(invalid(st, flag::INVALID_OPERATION));
                }
                return Ok(Decimal::infinity(sign));
            }
        }
        let exp = self.exp as i128 + other.exp as i128;
        if self.is_zero() || other.is_zero() {
            return zero_of(sign, exp).fix(ctx, st);
        }
        let c = self.coef.checked_mul(&other.coef).map_err(|_| Mem)?;
        Decimal::finite(sign, c, sat(exp)).fix(ctx, st)
    }

    pub fn div(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.div_impl(other, ctx, st);
        guard(st, r)
    }

    fn div_impl(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> R<Decimal> {
        let sign = self.sign ^ other.sign;
        if self.is_special() || other.is_special() {
            if let Some(n) = self.check_nans(Some(other), ctx, st) {
                return Ok(n);
            }
            if self.is_infinite() {
                if other.is_infinite() {
                    return Ok(invalid(st, flag::INVALID_OPERATION));
                }
                return Ok(Decimal::infinity(sign));
            }
            if other.is_infinite() {
                *st |= flag::CLAMPED;
                return Ok(zero_of(sign, ctx.etiny() as i128));
            }
        }
        if other.is_zero() {
            if self.is_zero() {
                return Ok(invalid(st, flag::DIVISION_UNDEFINED));
            }
            *st |= flag::DIVISION_BY_ZERO;
            return Ok(Decimal::infinity(sign));
        }
        let ideal = self.exp as i128 - other.exp as i128;
        if self.is_zero() {
            return zero_of(sign, ideal).fix(ctx, st);
        }
        let prec = ctx.prec as i128;
        let shift = other.digits() as i128 - self.digits() as i128 + prec + 1;
        if shift > 4096 {
            if let Some(exact) = self.div_exact_terminating(other, sign, ideal)? {
                return exact.fix(ctx, st);
            }
        }
        if shift > coef::MAX_DIGITS as i128 {
            return Err(Mem);
        }
        let (q, r) = if shift >= 0 {
            let n = mul_pow10(&self.coef, shift as u64)?;
            n.divmod_floor(&other.coef).ok_or(Mem)?
        } else {
            let d = mul_pow10(&other.coef, (-shift) as u64)?;
            self.coef.divmod_floor(&d).ok_or(Mem)?
        };
        let mut exp = ideal - shift;
        let mut coeff = q;
        if !r.is_zero() {
            if coeff.rem(&big(5)).is_some_and(|m| m.is_zero()) {
                coeff = coeff.add(&big(1));
            }
        } else {
            let room = (ideal - exp).max(0) as u64;
            let (c, n) = coef::strip_zeros(&coeff, room);
            coeff = c;
            exp += n as i128;
        }
        Decimal::finite(sign, coeff, sat(exp)).fix(ctx, st)
    }

    /// The quotient when it terminates within a few digits of the operands (used when the context
    /// precision is far larger than the operands): `None` when it does not.
    fn div_exact_terminating(&self, other: &Decimal, sign: bool, ideal: i128) -> R<Option<Decimal>> {
        let s2 = other.digits() * 4 + 2;
        let n = mul_pow10(&self.coef, s2)?;
        let (q, r) = n.divmod_floor(&other.coef).ok_or(Mem)?;
        if !r.is_zero() {
            return Ok(None);
        }
        let exp = ideal - s2 as i128;
        let room = (ideal - exp) as u64;
        let (c, k) = coef::strip_zeros(&q, room);
        Ok(Some(Decimal::finite(sign, c, sat(exp + k as i128))))
    }

    /// `(self // other, self % other)` for a finite `self` and a non-zero `other` (non-NaN).
    fn divide(&self, other: &Decimal, ctx: &Context) -> R<Option<(Decimal, Decimal)>> {
        let sign = self.sign ^ other.sign;
        let ideal = if other.is_infinite() { self.exp as i128 } else { self.exp.min(other.exp) as i128 };
        let expdiff = self.adjusted() - other.adjusted();
        if self.is_zero() || other.is_infinite() || expdiff <= -2 {
            return Ok(Some((zero_of(sign, 0), self.rescale(ideal, ctx.round)?)));
        }
        if expdiff <= ctx.prec as i128 {
            let (mut c1, mut c2) = (self.coef.clone(), other.coef.clone());
            if self.exp >= other.exp {
                c1 = mul_pow10(&c1, (self.exp as i128 - other.exp as i128) as u64)?;
            } else {
                c2 = mul_pow10(&c2, (other.exp as i128 - self.exp as i128) as u64)?;
            }
            let (q, r) = c1.divmod_floor(&c2).ok_or(Mem)?;
            if ndigits(&q) as i128 <= ctx.prec as i128 {
                return Ok(Some((Decimal::finite(sign, q, 0), Decimal::finite(self.sign, r, sat(ideal)))));
            }
        }
        Ok(None)
    }

    pub fn divmod(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> (Decimal, Decimal) {
        let r = self.divmod_impl(other, ctx, st);
        guard2(st, r)
    }

    fn divmod_impl(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> R<(Decimal, Decimal)> {
        if let Some(n) = self.check_nans(Some(other), ctx, st) {
            return Ok((n.clone(), n));
        }
        let sign = self.sign ^ other.sign;
        if self.is_infinite() {
            if other.is_infinite() {
                let n = invalid(st, flag::INVALID_OPERATION);
                return Ok((n.clone(), n));
            }
            return Ok((Decimal::infinity(sign), invalid(st, flag::INVALID_OPERATION)));
        }
        if other.is_zero() {
            if self.is_zero() {
                let n = invalid(st, flag::DIVISION_UNDEFINED);
                return Ok((n.clone(), n));
            }
            *st |= flag::DIVISION_BY_ZERO;
            return Ok((Decimal::infinity(sign), invalid(st, flag::INVALID_OPERATION)));
        }
        match self.divide(other, ctx)? {
            Some((q, r)) => Ok((q, r.fix(ctx, st)?)),
            None => {
                let n = invalid(st, flag::DIVISION_IMPOSSIBLE);
                Ok((n.clone(), n))
            }
        }
    }

    pub fn divint(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.divint_impl(other, ctx, st);
        guard(st, r)
    }

    fn divint_impl(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> R<Decimal> {
        if let Some(n) = self.check_nans(Some(other), ctx, st) {
            return Ok(n);
        }
        if self.is_infinite() {
            if other.is_infinite() {
                return Ok(invalid(st, flag::INVALID_OPERATION));
            }
            return Ok(Decimal::infinity(self.sign ^ other.sign));
        }
        if other.is_zero() {
            if self.is_zero() {
                return Ok(invalid(st, flag::DIVISION_UNDEFINED));
            }
            *st |= flag::DIVISION_BY_ZERO;
            return Ok(Decimal::infinity(self.sign ^ other.sign));
        }
        match self.divide(other, ctx)? {
            Some((q, _)) => Ok(q),
            None => Ok(invalid(st, flag::DIVISION_IMPOSSIBLE)),
        }
    }

    pub fn rem(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.rem_impl(other, ctx, st);
        guard(st, r)
    }

    fn rem_impl(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> R<Decimal> {
        if let Some(n) = self.check_nans(Some(other), ctx, st) {
            return Ok(n);
        }
        if self.is_infinite() {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        if other.is_zero() {
            return Ok(if self.is_zero() {
                invalid(st, flag::DIVISION_UNDEFINED)
            } else {
                invalid(st, flag::INVALID_OPERATION)
            });
        }
        match self.divide(other, ctx)? {
            Some((_, r)) => r.fix(ctx, st),
            None => Ok(invalid(st, flag::DIVISION_IMPOSSIBLE)),
        }
    }

    pub fn rem_near(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.rem_near_impl(other, ctx, st);
        guard(st, r)
    }

    fn rem_near_impl(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> R<Decimal> {
        if let Some(n) = self.check_nans(Some(other), ctx, st) {
            return Ok(n);
        }
        if self.is_infinite() {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        if other.is_zero() {
            return Ok(if self.is_zero() {
                invalid(st, flag::DIVISION_UNDEFINED)
            } else {
                invalid(st, flag::INVALID_OPERATION)
            });
        }
        if other.is_infinite() {
            return self.fix(ctx, st);
        }
        let ideal = self.exp.min(other.exp) as i128;
        if self.is_zero() {
            return zero_of(self.sign, ideal).fix(ctx, st);
        }
        let expdiff = self.adjusted() - other.adjusted();
        if expdiff >= ctx.prec as i128 + 1 {
            return Ok(invalid(st, flag::DIVISION_IMPOSSIBLE));
        }
        if expdiff <= -2 {
            return self.rescale(ideal, ctx.round)?.fix(ctx, st);
        }
        let (mut c1, mut c2) = (self.coef.clone(), other.coef.clone());
        if self.exp >= other.exp {
            c1 = mul_pow10(&c1, (self.exp as i128 - other.exp as i128) as u64)?;
        } else {
            c2 = mul_pow10(&c2, (other.exp as i128 - self.exp as i128) as u64)?;
        }
        let (mut q, mut r) = c1.divmod_floor(&c2).ok_or(Mem)?;
        let twice = r.add(&r).add(&if coef::is_odd(&q) { big(1) } else { BigInt::zero() });
        let mut sign = self.sign;
        if twice.cmp(&c2) == Ordering::Greater {
            r = r.sub(&c2);
            q = q.add(&big(1));
        }
        if ndigits(&q) as i128 > ctx.prec as i128 {
            return Ok(invalid(st, flag::DIVISION_IMPOSSIBLE));
        }
        if r.is_negative() {
            sign = !sign;
            r = r.neg();
        }
        Decimal::finite(sign, r, sat(ideal)).fix(ctx, st)
    }

    /// `self * other + third` with a single rounding.
    pub fn fma(&self, other: &Decimal, third: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.fma_impl(other, third, ctx, st);
        guard(st, r)
    }

    fn fma_impl(&self, other: &Decimal, third: &Decimal, ctx: &Context, st: &mut Status) -> R<Decimal> {
        if self.is_nan() || other.is_nan() || third.is_nan() {
            if let Some(n) = self.check_nans3(other, third, ctx, st) {
                return Ok(n);
            }
        }
        let sign = self.sign ^ other.sign;
        let product = if self.is_special() || other.is_special() {
            if self.is_infinite() && other.is_zero() || other.is_infinite() && self.is_zero() {
                return Ok(invalid(st, flag::INVALID_OPERATION));
            }
            Decimal::infinity(sign)
        } else {
            let c = self.coef.checked_mul(&other.coef).map_err(|_| Mem)?;
            Decimal::finite(sign, c, sat(self.exp as i128 + other.exp as i128))
        };
        product.add_impl(third, ctx, st)
    }

    /// Numeric comparison as a decimal (`-1`, `0`, `1`, or NaN).
    pub fn compare(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        if self.is_special() || other.is_special() {
            if let Some(n) = self.check_nans(Some(other), ctx, st) {
                return n;
            }
        }
        Decimal::from_i64(ordering_int(cmp_values(self, other)))
    }

    /// Like `compare`, but any NaN raises Invalid.
    pub fn compare_signal(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        if let Some(n) = self.compare_check_nans(other, ctx, st) {
            return n;
        }
        Decimal::from_i64(ordering_int(cmp_values(self, other)))
    }

    pub fn max(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        self.minmax(other, true, false, ctx, st)
    }

    pub fn min(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        self.minmax(other, false, false, ctx, st)
    }

    pub fn max_mag(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        self.minmax(other, true, true, ctx, st)
    }

    pub fn min_mag(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        self.minmax(other, false, true, ctx, st)
    }

    fn minmax(&self, other: &Decimal, want_max: bool, mag: bool, ctx: &Context, st: &mut Status) -> Decimal {
        if self.is_special() || other.is_special() {
            let (sn, on) = (self.is_nan(), other.is_nan());
            if sn || on {
                if other.is_qnan() && !sn {
                    return self.fit(ctx, st);
                }
                if self.is_qnan() && !on {
                    return other.fit(ctx, st);
                }
                return self.check_nans(Some(other), ctx, st).unwrap_or_else(Decimal::quiet_nan);
            }
        }
        let mut c = if mag { cmp_values(&self.copy_abs(), &other.copy_abs()) } else { cmp_values(self, other) };
        if c == Ordering::Equal {
            c = self.compare_total_order(other);
        }
        let ans = match (want_max, c == Ordering::Less) {
            (true, true) | (false, false) => other,
            _ => self,
        };
        ans.fit(ctx, st)
    }
}

pub(crate) fn ordering_int(o: Ordering) -> i64 {
    match o {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// Aligns two non-zero finite operands for addition (`_normalize`): returns the shifted
/// coefficients and the common exponent. The smaller operand is replaced by a sticky digit when
/// it lies entirely below the precision.
fn align(a: &Decimal, b: &Decimal, prec: i64) -> R<(BigInt, BigInt, i64)> {
    let a_is_tmp = a.exp >= b.exp;
    let (tmp, other) = if a_is_tmp { (a, b) } else { (b, a) };
    let tmp_len = tmp.digits() as i128;
    let other_len = other.digits() as i128;
    let exp = tmp.exp as i128 + (-1i128).min(tmp_len - prec as i128 - 2);
    let (oc, oe) = if other_len + other.exp as i128 - 1 < exp { (big(1), exp) } else { (other.coef.clone(), other.exp as i128) };
    let tc = mul_pow10(&tmp.coef, (tmp.exp as i128 - oe) as u64)?;
    let oe = sat(oe);
    Ok(if a_is_tmp { (tc, oc, oe) } else { (oc, tc, oe) })
}
