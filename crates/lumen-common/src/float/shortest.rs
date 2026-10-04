//! Shortest round-tripping decimal digits of a double, as both ECMAScript `Number::toString` and
//! CPython's `repr(float)` define them: the fewest digits that read back as the same double, and
//! among those the digit string closest to the exact value, ties to even.

use core::fmt::Write;

/// The significant digits of a finite, nonzero double: value = 0.`digits` × 10^`decpt`.
pub struct Digits {
    buf: [u8; 32],
    len: u8,
    /// Position of the decimal point relative to the first digit (`1.5` has `decpt == 1`).
    pub decpt: i32,
}

impl Digits {
    /// ASCII digits, no leading or trailing zeros.
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("0")
    }

    pub fn len(&self) -> usize {
        self.len as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

struct Buf {
    buf: [u8; 32],
    len: usize,
}

impl Write for Buf {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let end = self.len + s.len();
        if end > self.buf.len() {
            return Err(core::fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

fn sci(args: core::fmt::Arguments<'_>) -> Digits {
    let mut b = Buf {
        buf: [0; 32],
        len: 0,
    };
    let _ = b.write_fmt(args);
    let s = &b.buf[..b.len];
    let e = s.iter().position(|&c| c == b'e').unwrap_or(s.len());
    let mut d = Digits {
        buf: [0; 32],
        len: 0,
        decpt: 0,
    };
    for &c in &s[..e] {
        if c.is_ascii_digit() {
            d.buf[d.len as usize] = c;
            d.len += 1;
        }
    }
    while d.len > 1 && d.buf[d.len as usize - 1] == b'0' {
        d.len -= 1;
    }
    let mut exp = 0i32;
    let mut neg = false;
    for &c in &s[(e + 1).min(s.len())..] {
        match c {
            b'-' => neg = true,
            b'0'..=b'9' => exp = exp * 10 + (c - b'0') as i32,
            _ => {}
        }
    }
    d.decpt = if neg { -exp } else { exp } + 1;
    d
}

/// Shortest digits of `|v|`; `v` must be finite and nonzero.
pub fn shortest(v: f64) -> Digits {
    let v = v.abs();
    let d = sci(format_args!("{:e}", v));
    // Below 16 digits at most one candidate lies in the rounding interval (the decimal spacing
    // exceeds an ulp). At 16 and 17 there can be two; core's shortest mode does not break the
    // tie to even, so take the correctly rounded string of that length when it reads back.
    if d.len() < 16 {
        return d;
    }
    let r = sci(format_args!("{:.*e}", d.len() - 1, v));
    let mut b = Buf {
        buf: [0; 32],
        len: 0,
    };
    let _ = write!(b, "{}e{}", r.as_str(), r.decpt - r.len() as i32);
    let back = core::str::from_utf8(&b.buf[..b.len])
        .ok()
        .and_then(|s| s.parse::<f64>().ok());
    if back == Some(v) && r.len() <= d.len() {
        r
    } else {
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(v: f64) -> (String, i32) {
        let d = shortest(v);
        (d.as_str().to_string(), d.decpt)
    }

    #[test]
    fn ties_go_to_even() {
        assert_eq!(show(2f64.powi(-25)), ("29802322387695312".into(), -7));
        assert_eq!(show(9.425454303262663e13), ("9425454303262662".into(), 14));
    }

    #[test]
    fn plain_values() {
        assert_eq!(show(0.1), ("1".into(), 0));
        assert_eq!(show(1.5), ("15".into(), 1));
        assert_eq!(show(-1e300), ("1".into(), 301));
        assert_eq!(show(5e-324), ("5".into(), -323));
        assert_eq!(show(f64::MAX), ("17976931348623157".into(), 309));
        assert_eq!(show(100.0), ("1".into(), 3));
    }
}
