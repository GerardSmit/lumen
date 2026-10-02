//! Complex arithmetic and the elementary complex functions with C99 Annex G special values, as
//! CPython's `complexobject.c` and `cmathmodule.c` compute them.
//!
//! Each function returns its result together with the C `errno` CPython derives from it:
//! `Err(MathError::Domain)` where Annex G signals divide-by-zero or invalid, `Err(MathError::Range)`
//! on overflow. The special-value tables are indexed by [`special_type`] of the real and imaginary
//! parts.

use super::MathError;
use core::f64::consts::{E, LN_2, PI};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Complex {
    pub re: f64,
    pub im: f64,
}

pub type CResult = Result<Complex, (MathError, Complex)>;

const fn c(re: f64, im: f64) -> Complex {
    Complex { re, im }
}

impl Complex {
    pub const fn new(re: f64, im: f64) -> Complex {
        c(re, im)
    }

    fn neg(self) -> Complex {
        c(-self.re, -self.im)
    }

    fn is_finite(self) -> bool {
        self.re.is_finite() && self.im.is_finite()
    }
}

const LARGE_DOUBLE: f64 = f64::MAX / 4.0;
// sqrt(LARGE_DOUBLE), log(LARGE_DOUBLE) and sqrt(DBL_MIN).
const SQRT_LARGE_DOUBLE: f64 = 6.703903964971298e+153;
const LOG_LARGE_DOUBLE: f64 = 708.3964185322641;
const SQRT_DBL_MIN: f64 = 1.4916681462400413e-154;
const MANT_DIG: i32 = 53;
const SCALE_UP: i32 = 2 * (MANT_DIG / 2) + 1;
const SCALE_DOWN: i32 = -(SCALE_UP + 1) / 2;

/// The class of a double for the special-value tables: -inf, negative, -0, +0, positive, +inf,
/// NaN.
fn special_type(d: f64) -> usize {
    if d.is_finite() {
        match (d != 0.0, d.is_sign_positive()) {
            (true, true) => 4,
            (true, false) => 1,
            (false, true) => 3,
            (false, false) => 2,
        }
    } else if d.is_nan() {
        6
    } else if d > 0.0 {
        5
    } else {
        0
    }
}

type Table = [[Complex; 7]; 7];

fn special(z: Complex, table: &Table) -> Complex {
    table[special_type(z.re)][special_type(z.im)]
}

const P: f64 = PI;
const P14: f64 = 0.25 * PI;
const P12: f64 = 0.5 * PI;
const P34: f64 = 0.75 * PI;
const INF: f64 = f64::INFINITY;
const N: f64 = f64::NAN;
/// Placeholder for table entries that finite arguments never reach.
const U: f64 = -9.542_631_940_771_103e33;

macro_rules! table {
    ($([$(($re:expr, $im:expr))*])*) => { [$([$(c($re, $im)),*]),*] };
}

#[rustfmt::skip]
static ACOS: Table = table![
    [(P34,INF) (P,INF)  (P,INF)  (P,-INF)  (P,-INF)  (P34,-INF) (N,INF)]
    [(P12,INF) (U,U)    (U,U)    (U,U)     (U,U)     (P12,-INF) (N,N)]
    [(P12,INF) (U,U)    (P12,0.) (P12,-0.) (U,U)     (P12,-INF) (P12,N)]
    [(P12,INF) (U,U)    (P12,0.) (P12,-0.) (U,U)     (P12,-INF) (P12,N)]
    [(P12,INF) (U,U)    (U,U)    (U,U)     (U,U)     (P12,-INF) (N,N)]
    [(P14,INF) (0.,INF) (0.,INF) (0.,-INF) (0.,-INF) (P14,-INF) (N,INF)]
    [(N,INF)   (N,N)    (N,N)    (N,N)     (N,N)     (N,-INF)   (N,N)]
];

#[rustfmt::skip]
static ACOSH: Table = table![
    [(INF,-P34) (INF,-P)  (INF,-P)  (INF,P)  (INF,P)  (INF,P34) (INF,N)]
    [(INF,-P12) (U,U)     (U,U)     (U,U)    (U,U)    (INF,P12) (N,N)]
    [(INF,-P12) (U,U)     (0.,-P12) (0.,P12) (U,U)    (INF,P12) (N,N)]
    [(INF,-P12) (U,U)     (0.,-P12) (0.,P12) (U,U)    (INF,P12) (N,N)]
    [(INF,-P12) (U,U)     (U,U)     (U,U)    (U,U)    (INF,P12) (N,N)]
    [(INF,-P14) (INF,-0.) (INF,-0.) (INF,0.) (INF,0.) (INF,P14) (INF,N)]
    [(INF,N)    (N,N)     (N,N)     (N,N)    (N,N)    (INF,N)   (N,N)]
];

#[rustfmt::skip]
static ASINH: Table = table![
    [(-INF,-P14) (-INF,-0.) (-INF,-0.) (-INF,0.) (-INF,0.) (-INF,P14) (-INF,N)]
    [(-INF,-P12) (U,U)      (U,U)      (U,U)     (U,U)     (-INF,P12) (N,N)]
    [(-INF,-P12) (U,U)      (-0.,-0.)  (-0.,0.)  (U,U)     (-INF,P12) (N,N)]
    [(INF,-P12)  (U,U)      (0.,-0.)   (0.,0.)   (U,U)     (INF,P12)  (N,N)]
    [(INF,-P12)  (U,U)      (U,U)      (U,U)     (U,U)     (INF,P12)  (N,N)]
    [(INF,-P14)  (INF,-0.)  (INF,-0.)  (INF,0.)  (INF,0.)  (INF,P14)  (INF,N)]
    [(INF,N)     (N,N)      (N,-0.)    (N,0.)    (N,N)     (INF,N)    (N,N)]
];

#[rustfmt::skip]
static ATANH: Table = table![
    [(-0.,-P12) (-0.,-P12) (-0.,-P12) (-0.,P12) (-0.,P12) (-0.,P12) (-0.,N)]
    [(-0.,-P12) (U,U)      (U,U)      (U,U)     (U,U)     (-0.,P12) (N,N)]
    [(-0.,-P12) (U,U)      (-0.,-0.)  (-0.,0.)  (U,U)     (-0.,P12) (-0.,N)]
    [(0.,-P12)  (U,U)      (0.,-0.)   (0.,0.)   (U,U)     (0.,P12)  (0.,N)]
    [(0.,-P12)  (U,U)      (U,U)      (U,U)     (U,U)     (0.,P12)  (N,N)]
    [(0.,-P12)  (0.,-P12)  (0.,-P12)  (0.,P12)  (0.,P12)  (0.,P12)  (0.,N)]
    [(0.,-P12)  (N,N)      (N,N)      (N,N)     (N,N)     (0.,P12)  (N,N)]
];

#[rustfmt::skip]
static COSH: Table = table![
    [(INF,N) (U,U) (INF,0.)  (INF,-0.) (U,U) (INF,N) (INF,N)]
    [(N,N)   (U,U) (U,U)     (U,U)     (U,U) (N,N)   (N,N)]
    [(N,0.)  (U,U) (1.,0.)   (1.,-0.)  (U,U) (N,0.)  (N,0.)]
    [(N,0.)  (U,U) (1.,-0.)  (1.,0.)   (U,U) (N,0.)  (N,0.)]
    [(N,N)   (U,U) (U,U)     (U,U)     (U,U) (N,N)   (N,N)]
    [(INF,N) (U,U) (INF,-0.) (INF,0.)  (U,U) (INF,N) (INF,N)]
    [(N,N)   (N,N) (N,0.)    (N,0.)    (N,N) (N,N)   (N,N)]
];

#[rustfmt::skip]
static EXP: Table = table![
    [(0.,0.) (U,U) (0.,-0.)  (0.,0.)  (U,U) (0.,0.) (0.,0.)]
    [(N,N)   (U,U) (U,U)     (U,U)    (U,U) (N,N)   (N,N)]
    [(N,N)   (U,U) (1.,-0.)  (1.,0.)  (U,U) (N,N)   (N,N)]
    [(N,N)   (U,U) (1.,-0.)  (1.,0.)  (U,U) (N,N)   (N,N)]
    [(N,N)   (U,U) (U,U)     (U,U)    (U,U) (N,N)   (N,N)]
    [(INF,N) (U,U) (INF,-0.) (INF,0.) (U,U) (INF,N) (INF,N)]
    [(N,N)   (N,N) (N,-0.)   (N,0.)   (N,N) (N,N)   (N,N)]
];

#[rustfmt::skip]
static LOG: Table = table![
    [(INF,-P34) (INF,-P)  (INF,-P)   (INF,P)   (INF,P)  (INF,P34)  (INF,N)]
    [(INF,-P12) (U,U)     (U,U)      (U,U)     (U,U)    (INF,P12)  (N,N)]
    [(INF,-P12) (U,U)     (-INF,-P)  (-INF,P)  (U,U)    (INF,P12)  (N,N)]
    [(INF,-P12) (U,U)     (-INF,-0.) (-INF,0.) (U,U)    (INF,P12)  (N,N)]
    [(INF,-P12) (U,U)     (U,U)      (U,U)     (U,U)    (INF,P12)  (N,N)]
    [(INF,-P14) (INF,-0.) (INF,-0.)  (INF,0.)  (INF,0.) (INF,P14)  (INF,N)]
    [(INF,N)    (N,N)     (N,N)      (N,N)     (N,N)    (INF,N)    (N,N)]
];

#[rustfmt::skip]
static SINH: Table = table![
    [(INF,N) (U,U) (-INF,-0.) (-INF,0.) (U,U) (INF,N) (INF,N)]
    [(N,N)   (U,U) (U,U)      (U,U)     (U,U) (N,N)   (N,N)]
    [(0.,N)  (U,U) (-0.,-0.)  (-0.,0.)  (U,U) (0.,N)  (0.,N)]
    [(0.,N)  (U,U) (0.,-0.)   (0.,0.)   (U,U) (0.,N)  (0.,N)]
    [(N,N)   (U,U) (U,U)      (U,U)     (U,U) (N,N)   (N,N)]
    [(INF,N) (U,U) (INF,-0.)  (INF,0.)  (U,U) (INF,N) (INF,N)]
    [(N,N)   (N,N) (N,-0.)    (N,0.)    (N,N) (N,N)   (N,N)]
];

#[rustfmt::skip]
static SQRT: Table = table![
    [(INF,-INF) (0.,-INF) (0.,-INF) (0.,INF) (0.,INF) (INF,INF) (N,INF)]
    [(INF,-INF) (U,U)     (U,U)     (U,U)    (U,U)    (INF,INF) (N,N)]
    [(INF,-INF) (U,U)     (0.,-0.)  (0.,0.)  (U,U)    (INF,INF) (N,N)]
    [(INF,-INF) (U,U)     (0.,-0.)  (0.,0.)  (U,U)    (INF,INF) (N,N)]
    [(INF,-INF) (U,U)     (U,U)     (U,U)    (U,U)    (INF,INF) (N,N)]
    [(INF,-INF) (INF,-0.) (INF,-0.) (INF,0.) (INF,0.) (INF,INF) (INF,N)]
    [(INF,-INF) (N,N)     (N,N)     (N,N)    (N,N)    (INF,INF) (N,N)]
];

#[rustfmt::skip]
static TANH: Table = table![
    [(-1.,0.) (U,U) (-1.,-0.) (-1.,0.) (U,U) (-1.,0.) (-1.,0.)]
    [(N,N)    (U,U) (U,U)     (U,U)    (U,U) (N,N)    (N,N)]
    [(N,N)    (U,U) (-0.,-0.) (-0.,0.) (U,U) (N,N)    (N,N)]
    [(N,N)    (U,U) (0.,-0.)  (0.,0.)  (U,U) (N,N)    (N,N)]
    [(N,N)    (U,U) (U,U)     (U,U)    (U,U) (N,N)    (N,N)]
    [(1.,0.)  (U,U) (1.,-0.)  (1.,0.)  (U,U) (1.,0.)  (1.,0.)]
    [(N,N)    (N,N) (N,-0.)   (N,0.)   (N,N) (N,N)    (N,N)]
];

#[rustfmt::skip]
static RECT: Table = table![
    [(INF,N) (U,U) (-INF,0.) (-INF,-0.) (U,U) (INF,N) (INF,N)]
    [(N,N)   (U,U) (U,U)     (U,U)      (U,U) (N,N)   (N,N)]
    [(0.,0.) (U,U) (-0.,0.)  (-0.,-0.)  (U,U) (0.,0.) (0.,0.)]
    [(0.,0.) (U,U) (0.,-0.)  (0.,0.)    (U,U) (0.,0.) (0.,0.)]
    [(N,N)   (U,U) (U,U)     (U,U)      (U,U) (N,N)   (N,N)]
    [(INF,N) (U,U) (INF,-0.) (INF,0.)   (U,U) (INF,N) (INF,N)]
    [(N,N)   (N,N) (N,0.)    (N,0.)     (N,N) (N,N)   (N,N)]
];

fn ok(z: Complex) -> CResult {
    Ok(z)
}

fn with_err(z: Complex, e: Option<MathError>) -> CResult {
    match e {
        Some(e) => Err((e, z)),
        None => Ok(z),
    }
}

/// Overflow check shared by exp, cosh and sinh.
fn range_checked(z: Complex) -> CResult {
    if z.re.is_infinite() || z.im.is_infinite() {
        Err((MathError::Range, z))
    } else {
        Ok(z)
    }
}

/// `log1p` keeping the sign of a zero argument (CPython's `_Py_log1p`).
fn log1p(x: f64) -> f64 {
    if x == 0.0 {
        x
    } else {
        x.ln_1p()
    }
}

fn ldexp(x: f64, e: i32) -> f64 {
    super::ldexp(x, e as i64)
}

pub fn sqrt(z: Complex) -> CResult {
    if !z.is_finite() {
        return ok(special(z, &SQRT));
    }
    if z.re == 0.0 && z.im == 0.0 {
        return ok(c(0.0, z.im));
    }
    let (mut ax, ay) = (z.re.abs(), z.im.abs());
    let s = if ax < f64::MIN_POSITIVE && ay < f64::MIN_POSITIVE {
        ax = ldexp(ax, SCALE_UP);
        ldexp((ax + ax.hypot(ldexp(ay, SCALE_UP))).sqrt(), SCALE_DOWN)
    } else {
        ax /= 8.0;
        2.0 * (ax + ax.hypot(ay / 8.0)).sqrt()
    };
    let d = ay / (2.0 * s);
    ok(if z.re >= 0.0 {
        c(s, d.copysign(z.im))
    } else {
        c(d, s.copysign(z.im))
    })
}

pub fn acos(z: Complex) -> CResult {
    if !z.is_finite() {
        return ok(special(z, &ACOS));
    }
    let r = if z.re.abs() > LARGE_DOUBLE || z.im.abs() > LARGE_DOUBLE {
        let re = z.im.abs().atan2(z.re);
        let l = (z.re / 2.0).hypot(z.im / 2.0).ln() + LN_2 * 2.0;
        let im = if z.re < 0.0 {
            -l.copysign(z.im)
        } else {
            l.copysign(-z.im)
        };
        c(re, im)
    } else {
        let s1 = unwrap(sqrt(c(1.0 - z.re, -z.im)));
        let s2 = unwrap(sqrt(c(1.0 + z.re, z.im)));
        c(
            2.0 * s1.re.atan2(s2.re),
            super::asinh(s2.re * s1.im - s2.im * s1.re),
        )
    };
    ok(r)
}

fn unwrap(r: CResult) -> Complex {
    match r {
        Ok(z) | Err((_, z)) => z,
    }
}

pub fn acosh(z: Complex) -> CResult {
    if !z.is_finite() {
        return ok(special(z, &ACOSH));
    }
    let r = if z.re.abs() > LARGE_DOUBLE || z.im.abs() > LARGE_DOUBLE {
        c(
            (z.re / 2.0).hypot(z.im / 2.0).ln() + LN_2 * 2.0,
            z.im.atan2(z.re),
        )
    } else {
        let s1 = unwrap(sqrt(c(z.re - 1.0, z.im)));
        let s2 = unwrap(sqrt(c(z.re + 1.0, z.im)));
        c(
            super::asinh(s1.re * s2.re + s1.im * s2.im),
            2.0 * s1.im.atan2(s2.re),
        )
    };
    ok(r)
}

pub fn asin(z: Complex) -> CResult {
    let s = asinh(c(-z.im, z.re));
    map(s, |s| c(s.im, -s.re))
}

fn map(r: CResult, f: impl Fn(Complex) -> Complex) -> CResult {
    match r {
        Ok(z) => Ok(f(z)),
        Err((e, z)) => Err((e, f(z))),
    }
}

pub fn asinh(z: Complex) -> CResult {
    if !z.is_finite() {
        return ok(special(z, &ASINH));
    }
    let r = if z.re.abs() > LARGE_DOUBLE || z.im.abs() > LARGE_DOUBLE {
        let l = (z.re / 2.0).hypot(z.im / 2.0).ln() + LN_2 * 2.0;
        let re = if z.im >= 0.0 {
            l.copysign(z.re)
        } else {
            -l.copysign(-z.re)
        };
        c(re, z.im.atan2(z.re.abs()))
    } else {
        let s1 = unwrap(sqrt(c(1.0 + z.im, -z.re)));
        let s2 = unwrap(sqrt(c(1.0 - z.im, z.re)));
        c(
            super::asinh(s1.re * s2.im - s2.re * s1.im),
            z.im.atan2(s1.re * s2.re - s1.im * s2.im),
        )
    };
    ok(r)
}

pub fn atan(z: Complex) -> CResult {
    let s = atanh(c(-z.im, z.re));
    map(s, |s| c(s.im, -s.re))
}

pub fn atanh(z: Complex) -> CResult {
    if !z.is_finite() {
        return ok(special(z, &ATANH));
    }
    if z.re < 0.0 {
        return map(atanh(z.neg()), Complex::neg);
    }
    let ay = z.im.abs();
    if z.re > SQRT_LARGE_DOUBLE || ay > SQRT_LARGE_DOUBLE {
        let h = (z.re / 2.0).hypot(z.im / 2.0);
        return ok(c(z.re / 4.0 / h / h, -(PI / 2.0).copysign(-z.im)));
    }
    if z.re == 1.0 && ay < SQRT_DBL_MIN {
        if ay == 0.0 {
            return Err((MathError::Domain, c(INF, z.im)));
        }
        return ok(c(
            -(ay.sqrt() / ay.hypot(2.0).sqrt()).ln(),
            (2.0f64.atan2(-ay) / 2.0).copysign(z.im),
        ));
    }
    ok(c(
        log1p(4.0 * z.re / ((1.0 - z.re) * (1.0 - z.re) + ay * ay)) / 4.0,
        -(-2.0 * z.im).atan2((1.0 - z.re) * (1.0 + z.re) - ay * ay) / 2.0,
    ))
}

/// The result for an infinite real part and finite nonzero imaginary part, shared by cosh, sinh
/// and exp: `(sign_re * inf * cos(y), sign_im * inf * sin(y))`.
fn inf_cis(y: f64, sign_re: f64, sign_im: f64) -> Complex {
    c(
        sign_re * INF.copysign(y.cos()),
        sign_im * INF.copysign(y.sin()),
    )
}

pub fn cosh(z: Complex) -> CResult {
    if !z.is_finite() {
        let r = if z.re.is_infinite() && z.im.is_finite() && z.im != 0.0 {
            if z.re > 0.0 {
                inf_cis(z.im, 1.0, 1.0)
            } else {
                inf_cis(z.im, 1.0, -1.0)
            }
        } else {
            special(z, &COSH)
        };
        let e = (z.im.is_infinite() && !z.re.is_nan()).then_some(MathError::Domain);
        return with_err(r, e);
    }
    let r = if z.re.abs() > LOG_LARGE_DOUBLE {
        let x1 = z.re - 1f64.copysign(z.re);
        c(z.im.cos() * x1.cosh() * E, z.im.sin() * x1.sinh() * E)
    } else {
        c(z.im.cos() * z.re.cosh(), z.im.sin() * z.re.sinh())
    };
    range_checked(r)
}

pub fn cos(z: Complex) -> CResult {
    cosh(c(-z.im, z.re))
}

pub fn sinh(z: Complex) -> CResult {
    if !z.is_finite() {
        let r = if z.re.is_infinite() && z.im.is_finite() && z.im != 0.0 {
            if z.re > 0.0 {
                inf_cis(z.im, 1.0, 1.0)
            } else {
                inf_cis(z.im, -1.0, 1.0)
            }
        } else {
            special(z, &SINH)
        };
        let e = (z.im.is_infinite() && !z.re.is_nan()).then_some(MathError::Domain);
        return with_err(r, e);
    }
    let r = if z.re.abs() > LOG_LARGE_DOUBLE {
        let x1 = z.re - 1f64.copysign(z.re);
        c(z.im.cos() * x1.sinh() * E, z.im.sin() * x1.cosh() * E)
    } else {
        c(z.im.cos() * z.re.sinh(), z.im.sin() * z.re.cosh())
    };
    range_checked(r)
}

pub fn sin(z: Complex) -> CResult {
    map(sinh(c(-z.im, z.re)), |s| c(s.im, -s.re))
}

pub fn tanh(z: Complex) -> CResult {
    if !z.is_finite() {
        let r = if z.re.is_infinite() && z.im.is_finite() && z.im != 0.0 {
            let im = 0f64.copysign(2.0 * z.im.sin() * z.im.cos());
            c(if z.re > 0.0 { 1.0 } else { -1.0 }, im)
        } else {
            special(z, &TANH)
        };
        let e = (z.im.is_infinite() && z.re.is_finite()).then_some(MathError::Domain);
        return with_err(r, e);
    }
    if z.re.abs() > LOG_LARGE_DOUBLE {
        return ok(c(
            1f64.copysign(z.re),
            4.0 * z.im.sin() * z.im.cos() * (-2.0 * z.re.abs()).exp(),
        ));
    }
    let tx = z.re.tanh();
    let ty = z.im.tan();
    let cx = 1.0 / z.re.cosh();
    let txty = tx * ty;
    let denom = 1.0 + txty * txty;
    ok(c(tx * (1.0 + ty * ty) / denom, ((ty / denom) * cx) * cx))
}

pub fn tan(z: Complex) -> CResult {
    map(tanh(c(-z.im, z.re)), |s| c(s.im, -s.re))
}

pub fn exp(z: Complex) -> CResult {
    if !z.is_finite() {
        let r = if z.re.is_infinite() && z.im.is_finite() && z.im != 0.0 {
            if z.re > 0.0 {
                inf_cis(z.im, 1.0, 1.0)
            } else {
                c(0f64.copysign(z.im.cos()), 0f64.copysign(z.im.sin()))
            }
        } else {
            special(z, &EXP)
        };
        let e =
            (z.im.is_infinite() && (z.re.is_finite() || z.re == INF)).then_some(MathError::Domain);
        return with_err(r, e);
    }
    let r = if z.re > LOG_LARGE_DOUBLE {
        let l = (z.re - 1.0).exp();
        c(l * z.im.cos() * E, l * z.im.sin() * E)
    } else {
        let l = z.re.exp();
        c(l * z.im.cos(), l * z.im.sin())
    };
    range_checked(r)
}

pub fn log(z: Complex) -> CResult {
    if !z.is_finite() {
        return ok(special(z, &LOG));
    }
    let (ax, ay) = (z.re.abs(), z.im.abs());
    let re = if ax > LARGE_DOUBLE || ay > LARGE_DOUBLE {
        (ax / 2.0).hypot(ay / 2.0).ln() + LN_2
    } else if ax < f64::MIN_POSITIVE && ay < f64::MIN_POSITIVE {
        if ax > 0.0 || ay > 0.0 {
            ldexp(ax, MANT_DIG).hypot(ldexp(ay, MANT_DIG)).ln() - MANT_DIG as f64 * LN_2
        } else {
            return Err((MathError::Domain, c(-INF, z.im.atan2(z.re))));
        }
    } else {
        let h = ax.hypot(ay);
        if (0.71..=1.73).contains(&h) {
            let (am, an) = if ax > ay { (ax, ay) } else { (ay, ax) };
            log1p((am - 1.0) * (am + 1.0) + an * an) / 2.0
        } else {
            h.ln()
        }
    };
    ok(c(re, z.im.atan2(z.re)))
}

pub fn log10(z: Complex) -> CResult {
    map(log(z), |r| {
        c(
            r.re / core::f64::consts::LN_10,
            r.im / core::f64::consts::LN_10,
        )
    })
}

/// `phase(z)`: atan2 with C99's special cases on every platform.
pub fn phase(z: Complex) -> f64 {
    if z.re.is_nan() || z.im.is_nan() {
        return f64::NAN;
    }
    if z.im.is_infinite() {
        if z.re.is_infinite() {
            return if z.re > 0.0 {
                (0.25 * PI).copysign(z.im)
            } else {
                (0.75 * PI).copysign(z.im)
            };
        }
        return (0.5 * PI).copysign(z.im);
    }
    if z.re.is_infinite() || z.im == 0.0 {
        return if z.re.is_sign_positive() {
            0f64.copysign(z.im)
        } else {
            PI.copysign(z.im)
        };
    }
    z.im.atan2(z.re)
}

/// `abs(z)` (`_Py_c_abs`): an infinite part wins over a NaN; overflow is a range error.
pub fn abs(z: Complex) -> Result<f64, MathError> {
    if !z.is_finite() {
        if z.re.is_infinite() {
            return Ok(z.re.abs());
        }
        if z.im.is_infinite() {
            return Ok(z.im.abs());
        }
        return Ok(f64::NAN);
    }
    let r = z.re.hypot(z.im);
    if r.is_finite() {
        Ok(r)
    } else {
        Err(MathError::Range)
    }
}

pub fn rect(r: f64, phi: f64) -> CResult {
    if !r.is_finite() || !phi.is_finite() {
        let z = if r.is_infinite() && phi.is_finite() && phi != 0.0 {
            if r > 0.0 {
                c(INF.copysign(phi.cos()), INF.copysign(phi.sin()))
            } else {
                c(-INF.copysign(phi.cos()), -INF.copysign(phi.sin()))
            }
        } else {
            special(c(r, phi), &RECT)
        };
        let e = (r != 0.0 && !r.is_nan() && phi.is_infinite()).then_some(MathError::Domain);
        return with_err(z, e);
    }
    if phi == 0.0 {
        return ok(c(r, r * phi));
    }
    ok(c(r * phi.cos(), r * phi.sin()))
}

/// `a / b` (`_Py_c_quot`): scaled by the larger part of `b`; `None` for a zero divisor.
pub fn quot(a: Complex, b: Complex) -> Option<Complex> {
    let (abs_br, abs_bi) = (b.re.abs(), b.im.abs());
    if abs_br >= abs_bi {
        if abs_br == 0.0 {
            return None;
        }
        let ratio = b.im / b.re;
        let denom = b.re + b.im * ratio;
        Some(c(
            (a.re + a.im * ratio) / denom,
            (a.im - a.re * ratio) / denom,
        ))
    } else if abs_bi >= abs_br {
        let ratio = b.re / b.im;
        let denom = b.re * ratio + b.im;
        Some(c(
            (a.re * ratio + a.im) / denom,
            (a.im * ratio - a.re) / denom,
        ))
    } else {
        Some(c(f64::NAN, f64::NAN))
    }
}

fn prod(a: Complex, b: Complex) -> Complex {
    c(a.re * b.re - a.im * b.im, a.re * b.im + a.im * b.re)
}

/// `a ** b` (`complex_pow`): repeated squaring for small integral exponents, polar form
/// otherwise. `Domain` is zero to a negative or complex power, `Range` an infinite result.
pub fn pow(a: Complex, b: Complex) -> Result<Complex, MathError> {
    let r = if b.im == 0.0 && b.re == b.re.floor() && b.re.abs() <= 100.0 {
        let n = b.re as i64;
        let powu = |x: Complex, n: i64| {
            let (mut r, mut p, mut mask) = (c(1.0, 0.0), x, 1i64);
            while mask > 0 && n >= mask {
                if n & mask != 0 {
                    r = prod(r, p);
                }
                mask <<= 1;
                p = prod(p, p);
            }
            r
        };
        if n > 0 {
            powu(a, n)
        } else {
            quot(c(1.0, 0.0), powu(a, -n)).ok_or(MathError::Domain)?
        }
    } else if b.re == 0.0 && b.im == 0.0 {
        c(1.0, 0.0)
    } else if a.re == 0.0 && a.im == 0.0 {
        if b.im != 0.0 || b.re < 0.0 {
            return Err(MathError::Domain);
        }
        c(0.0, 0.0)
    } else {
        let vabs = a.re.hypot(a.im);
        let mut len = vabs.powf(b.re);
        let at = a.im.atan2(a.re);
        let mut phase = at * b.re;
        if b.im != 0.0 {
            len /= (at * b.im).exp();
            phase += b.im * vabs.ln();
        }
        c(len * phase.cos(), len * phase.sin())
    };
    if r.re.is_infinite() || r.im.is_infinite() {
        return Err(MathError::Range);
    }
    Ok(r)
}

/// `cmath.isclose`: exact equality, then the weak relative test or the absolute tolerance.
pub fn isclose(a: Complex, b: Complex, rel_tol: f64, abs_tol: f64) -> bool {
    if a.re == b.re && a.im == b.im {
        return true;
    }
    if a.re.is_infinite() || a.im.is_infinite() || b.re.is_infinite() || b.im.is_infinite() {
        return false;
    }
    let diff = abs(c(a.re - b.re, a.im - b.im)).unwrap_or(f64::INFINITY);
    let (aa, ab) = (
        abs(a).unwrap_or(f64::INFINITY),
        abs(b).unwrap_or(f64::INFINITY),
    );
    diff <= rel_tol * ab || diff <= rel_tol * aa || diff <= abs_tol
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert_eq!(sqrt(c(-4.0, 0.0)), Ok(c(0.0, 2.0)));
        assert_eq!(sqrt(c(-4.0, -0.0)), Ok(c(0.0, -2.0)));
        assert!(matches!(log(c(0.0, 0.0)), Err((MathError::Domain, _))));
        assert!(matches!(exp(c(1000.0, 1.0)), Err((MathError::Range, _))));
        assert_eq!(quot(c(1.0, 0.0), c(0.0, 0.0)), None);
        assert_eq!(pow(c(0.0, 0.0), c(-1.0, 0.0)), Err(MathError::Domain));
        assert_eq!(phase(c(-1.0, -0.0)), -PI);
    }
}
