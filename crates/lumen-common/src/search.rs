//! Byte-string search for both languages' strings and byte buffers, over the `memchr` crate's
//! vectorized scans. UTF-8 and the smuggled spellings are self-synchronizing, so byte matches in
//! text are character matches.

/// Index of the first `b` in `hay`.
#[inline]
pub fn memchr(b: u8, hay: &[u8]) -> Option<usize> {
    memchr::memchr(b, hay)
}

/// Index of the last `b` in `hay`.
#[inline]
pub fn memrchr(b: u8, hay: &[u8]) -> Option<usize> {
    memchr::memrchr(b, hay)
}

/// Index of the first occurrence of `needle` in `hay` (0 for an empty needle).
#[inline]
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::find(hay, needle)
}

/// Index of the last occurrence of `needle` in `hay` (`hay.len()` for an empty needle).
///
/// Text that is a `str` searches faster backwards with `str::rfind` (std's Two-Way adds a
/// byte-set skip that `memchr`'s reverse search lacks).
#[inline]
pub fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    memchr::memmem::rfind(hay, needle)
}

/// The first index at or after `from` where `needle` occurs in `hay`.
pub fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    Some(find(hay.get(from..)?, needle)? + from)
}

/// The last index at or before `from` where `needle` occurs in `hay`.
pub fn rfind_upto(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    let end = from.min(hay.len().checked_sub(needle.len())?) + needle.len();
    rfind(&hay[..end], needle)
}

/// The number of non-overlapping occurrences of `needle` in `hay`, up to `max`.
pub fn count(hay: &[u8], needle: &[u8], max: usize) -> usize {
    if needle.is_empty() {
        return (hay.len() + 1).min(max);
    }
    memchr::memmem::find_iter(hay, needle).take(max).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive(h: &[u8], n: &[u8]) -> Option<usize> {
        (0..=h.len().checked_sub(n.len())?).find(|&i| &h[i..i + n.len()] == n)
    }

    fn naive_r(h: &[u8], n: &[u8]) -> Option<usize> {
        (0..=h.len().checked_sub(n.len())?).rev().find(|&i| &h[i..i + n.len()] == n)
    }

    #[test]
    fn agrees_with_a_naive_search() {
        let hays: [&[u8]; 6] = [b"", b"a", b"abracadabra", b"aaaaaaaaaaaaaaaaaaaab", b"xyzxyzxyzxy\x00\xffzz", b"mississippi river"];
        let needles: [&[u8]; 12] = [b"", b"a", b"b", b"ab", b"abra", b"cad", b"aab", b"aaaaab", b"xyzxy", b"\xffz", b"ssi", b"issip"];
        for h in hays {
            for n in needles {
                if !n.is_empty() {
                    assert_eq!(find(h, n), naive(h, n), "{h:?} {n:?}");
                    assert_eq!(rfind(h, n), naive_r(h, n), "{h:?} {n:?}");
                }
            }
        }
        assert_eq!(find(b"abc", b""), Some(0));
        assert_eq!(rfind(b"abc", b""), Some(3));
        assert_eq!(count(b"aaaa", b"aa", usize::MAX), 2);
        assert_eq!(count(b"abc", b"", usize::MAX), 4);
        assert_eq!(find_from(b"abcabc", b"abc", 1), Some(3));
        assert_eq!(find_from(b"abc", b"", 2), Some(2));
        assert_eq!(find_from(b"abc", b"", 4), None);
        assert_eq!(rfind_upto(b"abcabc", b"abc", 6), Some(3));
        assert_eq!(rfind_upto(b"abcabc", b"abc", 2), Some(0));
        assert_eq!(rfind_upto(b"abc", b"", 9), Some(3));
        assert_eq!(rfind_upto(b"ab", b"abc", 9), None);
    }

    #[test]
    fn random_small_alphabets() {
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = |m: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % m
        };
        for _ in 0..20000 {
            let k = next(3) + 2;
            let len = if next(2) == 0 { 40 } else { 900 };
            let h: Vec<u8> = (0..next(len)).map(|_| b'a' + next(k) as u8).collect();
            let n: Vec<u8> = (0..next(6) + 1).map(|_| b'a' + next(k) as u8).collect();
            assert_eq!(find(&h, &n), naive(&h, &n), "{h:?} {n:?}");
            assert_eq!(rfind(&h, &n), naive_r(&h, &n), "{h:?} {n:?}");
        }
    }

    #[test]
    fn byte_scans() {
        let h: Vec<u8> = (0..100u8).collect();
        for b in [0u8, 7, 8, 63, 99, 200] {
            assert_eq!(memchr(b, &h), h.iter().position(|&x| x == b));
            assert_eq!(memrchr(b, &h), h.iter().rposition(|&x| x == b));
        }
    }
}
