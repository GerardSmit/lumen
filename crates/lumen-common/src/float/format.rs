//! Correctly rounded decimal digits of a double at a requested precision, for the fixed and
//! exponential formatters of both languages: ECMAScript's `toFixed` / `toExponential` /
//! `toPrecision` round exact ties away from zero, CPython's `format()` rounds them to even. Ties
//! are judged on the exact binary value (`0.15` is really `0.1499…`).

use crate::rounding::{round_ascii, Mode};

/// Every f64's exact decimal expansion has at most 767 significant digits.
const EXACT_DIGITS: usize = 780;

/// `|v|` to `n` significant digits: `(digits, exp)` with `digits.len() == n` and
/// `|v| ≈ d.ddd × 10^exp`. `v` must be finite; zero gives `n` zeros and exponent 0. `n ≥ 1`.
pub fn significant(v: f64, n: usize, mode: Mode) -> (String, i32) {
    let n = n.max(1);
    let v = v.abs();
    if v == 0.0 {
        return ("0".repeat(n), 0);
    }
    if mode == Mode::HalfEven {
        // core's exact mode rounds ties to even.
        return split_sci(&format!("{:.*e}", n - 1, v));
    }
    let (mut digits, mut exp) = split_sci(&format!("{v:.EXACT_DIGITS$e}"));
    let mut bytes = std::mem::take(&mut digits).into_bytes();
    if round_ascii(&mut bytes, n, mode, false) {
        bytes.truncate(n);
        exp += 1;
    }
    (String::from_utf8(bytes).unwrap_or_default(), exp)
}

/// `|v|` to exactly `n` fraction digits as a plain decimal (`"0.050"`, `"12"`), no sign. `v` must
/// be finite and below 1e21 for the result to stay short.
pub fn fixed(v: f64, n: usize, mode: Mode) -> String {
    let v = v.abs();
    if mode == Mode::HalfEven {
        return format!("{v:.n$}");
    }
    // Fast path: when v·10^n is small enough that the product's rounding error (< 2^-13 below
    // 2^40) cannot move it across a rounding boundary, the nearest integer is decided from the
    // product directly.
    if mode == Mode::HalfUp && n <= 22 {
        let scaled = v * 10f64.powi(n as i32);
        if scaled < (1u64 << 40) as f64 {
            let floor = scaled.floor();
            let frac = scaled - floor;
            if (frac - 0.5).abs() > 1e-3 {
                let mut s = (floor as u64 + u64::from(frac > 0.5)).to_string();
                if n > 0 {
                    while s.len() <= n {
                        s.insert(0, '0');
                    }
                    s.insert(s.len() - n, '.');
                }
                return s;
            }
        }
    }
    // The exact expansion has at most 1074 fraction digits; format to that point (no rounding
    // happens in core), then round once ourselves.
    let exact = format!("{:.*}", exact_fraction_digits(v).max(n + 1), v);
    let (int, frac) = exact.split_once('.').unwrap_or((&exact, ""));
    let mut bytes: Vec<u8> = int.bytes().chain(frac.bytes()).collect();
    let keep = int.len() + n;
    let carried = round_ascii(&mut bytes, keep, mode, false);
    let int_len = int.len() + usize::from(carried);
    let mut out = String::with_capacity(bytes.len() + 1);
    out.push_str(std::str::from_utf8(&bytes[..int_len]).unwrap_or_default());
    if n > 0 {
        out.push('.');
        out.push_str(std::str::from_utf8(&bytes[int_len..]).unwrap_or_default());
    }
    out
}

/// The number of fraction digits in the exact decimal expansion of `v`.
fn exact_fraction_digits(v: f64) -> usize {
    let bits = v.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i64;
    let mantissa = bits & 0xf_ffff_ffff_ffff;
    let significand = if biased == 0 { mantissa } else { (1u64 << 52) | mantissa };
    if significand == 0 {
        return 0;
    }
    let e2 = if biased == 0 { -1074 } else { biased - 1075 } + significand.trailing_zeros() as i64;
    if e2 < 0 {
        (-e2) as usize
    } else {
        0
    }
}

/// `d.ddde±x` into (`dddd`, x).
fn split_sci(s: &str) -> (String, i32) {
    let (m, e) = s.split_once('e').unwrap_or((s, "0"));
    (m.chars().filter(|c| *c != '.').collect(), e.parse().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AWAY: Mode = Mode::HalfUp;
    const EVEN: Mode = Mode::HalfEven;

    #[test]
    fn fixed_ties() {
        assert_eq!(fixed(0.5, 0, AWAY), "1");
        assert_eq!(fixed(0.5, 0, EVEN), "0");
        assert_eq!(fixed(2.5, 0, AWAY), "3");
        assert_eq!(fixed(0.15, 1, AWAY), "0.1");
        assert_eq!(fixed(1.005, 2, AWAY), "1.00");
        assert_eq!(fixed(1e-10, 3, AWAY), "0.000");
        assert_eq!(fixed(999.9999, 2, AWAY), "1000.00");
        assert_eq!(fixed(123.456, 10, AWAY), "123.4560000000");
        assert_eq!(fixed(1e20, 2, AWAY), "100000000000000000000.00");
        assert_eq!(fixed(0.000001, 7, AWAY), "0.0000010");
    }

    #[test]
    fn significant_digits() {
        assert_eq!(significant(2.5, 1, AWAY), ("3".into(), 0));
        assert_eq!(significant(2.5, 1, EVEN), ("2".into(), 0));
        assert_eq!(significant(9.99, 2, AWAY), ("10".into(), 1));
        assert_eq!(significant(123.456, 2, AWAY), ("12".into(), 2));
        assert_eq!(significant(1.0, 4, AWAY), ("1000".into(), 0));
        assert_eq!(significant(0.0, 3, AWAY), ("000".into(), 0));
        assert_eq!(significant(1.25, 2, AWAY), ("13".into(), 0));
        assert_eq!(significant(1.25, 2, EVEN), ("12".into(), 0));
        assert_eq!(significant(5e-324, 1, AWAY), ("5".into(), -324));
    }
}
