//! Python `int` semantics on top of the shared arbitrary-precision integer.

pub use lumen_common::bigint::BigInt;

const HASH_MODULUS: u64 = (1 << 61) - 1;

pub trait PyInt: Sized {
    fn is_even(&self) -> bool;
    /// Floored quotient and remainder; the divisor must be non-zero.
    fn floor_divmod(&self, o: &Self) -> (Self, Self);
    fn floor_div(&self, o: &Self) -> Self {
        self.floor_divmod(o).0
    }
    fn floor_mod(&self, o: &Self) -> Self {
        self.floor_divmod(o).1
    }
    fn to_u64(&self) -> Option<u64>;
    /// Nearest f64, or `None` when the value is too large to represent (Python's OverflowError).
    fn to_float(&self) -> Option<f64>;
    /// Truncates toward zero; non-finite input yields zero.
    fn from_f64_trunc(f: f64) -> Self;
    /// Optional sign followed by digits in `radix`.
    fn parse_signed(s: &str, radix: u32) -> Option<Self>;
    /// Python's `hash(int)`: the value modulo 2^61 - 1 with its sign, never -1.
    fn py_hash(&self) -> i64;
    fn count_ones(&self) -> u32;
    /// Exactly `len` bytes of the magnitude or two's complement; `None` on overflow.
    fn to_py_bytes(&self, len: usize, big_endian: bool, signed: bool) -> Option<Vec<u8>>;
    fn from_py_bytes(bytes: &[u8], big_endian: bool, signed: bool) -> Self;
}

impl PyInt for BigInt {
    fn is_even(&self) -> bool {
        self.words().1.first().is_none_or(|&l| l & 1 == 0)
    }

    fn floor_divmod(&self, o: &Self) -> (Self, Self) {
        self.divmod_floor(o).expect("divisor checked non-zero")
    }

    fn to_u64(&self) -> Option<u64> {
        match self.words() {
            (false, []) => Some(0),
            (false, [m]) => Some(*m),
            _ => None,
        }
    }

    fn to_float(&self) -> Option<f64> {
        Some(self.to_f64()).filter(|f| f.is_finite())
    }

    fn from_f64_trunc(f: f64) -> Self {
        BigInt::from_f64(f.trunc()).unwrap_or_else(BigInt::zero)
    }

    fn parse_signed(s: &str, radix: u32) -> Option<Self> {
        let (neg, digits) = match s.strip_prefix('-') {
            Some(r) => (true, r),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let v = BigInt::parse_radix(digits, radix)?;
        Some(if neg { v.neg() } else { v })
    }

    fn py_hash(&self) -> i64 {
        let (neg, mag) = self.words();
        let m = HASH_MODULUS as u128;
        let acc = mag.iter().rev().fold(0u128, |acc, &l| ((acc << 64) | l as u128) % m);
        let h = if neg { -(acc as i64) } else { acc as i64 };
        if h == -1 {
            -2
        } else {
            h
        }
    }

    fn count_ones(&self) -> u32 {
        self.words().1.iter().map(|l| l.count_ones()).sum()
    }

    fn to_py_bytes(&self, len: usize, big_endian: bool, signed: bool) -> Option<Vec<u8>> {
        let (neg, mag) = self.words();
        if neg && !signed {
            return None;
        }
        let bits = match (signed, neg) {
            (false, _) => self.bit_len(),
            (true, false) => self.bit_len() + 1,
            (true, true) => self.add(&BigInt::from_i64(1)).bit_len() + 1,
        };
        if bits.div_ceil(8) > len {
            return None;
        }
        let mut bytes: Vec<u8> = mag.iter().flat_map(|l| l.to_le_bytes()).chain(std::iter::repeat(0)).take(len).collect();
        if neg {
            negate_le(&mut bytes);
        }
        if big_endian {
            bytes.reverse();
        }
        Some(bytes)
    }

    fn from_py_bytes(bytes: &[u8], big_endian: bool, signed: bool) -> Self {
        let mut le = bytes.to_vec();
        if big_endian {
            le.reverse();
        }
        let neg = signed && le.last().is_some_and(|&b| b & 0x80 != 0);
        if neg {
            negate_le(&mut le);
        }
        let mag = le
            .chunks(8)
            .map(|c| {
                let mut w = [0u8; 8];
                w[..c.len()].copy_from_slice(c);
                u64::from_le_bytes(w)
            })
            .collect();
        BigInt::from_words(neg, mag)
    }
}

fn negate_le(bytes: &mut [u8]) {
    let mut carry = true;
    for b in bytes {
        let (v, c) = (!*b).overflowing_add(carry as u8);
        *b = v;
        carry = c;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> BigInt {
        BigInt::parse_signed(s, 10).unwrap()
    }

    #[test]
    fn hash_follows_cpython() {
        assert_eq!(p("2305843009213693951").py_hash(), 0);
        assert_eq!(BigInt::from_i64(1).shl(61).py_hash(), 1);
        assert_eq!(p("-2305843009213693952").py_hash(), -2);
        assert_eq!(p("-2305843009213693953").py_hash(), -2);
    }

    #[test]
    fn bytes_round_trip() {
        let b = p("-129");
        let bytes = b.to_py_bytes(2, true, true).unwrap();
        assert_eq!(bytes, vec![0xFF, 0x7F]);
        assert_eq!(BigInt::from_py_bytes(&bytes, true, true), b);
        assert_eq!(p("255").to_py_bytes(1, false, false), Some(vec![255]));
        assert_eq!(p("255").to_py_bytes(1, false, true), None);
        assert_eq!(p("-128").to_py_bytes(1, false, true), Some(vec![0x80]));
        assert_eq!(p("-129").to_py_bytes(1, false, true), None);
        assert_eq!(p("-1").to_py_bytes(1, false, false), None);
        let wide = p("-340282366920938463463374607431768211457");
        let bytes = wide.to_py_bytes(17, false, true).unwrap();
        assert_eq!(BigInt::from_py_bytes(&bytes, false, true), wide);
        assert_eq!(BigInt::from_py_bytes(&[0xFF, 0xFF], false, false).to_i64(), Some(65535));
        assert_eq!(BigInt::from_py_bytes(&[], false, true).to_i64(), Some(0));
    }

    #[test]
    fn float_conversions() {
        assert_eq!(BigInt::from_i64(1).shl(100).to_float(), Some(2f64.powi(100)));
        assert_eq!(BigInt::from_i64(1).shl(1024).to_float(), None);
        assert_eq!(BigInt::from_i64(1).shl(1024).sub(&BigInt::from_i64(1)).to_float(), None);
        assert_eq!(BigInt::from_f64_trunc(-1e20).to_string_radix(10), "-100000000000000000000");
        assert_eq!(BigInt::from_f64_trunc(2.9).to_i64(), Some(2));
        assert_eq!(BigInt::from_f64_trunc(-2.9).to_i64(), Some(-2));
    }

    #[test]
    fn misc() {
        assert!(p("-4").is_even() && !p("7").is_even() && BigInt::zero().is_even());
        assert_eq!(p("-5").to_u64(), None);
        assert_eq!(p("18446744073709551615").to_u64(), Some(u64::MAX));
        assert_eq!(p("-255").count_ones(), 8);
        assert_eq!(BigInt::parse_signed("+ff", 16).unwrap().to_i64(), Some(255));
        assert_eq!(BigInt::parse_signed("-", 10), None);
    }
}
