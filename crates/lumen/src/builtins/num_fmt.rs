//! Fast Number::toString(10) for the common finite values.

/// Powers of ten exactly representable as f64.
const POW10: [f64; 23] = [
    1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
    1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
];

/// Append Number::toString(`n`) to `out` when `n` is an integer below 2^53 in magnitude or a
/// fraction whose shortest round-tripping decimal form has a small, unique fixed-point spelling;
/// returns false (writing nothing) otherwise, for the caller's general algorithm.
///
/// For the fraction case: the first `p` with an integer `m` (< 2^53) such that
/// `m / 10^p == n` gives a decimal with the fewest fractional digits that converts back to `n`
/// (the division of two exact doubles is correctly rounded, exactly as parsing `m e-p` is). With
/// the integer part fixed, fewest fractional digits is fewest significant digits; requiring
/// that no other integer near `n·10^p` also round-trips makes the candidate the unique shortest, so
/// it is the one Number::toString picks.
pub(crate) fn fast_num_to_str(n: f64, out: &mut String) -> bool {
    use std::fmt::Write as _;
    if n.trunc() == n && n.abs() < 9_007_199_254_740_992.0 {
        let _ = write!(out, "{}", n as i64);
        return true;
    }
    let a = n.abs();
    // Fixed-point range of Number::toString with room for the digits in 53 bits.
    if !(1e-6..1e15).contains(&a) {
        return false;
    }
    for (p, &scale) in POW10.iter().enumerate().skip(1) {
        let scaled = a * scale;
        if scaled >= 9_007_199_254_740_992.0 {
            return false;
        }
        // The product may be off by an ulp, so the nearest integer is m or a neighbour.
        let m0 = scaled.round();
        let mut found = None;
        for c in [m0 - 1.0, m0, m0 + 1.0] {
            if c > 0.0 && c / scale == a {
                if found.is_some() {
                    return false; // several candidates: leave the tie-break to the caller
                }
                found = Some(c);
            }
        }
        if let Some(m) = found {
            let digits = m as u64;
            let mut buf = itoa_u64(digits);
            // Left-pad so there is at least one integer digit.
            while buf.len() <= p {
                buf.insert(0, '0');
            }
            let point = buf.len() - p;
            if n < 0.0 {
                out.push('-');
            }
            out.push_str(&buf[..point]);
            out.push('.');
            out.push_str(&buf[point..]);
            return true;
        }
    }
    false
}

fn itoa_u64(mut v: u64) -> String {
    let mut tmp = [0u8; 20];
    let mut k = tmp.len();
    loop {
        k -= 1;
        tmp[k] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    String::from_utf8_lossy(&tmp[k..]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::fast_num_to_str;

    #[test]
    fn agrees_with_shortest_round_trip() {
        let mut x: u64 = 0x2545_F491_4F6C_DD1D;
        let mut checked = 0;
        for k in 0..400_000u32 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = match k % 4 {
                0 => f64::from_bits(x),
                1 => (x % 10_000_000) as f64 / 10f64.powi((x >> 40) as i32 % 9),
                2 => (x >> 11) as f64 / (1u64 << 53) as f64 * 1000.0,
                _ => ((x % 2_000_000) as f64 - 1_000_000.0) * 0.001,
            };
            if !n.is_finite() {
                continue;
            }
            let mut s = String::new();
            if fast_num_to_str(n, &mut s) {
                checked += 1;
                assert_eq!(s.parse::<f64>().unwrap(), n, "{s}");
                // Rust's `{}` is the shortest round-trip too; digit strings must agree.
                let r = format!("{}", n);
                assert_eq!(s, r, "value {n:e}");
            }
        }
        assert!(checked > 100_000);
    }
}
