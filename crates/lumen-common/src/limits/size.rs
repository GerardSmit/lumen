//! Checked size arithmetic and fallible reservation for allocations a script can size. Failures
//! are the neutral [`TooLarge`]; the engine turns it into its own exception.

/// A requested size passed its cap, overflowed, or could not be reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLarge;

pub fn check(n: usize, max: usize) -> Result<usize, TooLarge> {
    if n > max {
        Err(TooLarge)
    } else {
        Ok(n)
    }
}

/// `a + b`, within `max`.
pub fn sum(a: usize, b: usize, max: usize) -> Result<usize, TooLarge> {
    check(a.checked_add(b).ok_or(TooLarge)?, max)
}

/// `len * count`, within `max`.
pub fn repeat(len: usize, count: usize, max: usize) -> Result<usize, TooLarge> {
    check(len.checked_mul(count).ok_or(TooLarge)?, max)
}

/// An empty vector with room for exactly `n` items, within `max`.
pub fn vec_with_capacity<T>(n: usize, max: usize) -> Result<Vec<T>, TooLarge> {
    check(n, max)?;
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| TooLarge)?;
    Ok(v)
}

/// An empty string with room for exactly `n` bytes, within `max`.
pub fn string_with_capacity(n: usize, max: usize) -> Result<String, TooLarge> {
    check(n, max)?;
    let mut s = String::new();
    s.try_reserve_exact(n).map_err(|_| TooLarge)?;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_is_checked() {
        assert_eq!(sum(3, 4, 7), Ok(7));
        assert_eq!(sum(3, 5, 7), Err(TooLarge));
        assert_eq!(sum(usize::MAX, 1, usize::MAX), Err(TooLarge));
        assert_eq!(repeat(1 << 20, 1 << 20, 1 << 40), Ok(1 << 40));
        assert_eq!(repeat(usize::MAX, 2, usize::MAX), Err(TooLarge));
    }

    #[test]
    fn reservation_respects_the_cap_and_the_allocator() {
        assert!(vec_with_capacity::<u64>(8, 8).is_ok());
        assert!(vec_with_capacity::<u64>(9, 8).is_err());
        assert!(vec_with_capacity::<u64>(usize::MAX / 8, usize::MAX).is_err());
        assert_eq!(string_with_capacity(16, 16).map(|s| s.capacity() >= 16), Ok(true));
    }
}
