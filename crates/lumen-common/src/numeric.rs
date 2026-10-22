//! Small numeric helpers shared by CSS list-repeat, interpolation and similar code.

/// Linear interpolation from `a` to `b` at `progress`.
pub fn lerp(a: f64, b: f64, progress: f64) -> f64 {
    a + (b - a) * progress
}

/// End offset of the CSS `<number>` token starting at `at` in `bytes` (`at` itself when
/// no digits follow).
pub fn number_end(bytes: &[u8], mut at: usize) -> usize {
    if matches!(bytes.get(at), Some(b'+' | b'-')) { at += 1; }
    while bytes.get(at).is_some_and(u8::is_ascii_digit) { at += 1; }
    if bytes.get(at) == Some(&b'.') && bytes.get(at+1).is_some_and(u8::is_ascii_digit) {
        at += 1;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) { at += 1; }
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        let exponent = at+1;
        let digits = exponent + usize::from(matches!(bytes.get(exponent), Some(b'+' | b'-')));
        if bytes.get(digits).is_some_and(u8::is_ascii_digit) {
            at = digits+1;
            while bytes.get(at).is_some_and(u8::is_ascii_digit) { at += 1; }
        }
    }
    at
}

/// Splits a leading CSS number (parsed as `f64`) from the rest of `value`. `None` when
/// the number does not parse.
pub fn split_number_unit(value: &str) -> Option<(f64, &str)> {
    let end = number_end(value.as_bytes(), 0);
    Some((value[..end].parse().ok()?, &value[end..]))
}

/// Least common multiple of `a` and `b`, or `None` when either is zero, the
/// product overflows, or the result exceeds `max`.
pub fn lcm_capped(a: usize, b: usize, max: usize) -> Option<usize> {
    if a == 0 || b == 0 {
        return None;
    }
    let (mut x, mut y) = (a, b);
    while y != 0 {
        (x, y) = (y, x % y);
    }
    let lcm = (a / x).checked_mul(b)?;
    (lcm <= max).then_some(lcm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_lcm_within_cap() {
        assert_eq!(lcm_capped(4, 6, 64), Some(12));
        assert_eq!(lcm_capped(3, 3, 64), Some(3));
        assert_eq!(lcm_capped(1, 7, 7), Some(7));
    }

    #[test]
    fn rejects_zero_cap_and_overflow() {
        assert_eq!(lcm_capped(0, 5, 64), None);
        assert_eq!(lcm_capped(5, 0, 64), None);
        assert_eq!(lcm_capped(8, 9, 64), None);
        assert_eq!(lcm_capped(usize::MAX, usize::MAX - 1, usize::MAX), None);
    }
}
