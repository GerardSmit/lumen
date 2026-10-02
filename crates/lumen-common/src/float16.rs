//! IEEE 754 binary16 ("half") conversion with round-half-to-even, rounding straight from the
//! binary64 value (no intermediate binary32 rounding).

/// Half-float bits for `x`, or `None` when the finite value rounds beyond the largest finite half.
/// Infinities map to infinities; a NaN maps to a quiet NaN with the sign preserved.
pub fn f64_to_f16_bits(x: f64) -> Option<u16> {
    let bits = x.to_bits();
    let sign = ((bits >> 63) as u16) << 15;
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let mant = bits & ((1u64 << 52) - 1);
    if exp == 0x7ff {
        return Some(sign | 0x7c00 | if mant != 0 { 0x200 } else { 0 });
    }
    if exp == 0 {
        return Some(sign);
    }
    let e = exp - 1023;
    if e >= 16 {
        return None;
    }
    if e < -25 {
        return Some(sign);
    }
    let m = (1u64 << 52) | mant;
    let (shift, base) = if e >= -14 { (42u32, ((e + 15) as u64) << 10) } else { ((42 + (-14 - e)) as u32, 0) };
    let mut q = m >> shift;
    let rem = m & ((1u64 << shift) - 1);
    let half = 1u64 << (shift - 1);
    if rem > half || (rem == half && q & 1 == 1) {
        q += 1;
    }
    let out = if e >= -14 { base + (q - 1024) } else { q };
    if out >= 0x7c00 {
        return None;
    }
    Some(sign | out as u16)
}

/// The binary64 value of half-float bits; NaN payloads are preserved in the high mantissa bits.
#[inline]
pub fn f16_bits_to_f64(h: u16) -> f64 {
    f16_bits_to_f32(h) as f64
}

/// The binary32 value of half-float bits (exact: every half is a float).
pub fn f16_bits_to_f32(h: u16) -> f32 {
    let sign = (h as u32 & 0x8000) << 16;
    let exp = (h >> 10) & 0x1f;
    let mant = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // Subnormal half: normalize into a binary32 normal number.
            let shift = mant.leading_zeros() - 21;
            let m = (mant << shift) & 0x3ff;
            sign | ((113 - shift) << 23) | (m << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        sign | ((exp as u32 + 127 - 15) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_half() {
        for h in 0..=u16::MAX {
            let f = f16_bits_to_f64(h);
            if f.is_nan() {
                continue;
            }
            assert_eq!(f64_to_f16_bits(f), Some(h), "{h:#06x}");
        }
    }

    #[test]
    fn rounding_and_overflow() {
        assert_eq!(f64_to_f16_bits(65504.0), Some(0x7bff));
        assert_eq!(f64_to_f16_bits(65519.0), Some(0x7bff));
        assert_eq!(f64_to_f16_bits(65520.0), None);
        assert_eq!(f64_to_f16_bits(2f64.powi(-25)), Some(0));
        assert_eq!(f64_to_f16_bits(2f64.powi(-24)), Some(1));
        assert_eq!(f64_to_f16_bits(3.0 * 2f64.powi(-25)), Some(2));
        assert_eq!(f64_to_f16_bits(-0.0), Some(0x8000));
        assert_eq!(f64_to_f16_bits(f64::NEG_INFINITY), Some(0xfc00));
        assert_eq!(f64_to_f16_bits(f64::NAN), Some(0x7e00));
    }
}
