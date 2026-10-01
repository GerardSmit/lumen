//! Correctly rounded floating-point summation: Shewchuk's nonoverlapping partials with CPython's
//! `math.fsum` handling of specials and its final round-half-to-even step.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsumError {
    /// A partial overflowed although every input was finite.
    Overflow,
    /// The inputs held both `inf` and `-inf`.
    InfMinusInf,
}

/// An exact running sum; `add` the values, then read the correctly rounded `result`.
#[derive(Default)]
pub struct Fsum {
    partials: Vec<f64>,
    special: f64,
    inf: f64,
    overflow: bool,
}

impl Fsum {
    pub fn new() -> Fsum {
        Fsum::default()
    }

    pub fn add(&mut self, v: f64) {
        let mut x = v;
        let mut i = 0;
        for j in 0..self.partials.len() {
            let mut y = self.partials[j];
            if x.abs() < y.abs() {
                core::mem::swap(&mut x, &mut y);
            }
            let hi = x + y;
            let lo = y - (hi - x);
            if lo != 0.0 {
                self.partials[i] = lo;
                i += 1;
            }
            x = hi;
        }
        self.partials.truncate(i);
        if x != 0.0 {
            if !x.is_finite() {
                // Specials (and overflow) make the finite partials irrelevant.
                if v.is_finite() {
                    self.overflow = true;
                }
                if v.is_infinite() {
                    self.inf += v;
                }
                self.special += v;
                self.partials.clear();
            } else {
                self.partials.push(x);
            }
        }
    }

    pub fn result(&self) -> Result<f64, FsumError> {
        if self.overflow {
            return Err(FsumError::Overflow);
        }
        if self.special != 0.0 {
            return if self.inf.is_nan() { Err(FsumError::InfMinusInf) } else { Ok(self.special) };
        }
        let p = &self.partials;
        let mut n = p.len();
        if n == 0 {
            return Ok(0.0);
        }
        n -= 1;
        let mut hi = p[n];
        let mut lo = 0.0;
        while n > 0 {
            n -= 1;
            let x = hi;
            let y = p[n];
            hi = x + y;
            lo = y - (hi - x);
            if lo != 0.0 {
                break;
            }
        }
        // Round half to even: when the residual and the next partial agree in sign, the true sum
        // lies past the halfway point `hi + lo`.
        if n > 0 && ((lo < 0.0 && p[n - 1] < 0.0) || (lo > 0.0 && p[n - 1] > 0.0)) {
            let y = lo * 2.0;
            let x = hi + y;
            if y == x - hi {
                hi = x;
            }
        }
        Ok(hi)
    }
}

/// `math.fsum` over a slice.
pub fn fsum(values: &[f64]) -> Result<f64, FsumError> {
    let mut s = Fsum::new();
    for &v in values {
        s.add(v);
    }
    s.result()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_and_special() {
        assert_eq!(fsum(&[0.1; 10]), Ok(1.0));
        assert_eq!(fsum(&[1e100, 1.0, -1e100, 1e-100, 1e50, -1.0, -1e50]), Ok(1e-100));
        assert_eq!(fsum(&[1.0, 1e-16, 1e-16]), Ok(1.0000000000000002));
        assert_eq!(fsum(&[f64::MAX, f64::MAX]), Err(FsumError::Overflow));
        assert_eq!(fsum(&[f64::INFINITY, f64::NEG_INFINITY]), Err(FsumError::InfMinusInf));
        assert_eq!(fsum(&[f64::INFINITY, 1.0]), Ok(f64::INFINITY));
        assert!(fsum(&[f64::NAN, 1.0]).unwrap().is_nan());
        assert_eq!(fsum(&[]), Ok(0.0));
    }
}
