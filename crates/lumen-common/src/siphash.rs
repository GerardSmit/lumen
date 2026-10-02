//! SipHash-1-3 (one compression round, three finalization rounds), the keyed hash CPython uses
//! for `str`/`bytes` hashing and for the source hash of hash-based `.pyc` files.

#[inline(always)]
fn half_round(a: &mut u64, b: &mut u64, c: &mut u64, d: &mut u64, s: u32, t: u32) {
    *a = a.wrapping_add(*b);
    *c = c.wrapping_add(*d);
    *b = b.rotate_left(s) ^ *a;
    *d = d.rotate_left(t) ^ *c;
    *a = a.rotate_left(32);
}

#[inline(always)]
fn round(v: &mut [u64; 4]) {
    let [v0, v1, v2, v3] = v;
    half_round(v0, v1, v2, v3, 13, 16);
    half_round(v2, v1, v0, v3, 17, 21);
}

/// SipHash-1-3 of `data` under the key `(k0, k1)`.
pub fn siphash13(k0: u64, k1: u64, data: &[u8]) -> u64 {
    let mut v = [k0 ^ 0x736f_6d65_7073_6575, k1 ^ 0x646f_7261_6e64_6f6d, k0 ^ 0x6c79_6765_6e65_7261, k1 ^ 0x7465_6462_7974_6573];
    let (chunks, rest) = data.as_chunks::<8>();
    for c in chunks {
        let m = u64::from_le_bytes(*c);
        v[3] ^= m;
        round(&mut v);
        v[0] ^= m;
    }
    let mut b = (data.len() as u64) << 56;
    for (i, &byte) in rest.iter().enumerate() {
        b |= (byte as u64) << (8 * i);
    }
    v[3] ^= b;
    round(&mut v);
    v[0] ^= b;
    v[2] ^= 0xff;
    round(&mut v);
    round(&mut v);
    round(&mut v);
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_cpython_source_hash() {
        // `_imp.source_hash(1, b'abc')` in CPython 3.12.
        assert_eq!(siphash13(1, 0, b"abc").to_le_bytes(), *b"\xaf\xbdc\xb5@\xc5\x06I");
    }
}
