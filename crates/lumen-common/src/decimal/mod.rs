//! Arbitrary-precision decimal arithmetic after the General Decimal Arithmetic Specification
//! (IEEE 754-2008 decimal semantics, as libmpdec implements it): signed zeros, infinities, quiet
//! and signalling NaNs with payloads, contexts with precision / exponent range / rounding / clamp,
//! and the status flags the specification raises.
//!
//! The API knows no language. A value is a [`Decimal`] (`sign`, [`BigInt`] coefficient, `i64`
//! exponent, or a special). Every operation takes a [`Context`] and a status word: it returns the
//! result the specification prescribes for the conditions it raised (an infinity on overflow, a
//! NaN for an invalid operation, ...) and ORs the raised conditions into the status word. Whether
//! a condition traps is the host's decision (`ctx.traps & status`); [`Context::raise`] ORs the
//! status into the context's flags and reports what trapped.
//!
//! # Representation
//! The coefficient is a [`BigInt`] rather than a base-10^19 limb vector. BigInt already carries
//! Karatsuba / NTT multiplication, Burnikel–Ziegler division and divide-and-conquer radix
//! conversion; a decimal limb vector would make digit shifts and digit counts trivial but would
//! need all of that machinery again (and the project forbids duplicating it). The digit-oriented
//! steps are cheap on BigInt: a digit count is one bit length plus at most two comparisons against
//! cached powers of ten, and shifting by `k` digits is one multiplication or one division by
//! `10^k`, which BigInt performs with its fast paths (u128 arithmetic below 128 bits, so the
//! common 28-digit contexts never leave machine words in the hot paths).
//!
//! # Limits
//! 64-bit libmpdec limits: [`MAX_PREC`], [`MAX_EMAX`], [`MIN_EMIN`], [`MIN_ETINY`]. Operations
//! whose coefficient would exceed the allocation ceiling raise [`flag::MALLOC_ERROR`], which hosts
//! map to an out-of-memory error.

mod arith;
mod coef;
mod convert;
mod format;
mod logical;
mod misc;
mod power;
mod transcend;

pub use coef::{Mem, MAX_DIGITS};
pub use convert::{ParseError, HASH_INF, HASH_MODULUS};
pub use format::{parse_format_spec, FormatError, FormatSpec, Locale};

#[cfg(test)]
mod tests;

use crate::bigint::BigInt;
use std::cmp::Ordering;

pub const MAX_PREC: i64 = 999_999_999_999_999_999;
pub const MAX_EMAX: i64 = 999_999_999_999_999_999;
pub const MIN_EMIN: i64 = -999_999_999_999_999_999;
pub const MIN_ETINY: i64 = MIN_EMIN - (MAX_PREC - 1);

/// Condition bits, numerically equal to libmpdec's status flags.
pub mod flag {
    pub const CLAMPED: u32 = 1;
    pub const CONVERSION_SYNTAX: u32 = 2;
    pub const DIVISION_BY_ZERO: u32 = 4;
    pub const DIVISION_IMPOSSIBLE: u32 = 8;
    pub const DIVISION_UNDEFINED: u32 = 16;
    pub const INEXACT: u32 = 64;
    pub const INVALID_CONTEXT: u32 = 128;
    pub const INVALID_OPERATION: u32 = 256;
    pub const MALLOC_ERROR: u32 = 512;
    /// libmpdec has no such condition; `_decimal` reuses `NOT_IMPLEMENTED` for `FloatOperation`.
    pub const FLOAT_OPERATION: u32 = 1024;
    pub const OVERFLOW: u32 = 2048;
    pub const ROUNDED: u32 = 4096;
    pub const SUBNORMAL: u32 = 8192;
    pub const UNDERFLOW: u32 = 16384;
    /// Every way an operation can be invalid.
    pub const IEEE_INVALID_OPERATION: u32 =
        CONVERSION_SYNTAX | DIVISION_IMPOSSIBLE | DIVISION_UNDEFINED | INVALID_CONTEXT | INVALID_OPERATION | MALLOC_ERROR;
    /// The conditions a context can trap or flag (the specification's signals).
    pub const SIGNALS: u32 =
        IEEE_INVALID_OPERATION | FLOAT_OPERATION | DIVISION_BY_ZERO | OVERFLOW | UNDERFLOW | SUBNORMAL | INEXACT | ROUNDED | CLAMPED;
}

/// A status word the operations accumulate raised conditions into.
pub type Status = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rounding {
    Up,
    Down,
    HalfUp,
    HalfEven,
    HalfDown,
    Ceiling,
    Floor,
    Up05,
}

impl Rounding {
    pub const ALL: [Rounding; 8] = [
        Rounding::Up,
        Rounding::Down,
        Rounding::HalfUp,
        Rounding::HalfEven,
        Rounding::HalfDown,
        Rounding::Ceiling,
        Rounding::Floor,
        Rounding::Up05,
    ];

    /// `ROUND_HALF_EVEN`, `ROUND_05UP`, ...
    pub fn name(self) -> &'static str {
        match self {
            Rounding::Up => "ROUND_UP",
            Rounding::Down => "ROUND_DOWN",
            Rounding::HalfUp => "ROUND_HALF_UP",
            Rounding::HalfEven => "ROUND_HALF_EVEN",
            Rounding::HalfDown => "ROUND_HALF_DOWN",
            Rounding::Ceiling => "ROUND_CEILING",
            Rounding::Floor => "ROUND_FLOOR",
            Rounding::Up05 => "ROUND_05UP",
        }
    }

    pub fn from_name(name: &str) -> Option<Rounding> {
        Self::ALL.into_iter().find(|r| r.name() == name)
    }
}

/// An arithmetic context: the specification's precision, exponent range, rounding, clamp and
/// capitals, plus the host's trap and flag words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub prec: i64,
    pub emax: i64,
    pub emin: i64,
    pub round: Rounding,
    pub clamp: bool,
    pub capitals: bool,
    pub traps: u32,
    pub flags: u32,
}

impl Default for Context {
    /// CPython's `DefaultContext`.
    fn default() -> Context {
        Context {
            prec: 28,
            emax: 999_999,
            emin: -999_999,
            round: Rounding::HalfEven,
            clamp: false,
            capitals: true,
            traps: flag::IEEE_INVALID_OPERATION | flag::DIVISION_BY_ZERO | flag::OVERFLOW,
            flags: 0,
        }
    }
}

impl Context {
    /// The context `Decimal(...)` constructors use for exact conversions.
    pub fn max() -> Context {
        Context {
            prec: MAX_PREC,
            emax: MAX_EMAX,
            emin: MIN_EMIN,
            round: Rounding::HalfEven,
            clamp: false,
            capitals: true,
            traps: 0,
            flags: 0,
        }
    }

    /// The smallest exponent of a subnormal result.
    pub fn etiny(&self) -> i64 {
        self.emin - self.prec + 1
    }

    /// The largest exponent a coefficient of `prec` digits can carry.
    pub fn etop(&self) -> i64 {
        self.emax - self.prec + 1
    }

    /// ORs `status` into the flags; returns the conditions that trap (zero when none).
    pub fn raise(&mut self, status: Status) -> Status {
        self.flags |= status & flag::SIGNALS;
        status & (self.traps | flag::MALLOC_ERROR)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Finite,
    Infinity,
    NaN,
    SNaN,
}

/// A decimal number: `(-1)^sign * coef * 10^exp`, an infinity, or a NaN carrying a payload in
/// `coef`. Values are immutable; coefficients are not normalised (`1.0` keeps its trailing zero).
#[derive(Clone, Debug)]
pub struct Decimal {
    sign: bool,
    kind: Kind,
    coef: BigInt,
    exp: i64,
}

/// The classes `number_class` reports.
pub fn class_name(c: Class) -> &'static str {
    match c {
        Class::SNaN => "sNaN",
        Class::NaN => "NaN",
        Class::NegInfinity => "-Infinity",
        Class::NegNormal => "-Normal",
        Class::NegSubnormal => "-Subnormal",
        Class::NegZero => "-Zero",
        Class::PosZero => "+Zero",
        Class::PosSubnormal => "+Subnormal",
        Class::PosNormal => "+Normal",
        Class::PosInfinity => "+Infinity",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    SNaN,
    NaN,
    NegInfinity,
    NegNormal,
    NegSubnormal,
    NegZero,
    PosZero,
    PosSubnormal,
    PosNormal,
    PosInfinity,
}

/// Exponents are kept in an `i64`; values beyond this magnitude are saturated (they overflow or
/// underflow in every context anyway).
pub(crate) const EXP_LIMIT: i64 = 1 << 61;

pub(crate) fn sat(x: i128) -> i64 {
    x.clamp(-(EXP_LIMIT as i128), EXP_LIMIT as i128) as i64
}

impl Decimal {
    pub fn zero() -> Decimal {
        Decimal::finite(false, BigInt::zero(), 0)
    }

    pub fn one() -> Decimal {
        Decimal::finite(false, BigInt::from_u64(1), 0)
    }

    pub fn from_i64(v: i64) -> Decimal {
        Decimal::finite(v < 0, BigInt::from_u64(v.unsigned_abs()), 0)
    }

    pub fn from_bigint(v: &BigInt) -> Decimal {
        Decimal::finite(v.is_negative(), v.abs(), 0)
    }

    /// A finite value; `coef` must be non-negative.
    pub fn finite(sign: bool, coef: BigInt, exp: i64) -> Decimal {
        Decimal { sign, kind: Kind::Finite, coef, exp }
    }

    pub fn infinity(sign: bool) -> Decimal {
        Decimal { sign, kind: Kind::Infinity, coef: BigInt::zero(), exp: 0 }
    }

    /// A NaN with the given payload (`payload` must be non-negative).
    pub fn nan(sign: bool, signaling: bool, payload: BigInt) -> Decimal {
        Decimal { sign, kind: if signaling { Kind::SNaN } else { Kind::NaN }, coef: payload, exp: 0 }
    }

    pub fn quiet_nan() -> Decimal {
        Decimal::nan(false, false, BigInt::zero())
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn is_negative(&self) -> bool {
        self.sign
    }

    pub fn is_special(&self) -> bool {
        self.kind != Kind::Finite
    }

    pub fn is_finite(&self) -> bool {
        self.kind == Kind::Finite
    }

    pub fn is_infinite(&self) -> bool {
        self.kind == Kind::Infinity
    }

    pub fn is_nan(&self) -> bool {
        matches!(self.kind, Kind::NaN | Kind::SNaN)
    }

    pub fn is_qnan(&self) -> bool {
        self.kind == Kind::NaN
    }

    pub fn is_snan(&self) -> bool {
        self.kind == Kind::SNaN
    }

    /// Finite and zero.
    pub fn is_zero(&self) -> bool {
        self.kind == Kind::Finite && self.coef.is_zero()
    }

    /// The coefficient (the payload of a NaN, zero for an infinity).
    pub fn coefficient(&self) -> &BigInt {
        &self.coef
    }

    /// The exponent (zero for a special value).
    pub fn exponent(&self) -> i64 {
        self.exp
    }

    pub fn copy_abs(&self) -> Decimal {
        Decimal { sign: false, ..self.clone() }
    }

    pub fn copy_negate(&self) -> Decimal {
        Decimal { sign: !self.sign, ..self.clone() }
    }

    pub fn copy_sign(&self, other: &Decimal) -> Decimal {
        Decimal { sign: other.sign, ..self.clone() }
    }

    pub(crate) fn with_sign(&self, sign: bool) -> Decimal {
        Decimal { sign, ..self.clone() }
    }

    /// The number of digits of the coefficient (1 for zero and for specials with no payload).
    pub fn digits(&self) -> u64 {
        coef::ndigits(&self.coef)
    }

    /// The adjusted exponent: the exponent of the most significant digit (0 for a special).
    pub fn adjusted(&self) -> i128 {
        if self.kind == Kind::Finite {
            self.exp as i128 + self.digits() as i128 - 1
        } else {
            0
        }
    }

    /// Whether the value is integral (a finite value with no non-zero fractional digit).
    pub fn is_integer(&self) -> bool {
        if self.kind != Kind::Finite {
            return false;
        }
        if self.exp >= 0 || self.coef.is_zero() {
            return true;
        }
        coef::trailing_zeros10(&self.coef) >= self.exp.unsigned_abs()
    }

    /// Whether an integral value is even. Meaningful for integers only.
    pub fn is_even(&self) -> bool {
        if self.coef.is_zero() || self.exp > 0 {
            return true;
        }
        if self.exp == 0 {
            return !coef::is_odd(&self.coef);
        }
        match coef::pow10(self.exp.unsigned_abs()) {
            Ok(p) => match self.coef.divmod_floor(&p) {
                Some((q, _)) => !coef::is_odd(&q),
                None => true,
            },
            Err(_) => true,
        }
    }

    /// Numeric comparison of two non-NaN values (`None` when either is a NaN). Zeros of either
    /// sign are equal; infinities order by sign.
    pub fn compare_numeric(&self, other: &Decimal) -> Option<Ordering> {
        if self.is_nan() || other.is_nan() {
            return None;
        }
        Some(arith::cmp_values(self, other))
    }

    /// The sign-and-class ordering of `compare_total`: a total order on every representation.
    pub fn compare_total_order(&self, other: &Decimal) -> Ordering {
        misc::total_order(self, other)
    }

    /// `compare_total` with both signs ignored.
    pub fn compare_total_mag_order(&self, other: &Decimal) -> Ordering {
        misc::total_order(&self.copy_abs(), &other.copy_abs())
    }

    /// Whether the two operands have the same exponent (both infinite or both NaN count).
    pub fn same_quantum(&self, other: &Decimal) -> bool {
        if self.is_special() || other.is_special() {
            return (self.is_nan() && other.is_nan()) || (self.is_infinite() && other.is_infinite());
        }
        self.exp == other.exp
    }
}

impl PartialEq for Decimal {
    /// Representation equality (sign, kind, coefficient, exponent), not numeric equality.
    fn eq(&self, other: &Decimal) -> bool {
        self.sign == other.sign && self.kind == other.kind && self.coef == other.coef && self.exp == other.exp
    }
}
impl Eq for Decimal {}
