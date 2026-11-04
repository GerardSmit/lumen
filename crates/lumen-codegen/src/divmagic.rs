//! Division by a constant as a multiply-high (Hacker's Delight, chapter 10), for backends whose
//! target has no divide instruction or a slow one. Backend-neutral: each backend emits the
//! sequence documented on [`signed32`] / [`unsigned32`] with its own instructions.

/// The magic multiplier `m` and shift `s` for signed 32-bit division by `d`
/// (`d` ∉ {-1, 0, 1}). The quotient of `n / d` (truncating) is:
///
/// ```text
/// q = mulhs(n, m)            // high word of the signed 64-bit product
/// if d > 0 && m < 0 { q += n }
/// if d < 0 && m > 0 { q -= n }
/// q >>= s                    // arithmetic
/// q += (q as u32) >> 31      // round toward zero
/// ```
pub fn signed32(d: i32) -> (i32, u32) {
    debug_assert!(!matches!(d, -1..=1));
    const TWO31: u32 = 0x8000_0000;
    let ad = d.unsigned_abs();
    let t = TWO31 + ((d as u32) >> 31);
    let anc = t - 1 - t % ad;
    let mut p = 31u32;
    let (mut q1, mut r1) = (TWO31 / anc, TWO31 - (TWO31 / anc) * anc);
    let (mut q2, mut r2) = (TWO31 / ad, TWO31 - (TWO31 / ad) * ad);
    loop {
        p += 1;
        q1 = q1.wrapping_mul(2);
        r1 = r1.wrapping_mul(2);
        if r1 >= anc {
            q1 = q1.wrapping_add(1);
            r1 = r1.wrapping_sub(anc);
        }
        q2 = q2.wrapping_mul(2);
        r2 = r2.wrapping_mul(2);
        if r2 >= ad {
            q2 = q2.wrapping_add(1);
            r2 = r2.wrapping_sub(ad);
        }
        let delta = ad - r2;
        if !(q1 < delta || (q1 == delta && r1 == 0)) {
            break;
        }
    }
    let m = q2.wrapping_add(1) as i32;
    (if d < 0 { m.wrapping_neg() } else { m }, p - 32)
}

/// The magic multiplier `m`, the `add` indicator and shift `s` for unsigned 32-bit division by
/// `d` (`d >= 2`, not a power of two). The quotient of `n / d` is:
///
/// ```text
/// q = mulhu(n, m)
/// if add { q = (((n - q) >> 1) + q) >> (s - 1) } else { q >>= s }
/// ```
pub fn unsigned32(d: u32) -> (u32, bool, u32) {
    debug_assert!(d >= 2 && !d.is_power_of_two());
    let mut add = false;
    let nc = u32::MAX - 0u32.wrapping_sub(d) % d;
    let mut p = 31u32;
    let (mut q1, mut r1) = (0x8000_0000 / nc, 0x8000_0000 - (0x8000_0000 / nc) * nc);
    let (mut q2, mut r2) = (0x7fff_ffff / d, 0x7fff_ffff - (0x7fff_ffff / d) * d);
    loop {
        p += 1;
        if r1 >= nc - r1 {
            q1 = q1.wrapping_mul(2).wrapping_add(1);
            r1 = r1.wrapping_mul(2).wrapping_sub(nc);
        } else {
            q1 = q1.wrapping_mul(2);
            r1 = r1.wrapping_mul(2);
        }
        if r2.wrapping_add(1) >= d - r2 {
            if q2 >= 0x7fff_ffff {
                add = true;
            }
            q2 = q2.wrapping_mul(2).wrapping_add(1);
            r2 = r2.wrapping_mul(2).wrapping_add(1).wrapping_sub(d);
        } else {
            if q2 >= 0x8000_0000 {
                add = true;
            }
            q2 = q2.wrapping_mul(2);
            r2 = r2.wrapping_mul(2).wrapping_add(1);
        }
        let delta = d - 1 - r2;
        if !(p < 64 && (q1 < delta || (q1 == delta && r1 == 0))) {
            break;
        }
    }
    (q2.wrapping_add(1), add, p - 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sdiv(n: i32, d: i32) -> i32 {
        let (m, s) = signed32(d);
        let mut q = ((n as i64 * m as i64) >> 32) as i32;
        if d > 0 && m < 0 {
            q = q.wrapping_add(n);
        }
        if d < 0 && m > 0 {
            q = q.wrapping_sub(n);
        }
        q >>= s;
        q.wrapping_add(((q as u32) >> 31) as i32)
    }

    fn udiv(n: u32, d: u32) -> u32 {
        let (m, add, s) = unsigned32(d);
        let q = ((n as u64 * m as u64) >> 32) as u32;
        if add {
            ((n - q) / 2 + q) >> (s - 1)
        } else {
            q >> s
        }
    }

    fn samples() -> Vec<u32> {
        let mut v: Vec<u32> = (0..2000).collect();
        let mut x = 0x9e37_79b9u32;
        for _ in 0..4000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            v.push(x);
        }
        for k in 0..32 {
            let p = 1u32 << k;
            v.extend([p, p.wrapping_sub(1), p.wrapping_add(1)]);
        }
        v.extend([
            u32::MAX,
            u32::MAX - 1,
            0x7fff_ffff,
            0x8000_0000,
            0x8000_0001,
        ]);
        v
    }

    #[test]
    fn signed_magic_matches_division() {
        let ns = samples();
        let mut ds: Vec<i32> = (2..1100).flat_map(|d| [d, -d]).collect();
        ds.extend([
            7,
            10,
            1000,
            1_000_003,
            0x7fff_ffff,
            -0x7fff_ffff,
            i32::MIN,
            641,
            6700417,
        ]);
        ds.extend(
            ns.iter()
                .map(|&n| n as i32)
                .filter(|d| !matches!(d, -1..=1))
                .take(600),
        );
        for &d in &ds {
            for &n in &ns {
                let n = n as i32;
                assert_eq!(sdiv(n, d), n.wrapping_div(d), "{n} / {d}");
            }
        }
    }

    #[test]
    fn unsigned_magic_matches_division() {
        let ns = samples();
        let mut ds: Vec<u32> = (3..1100).filter(|d: &u32| !d.is_power_of_two()).collect();
        ds.extend([
            7,
            10,
            1000,
            1_000_003,
            0x7fff_ffff,
            0x8000_0001,
            u32::MAX,
            641,
            6700417,
        ]);
        ds.extend(
            ns.iter()
                .copied()
                .filter(|&d| d >= 3 && !d.is_power_of_two())
                .take(600),
        );
        for &d in &ds {
            for &n in &ns {
                assert_eq!(udiv(n, d), n / d, "{n} / {d}");
            }
        }
    }
}
