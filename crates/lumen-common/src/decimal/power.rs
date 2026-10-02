//! Exponentiation: `x ** y` (exact when representable, correctly rounded otherwise) and the
//! three-argument modular form.

use super::arith::{cmp_values, guard, invalid, zero_of};
use super::coef::{self, big, mul_pow10, ndigits, pow10, strip_zeros, Mem, R};
use super::transcend::{dpower, p10, roundable};
use super::{flag, sat, Context, Decimal, Rounding, Status};
use crate::bigint::BigInt;
use std::cmp::Ordering;

fn str_len_i128(n: i128) -> i128 {
    n.unsigned_abs().checked_ilog10().map_or(1, |l| l as i128 + 1)
}

/// `n * 10^e` when that is an integer, else `None`, without building large powers of ten.
fn lshift_exact(n: &BigInt, e: i128) -> R<Option<BigInt>> {
    if n.is_zero() {
        return Ok(Some(BigInt::zero()));
    }
    if e >= 0 {
        return Ok(Some(mul_pow10(n, u64::try_from(e).map_err(|_| Mem)?)?));
    }
    let val = coef::trailing_zeros10(&n.abs()) as i128;
    if val < -e {
        return Ok(None);
    }
    let d = p10(-e)?;
    Ok(n.divmod_floor(&d).map(|(q, _)| q))
}

/// A lower bound for `100 * log10(c)`.
fn log10_lb(c: &BigInt) -> i128 {
    const CORRECTION: [i128; 10] = [0, 100, 70, 53, 40, 31, 23, 16, 10, 5];
    let s = c.to_string_radix(10);
    let first = (s.as_bytes()[0] - b'0') as usize;
    100 * s.len() as i128 - CORRECTION[first]
}

fn to_u64(v: &BigInt) -> R<u64> {
    v.to_i64().and_then(|x| u64::try_from(x).ok()).ok_or(Mem)
}

fn is_power_of_two(c: &BigInt) -> bool {
    c.trailing_zeros() + 1 == c.bit_len()
}

fn big_to_i128_sat(v: &BigInt) -> i128 {
    v.to_i128().unwrap_or(if v.is_negative() { -(1i128 << 100) } else { 1i128 << 100 })
}

impl Decimal {
    /// `self ** other` (two-argument form).
    pub fn pow(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.pow_impl(other, ctx, st);
        guard(st, r)
    }

    fn pow_impl(&self, other: &Decimal, ctx: &Context, st: &mut Status) -> R<Decimal> {
        if let Some(n) = self.check_nans(Some(other), ctx, st) {
            return Ok(n);
        }
        if other.is_zero() {
            if self.is_zero() {
                return Ok(invalid(st, flag::INVALID_OPERATION));
            }
            return Ok(Decimal::one());
        }
        let mut result_sign = false;
        let mut x = self.clone();
        if self.sign {
            if other.is_integer() {
                if !other.is_even() {
                    result_sign = true;
                }
            } else if !self.is_zero() {
                return Ok(invalid(st, flag::INVALID_OPERATION));
            }
            x = self.copy_negate();
        }
        if x.is_zero() {
            return Ok(if other.sign { Decimal::infinity(result_sign) } else { zero_of(result_sign, 0) });
        }
        if x.is_infinite() {
            return Ok(if other.sign { zero_of(result_sign, 0) } else { Decimal::infinity(result_sign) });
        }
        let prec = ctx.prec as i128;
        if cmp_values(&x, &Decimal::one()) == Ordering::Equal {
            let exp;
            if other.is_integer() {
                let multiplier: i128 = if other.sign {
                    0
                } else {
                    let n = other.to_bigint_trunc()?;
                    if n.cmp(&BigInt::from_i64(ctx.prec)) == Ordering::Greater {
                        prec
                    } else {
                        n.to_i128().unwrap_or(0)
                    }
                };
                let mut e = x.exp as i128 * multiplier;
                if e < 1 - prec {
                    e = 1 - prec;
                    *st |= flag::ROUNDED;
                }
                exp = e;
            } else {
                *st |= flag::INEXACT | flag::ROUNDED;
                exp = 1 - prec;
            }
            return Ok(Decimal::finite(result_sign, pow10((-exp) as u64)?, sat(exp)));
        }
        let self_adj = x.adjusted();
        if other.is_infinite() {
            return Ok(if !other.sign == (self_adj < 0) { zero_of(result_sign, 0) } else { Decimal::infinity(result_sign) });
        }

        let bound = x.log10_exp_bound()? + other.adjusted();
        let mut ans: Option<Decimal> = None;
        let mut exact = false;
        if (self_adj >= 0) == !other.sign {
            if bound >= str_len_i128(ctx.emax as i128) {
                ans = Some(Decimal::finite(result_sign, big(1), sat(ctx.emax as i128 + 1)));
            }
        } else {
            let etiny = ctx.etiny() as i128;
            if bound >= str_len_i128(-etiny) {
                ans = Some(Decimal::finite(result_sign, big(1), sat(etiny - 1)));
            }
        }
        if ans.is_none() {
            if let Some(a) = x.power_exact(other, prec + 1)? {
                ans = Some(Decimal::finite(result_sign, a.coef, a.exp));
                exact = true;
            }
        }
        let ans = match ans {
            Some(a) => a,
            None => {
                let yc = if other.sign { other.coef.neg() } else { other.coef.clone() };
                let mut extra = 3;
                loop {
                    let (coeff, exp) = dpower(&x.coef, x.exp as i128, &yc, other.exp as i128, prec + extra)?;
                    if roundable(&coeff, prec)? {
                        break Decimal::finite(result_sign, coeff, sat(exp));
                    }
                    extra += 3;
                }
            }
        };
        if exact && !other.is_integer() {
            let mut ans = ans;
            let nd = ans.digits() as i128;
            if nd <= prec {
                let pad = (prec + 1 - nd) as u64;
                ans = Decimal::finite(ans.sign, mul_pow10(&ans.coef, pad)?, sat(ans.exp as i128 - pad as i128));
            }
            let mut ns: Status = 0;
            let ans = ans.fix(ctx, &mut ns)?;
            ns |= flag::INEXACT;
            if ns & flag::SUBNORMAL != 0 {
                ns |= flag::UNDERFLOW;
            }
            *st |= ns & (flag::OVERFLOW | flag::UNDERFLOW | flag::SUBNORMAL | flag::INEXACT | flag::ROUNDED | flag::CLAMPED);
            return Ok(ans);
        }
        ans.fix(ctx, st)
    }

    /// The exact value of `self ** other` when it has at most `p` digits; `None` otherwise.
    /// `self` is finite, positive and not 1, `other` finite and non-zero.
    fn power_exact(&self, other: &Decimal, p: i128) -> R<Option<Decimal>> {
        let (mut xc, k) = strip_zeros(&self.coef, u64::MAX);
        let mut xe = self.exp as i128 + k as i128;
        let (yc, k) = strip_zeros(&other.coef, u64::MAX);
        let mut ye = other.exp as i128 + k as i128;
        let int_nonneg = other.is_integer() && !other.sign;

        if xc.cmp(&big(1)) == Ordering::Equal {
            let mut xe_b = BigInt::from_i128(xe).mul(&yc);
            let neg = xe_b.is_negative();
            let (mag, k) = strip_zeros(&xe_b.abs(), u64::MAX);
            xe_b = if neg { mag.neg() } else { mag };
            ye += k as i128;
            if ye < 0 {
                return Ok(None);
            }
            let mut exponent = mul_pow10(&xe_b, u64::try_from(ye).map_err(|_| Mem)?)?;
            if other.sign {
                exponent = exponent.neg();
            }
            let zeros = if int_nonneg {
                let ideal = BigInt::from_i128(self.exp as i128).mul(&other.to_bigint_trunc()?);
                let diff = exponent.sub(&ideal);
                let cap = BigInt::from_i128(p - 1);
                if diff.cmp(&cap) == Ordering::Less { diff } else { cap }
            } else {
                BigInt::zero()
            };
            let zeros_u = if zeros.is_negative() { 0 } else { to_u64(&zeros)? };
            let e = big_to_i128_sat(&exponent.sub(&zeros));
            return Ok(Some(Decimal::finite(false, pow10(zeros_u)?, sat(e))));
        }

        if other.sign {
            let last = xc.rem(&big(10)).and_then(|r| r.to_i64()).unwrap_or(0);
            let e: BigInt;
            let emax;
            let new_xc;
            if matches!(last, 2 | 4 | 6 | 8) {
                if !is_power_of_two(&xc) {
                    return Ok(None);
                }
                let e0 = BigInt::from_u64(xc.bit_len() as u64 - 1);
                emax = p * 93 / 65;
                if ye >= str_len_i128(emax) {
                    return Ok(None);
                }
                let (Some(e1), Some(xe1)) = (lshift_exact(&e0.mul(&yc), ye)?, lshift_exact(&BigInt::from_i128(xe).mul(&yc), ye)?) else {
                    return Ok(None);
                };
                if e1.cmp(&BigInt::from_i128(emax)) == Ordering::Greater {
                    return Ok(None);
                }
                xe = big_to_i128_sat(&xe1);
                new_xc = BigInt::from_u64(5).pow(&e1).map_err(|_| Mem)?;
                e = e1;
            } else if last == 5 {
                let mut e0 = BigInt::from_u64(xc.bit_len() as u64 * 28 / 65);
                let five_e = BigInt::from_u64(5).pow(&e0).map_err(|_| Mem)?;
                let (q, r) = five_e.divmod_floor(&xc).ok_or(Mem)?;
                if !r.is_zero() {
                    return Ok(None);
                }
                let mut q = q;
                loop {
                    match q.divmod_floor(&big(5)) {
                        Some((q5, r5)) if r5.is_zero() => {
                            q = q5;
                            e0 = e0.sub(&big(1));
                        }
                        _ => break,
                    }
                }
                emax = p * 10 / 3;
                if ye >= str_len_i128(emax) {
                    return Ok(None);
                }
                let (Some(e1), Some(xe1)) = (lshift_exact(&e0.mul(&yc), ye)?, lshift_exact(&BigInt::from_i128(xe).mul(&yc), ye)?) else {
                    return Ok(None);
                };
                if e1.cmp(&BigInt::from_i128(emax)) == Ordering::Greater {
                    return Ok(None);
                }
                xe = big_to_i128_sat(&xe1);
                new_xc = BigInt::from_u64(2).pow(&e1).map_err(|_| Mem)?;
                e = e1;
            } else {
                return Ok(None);
            }
            xc = new_xc;
            let digits = ndigits(&xc) as i128;
            if digits > p {
                return Ok(None);
            }
            let xe_out = -big_to_i128_sat(&e) - xe;
            return Ok(Some(Decimal::finite(false, xc, sat(xe_out))));
        }

        let xc_bits = xc.bit_len() as i128;
        let (m, n): (BigInt, BigInt);
        if ye >= 0 {
            if ndigits(&yc) as i128 + ye > 40 {
                return Ok(None);
            }
            m = mul_pow10(&yc, ye as u64)?;
            n = big(1);
        } else {
            if xe != 0 && str_len_i128_big(&BigInt::from_i128(xe).mul(&yc)) <= -ye {
                return Ok(None);
            }
            if str_len_i128_big(&yc.mul(&BigInt::from_u64(xc_bits as u64))) <= -ye {
                return Ok(None);
            }
            let mut m0 = yc.clone();
            let mut n0 = p10(-ye)?;
            for f in [2u64, 5] {
                let fb = big(f);
                loop {
                    let (qm, rm) = m0.divmod_floor(&fb).ok_or(Mem)?;
                    let (qn, rn) = n0.divmod_floor(&fb).ok_or(Mem)?;
                    if rm.is_zero() && rn.is_zero() {
                        m0 = qm;
                        n0 = qn;
                    } else {
                        break;
                    }
                }
            }
            m = m0;
            n = n0;
        }

        if n.cmp(&big(1)) == Ordering::Greater {
            if BigInt::from_u64(xc_bits as u64).cmp(&n) != Ordering::Greater {
                return Ok(None);
            }
            let nu = to_u64(&n)?;
            let (q, rem) = BigInt::from_i128(xe).divmod_floor(&n).ok_or(Mem)?;
            if !rem.is_zero() {
                return Ok(None);
            }
            xe = big_to_i128_sat(&q);
            let shift = (xc_bits as u64).div_ceil(nu);
            let mut a = big(1).shl(shift);
            let nm1 = BigInt::from_u64(nu - 1);
            let (a_final, q_final, r_final) = loop {
                let p_a = a.pow(&nm1).map_err(|_| Mem)?;
                let (q, r) = xc.divmod_floor(&p_a).ok_or(Mem)?;
                if a.cmp(&q) != Ordering::Greater {
                    break (a, q, r);
                }
                a = a.mul(&nm1).add(&q).divmod_floor(&n).ok_or(Mem)?.0;
            };
            if a_final.cmp(&q_final) != Ordering::Equal || !r_final.is_zero() {
                return Ok(None);
            }
            xc = a_final;
        }

        if xc.cmp(&big(1)) == Ordering::Greater {
            let limit = p * 100 / log10_lb(&xc);
            if m.cmp(&BigInt::from_i128(limit)) == Ordering::Greater {
                return Ok(None);
            }
        }
        let mu = to_u64(&m)?;
        xc = xc.pow(&BigInt::from_u64(mu)).map_err(|_| Mem)?;
        xe = xe.checked_mul(mu as i128).ok_or(Mem)?;
        let digits = ndigits(&xc) as i128;
        if digits > p {
            return Ok(None);
        }
        let zeros: i128 = if int_nonneg {
            let ideal = self.exp as i128 * big_to_i128_sat(&other.to_bigint_trunc()?);
            (xe - ideal).min(p - digits).max(0)
        } else {
            0
        };
        let coef = mul_pow10(&xc, zeros as u64)?;
        Ok(Some(Decimal::finite(false, coef, sat(xe - zeros))))
    }

    /// `pow(self, other, modulo)`: the exact `(self ** other) % modulo` of three integers.
    pub fn pow_mod(&self, other: &Decimal, modulo: &Decimal, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.pow_mod_impl(other, modulo, ctx, st);
        guard(st, r)
    }

    fn pow_mod_impl(&self, other: &Decimal, modulo: &Decimal, ctx: &Context, st: &mut Status) -> R<Decimal> {
        if self.is_nan() || other.is_nan() || modulo.is_nan() {
            if let Some(n) = self.check_nans3(other, modulo, ctx, st) {
                return Ok(n);
            }
        }
        if !(self.is_integer() && other.is_integer() && modulo.is_integer()) {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        if other.sign && !other.is_zero() {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        if modulo.is_zero() {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        if modulo.adjusted() >= ctx.prec as i128 {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        if other.is_zero() && self.is_zero() {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        let sign = if other.is_even() { false } else { self.sign };
        let m = modulo.to_bigint_trunc()?.abs();
        let base = self.to_integral_value(Rounding::HalfEven, ctx, &mut 0);
        let exponent = other.to_integral_value(Rounding::HalfEven, ctx, &mut 0);
        let ten = big(10);
        let b0 = base.coef.rem(&m).ok_or(Mem)?;
        let scale = ten.pow_mod(&BigInt::from_i64(base.exp), &m).ok_or(Mem)?;
        let mut acc = b0.mul(&scale).rem(&m).ok_or(Mem)?;
        if !exponent.coef.is_zero() {
            if exponent.exp > 10_000_000 {
                return Err(Mem);
            }
            for _ in 0..exponent.exp {
                acc = acc.pow_mod(&ten, &m).ok_or(Mem)?;
            }
        }
        let res = acc.pow_mod(&exponent.coef, &m).ok_or(Mem)?;
        Ok(Decimal::finite(sign, res, 0))
    }
}

fn str_len_i128_big(n: &BigInt) -> i128 {
    ndigits(&n.abs()) as i128
}
