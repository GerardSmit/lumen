//! Conversions: numeric strings (to-scientific / to-engineering), integers, floats, integer
//! ratios and the numeric hash.

use super::coef::{self, Mem, R};
use super::{sat, Decimal, Kind};
use crate::bigint::BigInt;

/// The text is not a numeric string of the General Decimal Arithmetic Specification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParseError;

/// The modulus of the numeric hash (`sys.hash_info.modulus` on 64-bit platforms).
pub const HASH_MODULUS: u64 = (1 << 61) - 1;
pub const HASH_INF: i64 = 314_159;

fn mulmod(a: u64, b: u64) -> u64 {
    ((a as u128 * b as u128) % HASH_MODULUS as u128) as u64
}

fn powmod(mut base: u64, mut e: u64) -> u64 {
    let mut acc = 1u64;
    base %= HASH_MODULUS;
    while e > 0 {
        if e & 1 == 1 {
            acc = mulmod(acc, base);
        }
        base = mulmod(base, base);
        e >>= 1;
    }
    acc
}

impl Decimal {
    /// Parses an ASCII numeric string (no surrounding whitespace, no underscores): an optional
    /// sign, then digits with an optional point and exponent, `Inf`/`Infinity`, or `NaN`/`sNaN`
    /// with optional payload digits. Case-insensitive. The value is exact, never rounded; an
    /// exponent beyond the representable range is saturated.
    pub fn parse(text: &str) -> Result<Decimal, ParseError> {
        let b = text.as_bytes();
        let (sign, rest) = match b.first() {
            Some(b'-') => (true, &b[1..]),
            Some(b'+') => (false, &b[1..]),
            _ => (false, b),
        };
        let lower: Vec<u8> = rest.iter().map(|c| c.to_ascii_lowercase()).collect();
        if lower == b"inf" || lower == b"infinity" {
            return Ok(Decimal::infinity(sign));
        }
        let (signaling, tail) = if lower.starts_with(b"snan") {
            (true, &rest[4..])
        } else if lower.starts_with(b"nan") {
            (false, &rest[3..])
        } else {
            return Self::parse_finite(sign, rest);
        };
        if !tail.iter().all(u8::is_ascii_digit) {
            return Err(ParseError);
        }
        let digits = std::str::from_utf8(tail).map_err(|_| ParseError)?.trim_start_matches('0');
        let payload = if digits.is_empty() { BigInt::zero() } else { BigInt::parse_dec(digits).ok_or(ParseError)? };
        Ok(Decimal::nan(sign, signaling, payload))
    }

    fn parse_finite(sign: bool, s: &[u8]) -> Result<Decimal, ParseError> {
        let int_end = s.iter().position(|c| !c.is_ascii_digit()).unwrap_or(s.len());
        let int = &s[..int_end];
        let mut i = int_end;
        let mut frac: &[u8] = &[];
        if s.get(i) == Some(&b'.') {
            let start = i + 1;
            let end = s[start..].iter().position(|c| !c.is_ascii_digit()).map_or(s.len(), |p| start + p);
            frac = &s[start..end];
            i = end;
        }
        if int.is_empty() && frac.is_empty() {
            return Err(ParseError);
        }
        let mut exp: i128 = 0;
        if i < s.len() {
            if !matches!(s[i], b'e' | b'E') {
                return Err(ParseError);
            }
            i += 1;
            let neg = match s.get(i) {
                Some(b'-') => {
                    i += 1;
                    true
                }
                Some(b'+') => {
                    i += 1;
                    false
                }
                _ => false,
            };
            let digits = &s[i..];
            if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
                return Err(ParseError);
            }
            for d in digits {
                exp = (exp * 10 + (d - b'0') as i128).min(1 << 100);
            }
            if neg {
                exp = -exp;
            }
        }
        let mut all = Vec::with_capacity(int.len() + frac.len());
        all.extend_from_slice(int);
        all.extend_from_slice(frac);
        let text = std::str::from_utf8(&all).map_err(|_| ParseError)?;
        let trimmed = text.trim_start_matches('0');
        let coef = if trimmed.is_empty() { BigInt::zero() } else { BigInt::parse_dec(trimmed).ok_or(ParseError)? };
        Ok(Decimal::finite(sign, coef, sat(exp - frac.len() as i128)))
    }

    /// The exact value of a float (NaN keeps its sign bit).
    pub fn from_f64(v: f64) -> R<Decimal> {
        let sign = v.is_sign_negative();
        if v.is_nan() {
            return Ok(Decimal::nan(sign, false, BigInt::zero()));
        }
        if v.is_infinite() {
            return Ok(Decimal::infinity(sign));
        }
        let bits = v.abs().to_bits();
        let biased = ((bits >> 52) & 0x7ff) as i64;
        let frac = bits & ((1 << 52) - 1);
        let (mut m, mut e) = if biased == 0 { (frac, -1074) } else { (frac | (1 << 52), biased - 1075) };
        if m == 0 {
            return Ok(Decimal::finite(sign, BigInt::zero(), 0));
        }
        let tz = m.trailing_zeros() as i64;
        m >>= tz;
        e += tz;
        let m = BigInt::from_u64(m);
        if e >= 0 {
            return Ok(Decimal::finite(sign, m.shl(e as u64), 0));
        }
        let k = (-e) as u64;
        let five_k = BigInt::from_u64(5).pow(&BigInt::from_u64(k)).map_err(|_| Mem)?;
        Ok(Decimal::finite(sign, m.mul(&five_k), -(k as i64)))
    }

    /// The nearest float (correctly rounded; infinities for overflow). `None` for a signaling NaN.
    pub fn to_f64(&self) -> Option<f64> {
        let neg = if self.sign { -1.0 } else { 1.0 };
        match self.kind {
            Kind::SNaN => None,
            Kind::NaN => Some(f64::NAN.copysign(neg)),
            Kind::Infinity => Some(f64::INFINITY * neg),
            Kind::Finite => {
                let text = format!("{}e{}", self.coef.to_string_radix(10), self.exp);
                Some(text.parse::<f64>().unwrap_or(f64::NAN) * neg)
            }
        }
    }

    /// The integer part of a finite value (truncating toward zero). [`Mem`] when the exponent
    /// makes the integer too large to build.
    pub fn to_bigint_trunc(&self) -> R<BigInt> {
        let mag = if self.exp >= 0 {
            coef::mul_pow10(&self.coef, self.exp as u64)?
        } else {
            coef::divmod_pow10(&self.coef, self.exp.unsigned_abs()).0
        };
        Ok(if self.sign { mag.neg() } else { mag })
    }

    /// `(numerator, denominator)` in lowest terms for a finite value.
    pub fn as_integer_ratio(&self) -> R<(BigInt, BigInt)> {
        if self.coef.is_zero() {
            return Ok((BigInt::zero(), BigInt::from_u64(1)));
        }
        let (mut n, d) = if self.exp >= 0 {
            (coef::mul_pow10(&self.coef, self.exp as u64)?, BigInt::from_u64(1))
        } else {
            let mut n = self.coef.clone();
            let mut d5 = self.exp.unsigned_abs();
            let five27 = BigInt::from_u64(5u64.pow(27));
            let five = BigInt::from_u64(5);
            while d5 >= 27 {
                match n.divmod_floor(&five27) {
                    Some((q, r)) if r.is_zero() => {
                        n = q;
                        d5 -= 27;
                    }
                    _ => break,
                }
            }
            while d5 > 0 {
                match n.divmod_floor(&five) {
                    Some((q, r)) if r.is_zero() => {
                        n = q;
                        d5 -= 1;
                    }
                    _ => break,
                }
            }
            let mut d2 = self.exp.unsigned_abs();
            let shift2 = (n.trailing_zeros() as u64).min(d2);
            if shift2 > 0 {
                n = n.shr(shift2);
                d2 -= shift2;
            }
            let five_d5 = BigInt::from_u64(5).pow(&BigInt::from_u64(d5)).map_err(|_| Mem)?;
            let d = five_d5.checked_shl(d2 as u128).map_err(|_| Mem)?;
            (n, d)
        };
        if self.sign {
            n = n.neg();
        }
        Ok((n, d))
    }

    /// The numeric hash, equal to the hash of the equal int, float or fraction. Infinities hash to
    /// `±HASH_INF`; NaNs have no numeric hash (`None`).
    pub fn numeric_hash(&self) -> Option<i64> {
        match self.kind {
            Kind::NaN | Kind::SNaN => None,
            Kind::Infinity => Some(if self.sign { -HASH_INF } else { HASH_INF }),
            Kind::Finite => {
                let exp_hash = if self.exp >= 0 {
                    powmod(10, self.exp as u64)
                } else {
                    let inv10 = powmod(10, HASH_MODULUS - 2);
                    powmod(inv10, self.exp.unsigned_abs())
                };
                let c = self
                    .coef
                    .rem(&BigInt::from_u64(HASH_MODULUS))
                    .and_then(|r| r.to_i64())
                    .unwrap_or(0) as u64;
                let h = mulmod(c, exp_hash) as i64;
                let ans = if self.sign { -h } else { h };
                Some(if ans == -1 { -2 } else { ans })
            }
        }
    }

    /// Scientific notation as the specification's `to-scientific-string`.
    pub fn to_sci_string(&self, capitals: bool) -> String {
        self.to_string_inner(false, capitals)
    }

    /// Engineering notation as the specification's `to-engineering-string`.
    pub fn to_eng_string(&self, capitals: bool) -> String {
        self.to_string_inner(true, capitals)
    }

    fn to_string_inner(&self, eng: bool, capitals: bool) -> String {
        let sign = if self.sign { "-" } else { "" };
        match self.kind {
            Kind::Infinity => return format!("{sign}Infinity"),
            Kind::NaN | Kind::SNaN => {
                let payload = if self.coef.is_zero() { String::new() } else { self.coef.to_string_radix(10) };
                let tag = if self.kind == Kind::SNaN { "sNaN" } else { "NaN" };
                return format!("{sign}{tag}{payload}");
            }
            Kind::Finite => {}
        }
        let digits = self.coef.to_string_radix(10);
        let ndig = digits.len() as i128;
        let leftdigits = self.exp as i128 + ndig;
        let dotplace: i128 = if self.exp <= 0 && leftdigits > -6 {
            leftdigits
        } else if !eng {
            1
        } else if self.coef.is_zero() {
            (leftdigits + 1).rem_euclid(3) - 1
        } else {
            (leftdigits - 1).rem_euclid(3) + 1
        };
        let mut out = String::with_capacity(digits.len() + 16);
        out.push_str(sign);
        if dotplace <= 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat('0').take((-dotplace) as usize));
            out.push_str(&digits);
        } else if dotplace >= ndig {
            out.push_str(&digits);
            out.extend(std::iter::repeat('0').take((dotplace - ndig) as usize));
        } else {
            out.push_str(&digits[..dotplace as usize]);
            out.push('.');
            out.push_str(&digits[dotplace as usize..]);
        }
        if leftdigits != dotplace {
            out.push(if capitals { 'E' } else { 'e' });
            out.push_str(&format!("{:+}", leftdigits - dotplace));
        }
        out
    }
}
