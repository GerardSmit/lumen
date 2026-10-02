//! Every `f64` function that lives in `std` rather than `core`, behind one module so a `no_std`
//! build can substitute a pure-Rust libm here.

#[inline]
pub fn sin(x: f64) -> f64 {
    x.sin()
}

#[inline]
pub fn cos(x: f64) -> f64 {
    x.cos()
}

#[inline]
pub fn tan(x: f64) -> f64 {
    x.tan()
}

#[inline]
pub fn asin(x: f64) -> f64 {
    x.asin()
}

#[inline]
pub fn acos(x: f64) -> f64 {
    x.acos()
}

#[inline]
pub fn atan(x: f64) -> f64 {
    x.atan()
}

#[inline]
pub fn sinh(x: f64) -> f64 {
    x.sinh()
}

#[inline]
pub fn cosh(x: f64) -> f64 {
    x.cosh()
}

#[inline]
pub fn tanh(x: f64) -> f64 {
    x.tanh()
}

#[inline]
pub fn asinh(x: f64) -> f64 {
    x.asinh()
}

#[inline]
pub fn acosh(x: f64) -> f64 {
    x.acosh()
}

#[inline]
pub fn atanh(x: f64) -> f64 {
    x.atanh()
}

#[inline]
pub fn exp(x: f64) -> f64 {
    x.exp()
}

#[inline]
pub fn exp2(x: f64) -> f64 {
    x.exp2()
}

#[inline]
pub fn exp_m1(x: f64) -> f64 {
    x.exp_m1()
}

#[inline]
pub fn ln(x: f64) -> f64 {
    x.ln()
}

#[inline]
pub fn ln_1p(x: f64) -> f64 {
    x.ln_1p()
}

#[inline]
pub fn log2(x: f64) -> f64 {
    x.log2()
}

#[inline]
pub fn log10(x: f64) -> f64 {
    x.log10()
}

#[inline]
pub fn sqrt(x: f64) -> f64 {
    x.sqrt()
}

#[inline]
pub fn cbrt(x: f64) -> f64 {
    x.cbrt()
}

#[inline]
pub fn floor(x: f64) -> f64 {
    x.floor()
}

#[inline]
pub fn ceil(x: f64) -> f64 {
    x.ceil()
}

#[inline]
pub fn trunc(x: f64) -> f64 {
    x.trunc()
}

#[inline]
pub fn round(x: f64) -> f64 {
    x.round()
}

#[inline]
pub fn atan2(x: f64, y: f64) -> f64 {
    x.atan2(y)
}

#[inline]
pub fn hypot(x: f64, y: f64) -> f64 {
    x.hypot(y)
}

#[inline]
pub fn powf(x: f64, y: f64) -> f64 {
    x.powf(y)
}

#[inline]
pub fn powi(x: f64, n: i32) -> f64 {
    x.powi(n)
}
