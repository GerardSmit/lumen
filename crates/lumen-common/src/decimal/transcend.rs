//! Correctly rounded square root, exponential and logarithms.
//!
//! The integer kernels (`ilog`, `iexp`, `dlog`, `dlog10`, `dexp`, `dpower`) evaluate in fixed
//! point on [`BigInt`]s with a provable error bound of one unit in the last place; the callers
//! raise the working precision by three digits at a time until the result is unambiguously
//! roundable, so every result is the correctly rounded one.

use super::arith::{guard, invalid, zero_of};
use super::coef::{self, big, mul_pow10, ndigits, pow10, Mem, R};
use super::{flag, sat, Context, Decimal, Rounding, Status};
use crate::bigint::BigInt;
use std::cell::RefCell;
use std::cmp::Ordering;

pub(crate) fn p10(n: i128) -> R<BigInt> {
    if n < 0 {
        return Err(Mem);
    }
    pow10(u64::try_from(n).map_err(|_| Mem)?)
}

/// The closest integer to `a / b` (ties to even) for a positive `b`.
pub(crate) fn div_nearest(a: &BigInt, b: &BigInt) -> BigInt {
    let (q, r) = a.divmod_floor(b).unwrap_or_else(|| (BigInt::zero(), BigInt::zero()));
    let odd = if coef::is_odd(&q) { big(1) } else { BigInt::zero() };
    if r.add(&r).add(&odd).cmp(b) == Ordering::Greater {
        q.add(&big(1))
    } else {
        q
    }
}

fn rshift_nearest(x: &BigInt, shift: u64) -> BigInt {
    let q = x.shr(shift);
    let r = x.sub(&q.shl(shift));
    let odd = if coef::is_odd(&q) { big(1) } else { BigInt::zero() };
    if r.add(&r).add(&odd).cmp(&big(1).shl(shift)) == Ordering::Greater {
        q.add(&big(1))
    } else {
        q
    }
}

/// The closest integer to `sqrt(n)`, starting from the approximation `a`.
fn sqrt_nearest(n: &BigInt, a: &BigInt) -> BigInt {
    let mut a = a.clone();
    let mut b = BigInt::zero();
    while a.cmp(&b) != Ordering::Equal {
        b = a.clone();
        let (q, r) = n.divmod_floor(&a).unwrap_or_else(|| (BigInt::zero(), BigInt::zero()));
        let ceil = if r.is_zero() { q } else { q.add(&big(1)) };
        a = a.add(&ceil).shr(1);
    }
    a
}

const L: u64 = 8;

/// Number of Taylor terms for a modulus `m`: `ceil(10 * len(str(m)) / (3 * L))`.
fn taylor_terms(m: &BigInt) -> u64 {
    (10 * ndigits(m)).div_ceil(3 * L)
}

/// An integer approximation to `m * log(x / m)` (error at most 22 for `0.1 <= x/m <= 10`).
fn ilog(x: &BigInt, m: &BigInt) -> BigInt {
    let mut y = x.sub(m);
    let mut r: u64 = 0;
    loop {
        let again = if r <= L { y.abs().shl(L - r).cmp(m) != Ordering::Less } else { y.abs().shr(r - L).cmp(m) != Ordering::Less };
        if !again {
            break;
        }
        let inner = m.mul(&m.add(&rshift_nearest(&y, r)));
        y = div_nearest(&m.mul(&y).shl(1), &m.add(&sqrt_nearest(&inner, m)));
        r += 1;
    }
    let t = taylor_terms(m);
    let yshift = rshift_nearest(&y, r);
    let mut w = div_nearest(m, &big(t));
    for k in (1..t).rev() {
        w = div_nearest(m, &big(k)).sub(&div_nearest(&yshift.mul(&w), m));
    }
    div_nearest(&w.mul(&y), m)
}

thread_local! {
    static LOG10_DIGITS: RefCell<String> = RefCell::new("23025850929940456840179914546843642076011014886".to_string());
}

/// `floor(10^p * ln(10))`.
fn log10_digits(p: u64) -> R<BigInt> {
    let cached = LOG10_DIGITS.with(|d| {
        let d = d.borrow();
        (p as usize) < d.len()
    });
    if !cached {
        let mut extra = 3u64;
        let digits = loop {
            let m = pow10(p + extra + 2)?;
            let digits = div_nearest(&ilog(&m.mul(&big(10)), &m), &big(100)).to_string_radix(10);
            if !digits.ends_with(&"0".repeat(extra as usize)) {
                break digits;
            }
            extra += 3;
        };
        let trimmed = digits.trim_end_matches('0');
        let keep = trimmed[..trimmed.len() - 1].to_string();
        LOG10_DIGITS.with(|d| *d.borrow_mut() = keep);
    }
    LOG10_DIGITS.with(|d| {
        let d = d.borrow();
        BigInt::parse_dec(&d[..(p as usize + 1).min(d.len())]).ok_or(Mem)
    })
}

/// `x`'s decimal digit count as Python's `len(str(x))` reports it (a sign counts).
fn str_len(x: &BigInt) -> i128 {
    ndigits(x) as i128 + x.is_negative() as i128
}

/// `f` such that `c * 10^e = d * 10^f` with `1 <= d <= 10` (or `0.1 <= d <= 1` for `f <= 0`).
fn decompose(c: &BigInt, e: i128) -> (i128, i128) {
    let l = ndigits(c) as i128;
    (l, e + l - (e + l >= 1) as i128)
}

/// `10^p * log10(c * 10^e)` with an absolute error of at most 1; `c * 10^e != 1`.
pub(crate) fn dlog10(c: &BigInt, e: i128, p: i128) -> R<BigInt> {
    let p = p + 2;
    let (_, f) = decompose(c, e);
    let (log_d, log_tenpower) = if p > 0 {
        let m = p10(p)?;
        let k = e + p - f;
        let c = if k >= 0 { mul_pow10(c, k as u64)? } else { div_nearest(c, &p10(-k)?) };
        let ld = ilog(&c, &m);
        let log_10 = log10_digits(p as u64)?;
        (div_nearest(&ld.mul(&m), &log_10), BigInt::from_i128(f).mul(&m))
    } else {
        (BigInt::zero(), div_nearest(&BigInt::from_i128(f), &p10(-p)?))
    };
    Ok(div_nearest(&log_tenpower.add(&log_d), &big(100)))
}

/// `10^p * ln(c * 10^e)` with an absolute error of at most 1; `c * 10^e != 1`.
pub(crate) fn dlog(c: &BigInt, e: i128, p: i128) -> R<BigInt> {
    let p = p + 2;
    let (_, f) = decompose(c, e);
    let log_d = if p > 0 {
        let k = e + p - f;
        let c = if k >= 0 { mul_pow10(c, k as u64)? } else { div_nearest(c, &p10(-k)?) };
        ilog(&c, &p10(p)?)
    } else {
        BigInt::zero()
    };
    let f_log_ten = if f != 0 {
        let extra = ndigits(&BigInt::from_i128(f.abs())) as i128 - 1;
        if p + extra >= 0 {
            div_nearest(&BigInt::from_i128(f).mul(&log10_digits((p + extra) as u64)?), &p10(extra)?)
        } else {
            BigInt::zero()
        }
    } else {
        BigInt::zero()
    };
    Ok(div_nearest(&f_log_ten.add(&log_d), &big(100)))
}

/// An integer approximation to `m * exp(x / m)` for small `x / m`.
fn iexp(x: &BigInt, m: &BigInt) -> BigInt {
    let (r_q, _) = x.shl(L).divmod_floor(m).unwrap_or_else(|| (BigInt::zero(), BigInt::zero()));
    let r = r_q.bit_len() as u64;
    let t = taylor_terms(m);
    let mut y = div_nearest(x, &big(t));
    let mshift = m.shl(r);
    for i in (1..t).rev() {
        y = div_nearest(&x.mul(&mshift.add(&y)), &mshift.mul(&big(i)));
    }
    for k in (0..r).rev() {
        let mshift = m.shl(k + 2);
        y = div_nearest(&y.mul(&y.add(&mshift)), &mshift);
    }
    m.add(&y)
}

/// `(d, f)` with `(d-1) * 10^f < exp(c * 10^e) < (d+1) * 10^f` and `d` of `p` digits.
pub(crate) fn dexp(c: &BigInt, e: i128, p: i128) -> R<(BigInt, i128)> {
    let p = p + 2;
    let extra = (e + str_len(c) - 1).max(0);
    let q = p + extra;
    let shift = e + q;
    let cshift = if shift >= 0 {
        mul_pow10(c, shift as u64)?
    } else {
        let d = p10(-shift)?;
        c.divmod_floor(&d).ok_or(Mem)?.0
    };
    let (quot, rem) = cshift.divmod_floor(&log10_digits(q as u64)?).ok_or(Mem)?;
    let rem = div_nearest(&rem, &p10(extra)?);
    let d = div_nearest(&iexp(&rem, &p10(p)?), &big(1000));
    let q = quot.to_i128().ok_or(Mem)?;
    Ok((d, q - p + 3))
}

/// `(c, e)` with `(c-1) * 10^e < x^y < (c+1) * 10^e` for `x = xc * 10^xe > 0`, `x != 1`,
/// `y = yc * 10^ye != 0`.
pub(crate) fn dpower(xc: &BigInt, xe: i128, yc: &BigInt, ye: i128, p: i128) -> R<(BigInt, i128)> {
    let b = ndigits(yc) as i128 + ye;
    let lxc = dlog(xc, xe, p + b + 1)?;
    let shift = ye - b;
    let pc = if shift >= 0 {
        mul_pow10(&lxc.mul(yc), shift as u64)?
    } else {
        div_nearest(&lxc.mul(yc), &p10(-shift)?)
    };
    if pc.is_zero() {
        let greater = (ndigits(xc) as i128 + xe >= 1) == !yc.is_negative();
        return Ok(if greater { (p10(p - 1)?.add(&big(1)), 1 - p) } else { (p10(p)?.sub(&big(1)), -p) });
    }
    let (coeff, exp) = dexp(&pc, -(p + 1), p + 1)?;
    Ok((div_nearest(&coeff, &big(10)), exp + 1))
}

/// Whether `coeff` is unambiguously roundable to `p` digits (not within one unit of a rounding
/// boundary).
pub(crate) fn roundable(coeff: &BigInt, p: i128) -> R<bool> {
    let n = ndigits(coeff) as i128 - p - 1;
    if n < 0 {
        return Ok(true);
    }
    let m = p10(n)?.mul(&big(5));
    Ok(coeff.rem(&m).is_some_and(|r| !r.is_zero()))
}

fn half_even(ctx: &Context) -> Context {
    Context { round: Rounding::HalfEven, ..ctx.clone() }
}

fn digits_str_len(n: i128) -> i128 {
    n.unsigned_abs().checked_ilog10().map_or(1, |l| l as i128 + 1)
}

impl Decimal {
    /// A lower bound for the adjusted exponent of `ln(self)`; `self` finite, positive, not 1.
    fn ln_exp_bound(&self) -> R<i128> {
        let adj = self.adjusted();
        if adj >= 1 {
            return Ok(digits_str_len(adj * 23 / 10) - 1);
        }
        if adj <= -2 {
            return Ok(digits_str_len((-1 - adj) * 23 / 10) - 1);
        }
        let (c, e) = (&self.coef, self.exp as i128);
        if adj == 0 {
            let num = c.sub(&p10(-e)?).to_string_radix(10);
            let den = c.to_string_radix(10);
            return Ok(num.len() as i128 - den.len() as i128 - (num < den) as i128);
        }
        Ok(e + p10(-e)?.sub(c).to_string_radix(10).len() as i128 - 1)
    }

    /// A lower bound for the adjusted exponent of `log10(self)`.
    pub(crate) fn log10_exp_bound(&self) -> R<i128> {
        let adj = self.adjusted();
        if adj >= 1 {
            return Ok(digits_str_len(adj) - 1);
        }
        if adj <= -2 {
            return Ok(digits_str_len(-1 - adj) - 1);
        }
        let (c, e) = (&self.coef, self.exp as i128);
        if adj == 0 {
            let num = c.sub(&p10(-e)?).to_string_radix(10);
            let den = c.mul(&big(231)).to_string_radix(10);
            return Ok(num.len() as i128 - den.len() as i128 - (num < den) as i128 + 2);
        }
        let num = p10(-e)?.sub(c).to_string_radix(10);
        Ok(num.len() as i128 + e - (num.as_str() < "231") as i128 - 1)
    }

    pub fn exp(&self, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.exp_impl(ctx, st);
        guard(st, r)
    }

    fn exp_impl(&self, ctx: &Context, st: &mut Status) -> R<Decimal> {
        if let Some(n) = self.check_nans(None, ctx, st) {
            return Ok(n);
        }
        if self.is_infinite() && self.sign {
            return Ok(Decimal::zero());
        }
        if self.is_zero() {
            return Ok(Decimal::one());
        }
        if self.is_infinite() {
            return Ok(self.clone());
        }
        let p = ctx.prec as i128;
        let adj = self.adjusted();
        let emax1 = (ctx.emax as i128 + 1) * 3;
        let etiny1 = (-(ctx.etiny() as i128) + 1) * 3;
        let ans = if !self.sign && adj > digits_str_len(emax1) {
            Decimal::finite(false, big(1), sat(ctx.emax as i128 + 1))
        } else if self.sign && adj > digits_str_len(etiny1) {
            Decimal::finite(false, big(1), sat(ctx.etiny() as i128 - 1))
        } else if !self.sign && adj < -p {
            Decimal::finite(false, p10(p)?.add(&big(1)), sat(-p))
        } else if self.sign && adj < -p - 1 {
            Decimal::finite(false, p10(p + 1)?.sub(&big(1)), sat(-p - 1))
        } else {
            let c = if self.sign { self.coef.neg() } else { self.coef.clone() };
            let e = self.exp as i128;
            let mut extra = 3;
            loop {
                let (coeff, exp) = dexp(&c, e, p + extra)?;
                if roundable(&coeff, p)? {
                    break Decimal::finite(false, coeff, sat(exp));
                }
                extra += 3;
            }
        };
        ans.fix(&half_even(ctx), st)
    }

    pub fn ln(&self, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.ln_impl(ctx, st);
        guard(st, r)
    }

    fn ln_impl(&self, ctx: &Context, st: &mut Status) -> R<Decimal> {
        if let Some(n) = self.check_nans(None, ctx, st) {
            return Ok(n);
        }
        if self.is_zero() {
            return Ok(Decimal::infinity(true));
        }
        if self.is_infinite() && !self.sign {
            return Ok(Decimal::infinity(false));
        }
        if self.is_finite() && self.compare_numeric(&Decimal::one()) == Some(Ordering::Equal) {
            return Ok(Decimal::zero());
        }
        if self.sign {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        let p = ctx.prec as i128;
        let mut places = p - self.ln_exp_bound()? + 2;
        let coeff = loop {
            let coeff = dlog(&self.coef, self.exp as i128, places)?;
            if roundable(&coeff.abs(), p)? {
                break coeff;
            }
            places += 3;
        };
        Decimal::finite(coeff.is_negative(), coeff.abs(), sat(-places)).fix(&half_even(ctx), st)
    }

    pub fn log10(&self, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.log10_impl(ctx, st);
        guard(st, r)
    }

    fn log10_impl(&self, ctx: &Context, st: &mut Status) -> R<Decimal> {
        if let Some(n) = self.check_nans(None, ctx, st) {
            return Ok(n);
        }
        if self.is_zero() {
            return Ok(Decimal::infinity(true));
        }
        if self.is_infinite() && !self.sign {
            return Ok(Decimal::infinity(false));
        }
        if self.sign {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        let nd = self.digits();
        let ans = if self.coef.cmp(&pow10(nd - 1)?) == Ordering::Equal {
            Decimal::from_bigint(&BigInt::from_i128(self.exp as i128 + nd as i128 - 1))
        } else {
            let p = ctx.prec as i128;
            let mut places = p - self.log10_exp_bound()? + 2;
            let coeff = loop {
                let coeff = dlog10(&self.coef, self.exp as i128, places)?;
                if roundable(&coeff.abs(), p)? {
                    break coeff;
                }
                places += 3;
            };
            Decimal::finite(coeff.is_negative(), coeff.abs(), sat(-places))
        };
        ans.fix(&half_even(ctx), st)
    }

    pub fn sqrt(&self, ctx: &Context, st: &mut Status) -> Decimal {
        let r = self.sqrt_impl(ctx, st);
        guard(st, r)
    }

    fn sqrt_impl(&self, ctx: &Context, st: &mut Status) -> R<Decimal> {
        if self.is_special() {
            if let Some(n) = self.check_nans(None, ctx, st) {
                return Ok(n);
            }
            if self.is_infinite() && !self.sign {
                return Ok(self.clone());
            }
        }
        if self.is_zero() {
            return zero_of(self.sign, (self.exp as i128).div_euclid(2)).fix(ctx, st);
        }
        if self.sign {
            return Ok(invalid(st, flag::INVALID_OPERATION));
        }
        let prec = ctx.prec as i128 + 1;
        let exp = self.exp as i128;
        let nd = self.digits() as i128;
        let mut e = exp >> 1;
        let (mut c, l) = if exp & 1 == 1 { (mul_pow10(&self.coef, 1)?, (nd >> 1) + 1) } else { (self.coef.clone(), (nd + 1) >> 1) };
        let shift = prec - l;
        let mut exact;
        if shift >= 0 {
            c = mul_pow10(&c, 2 * shift as u64)?;
            exact = true;
        } else {
            let (q, r) = coef::divmod_pow10(&c, 2 * (-shift) as u64);
            c = q;
            exact = r.is_zero();
        }
        e -= shift;
        let mut n = c.isqrt().ok_or(Mem)?;
        exact = exact && n.mul(&n).cmp(&c) == Ordering::Equal;
        if exact {
            n = if shift >= 0 { coef::divmod_pow10(&n, shift as u64).0 } else { mul_pow10(&n, (-shift) as u64)? };
            e += shift;
        } else if n.rem(&big(5)).is_some_and(|m| m.is_zero()) {
            n = n.add(&big(1));
        }
        Decimal::finite(false, n, sat(e)).fix(&half_even(ctx), st)
    }
}
