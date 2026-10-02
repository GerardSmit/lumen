//! The MT19937 Mersenne Twister with the seeding and output conversions CPython's `_random` uses,
//! so seeded sequences match bit for bit. No OS calls: entropy is supplied by the caller.

pub const N: usize = 624;
const M: usize = 397;
const MATRIX_A: u32 = 0x9908_b0df;
const UPPER_MASK: u32 = 0x8000_0000;
const LOWER_MASK: u32 = 0x7fff_ffff;

#[derive(Clone)]
pub struct Mt19937 {
    state: [u32; N],
    index: usize,
}

impl Default for Mt19937 {
    fn default() -> Self {
        Self::new()
    }
}

impl Mt19937 {
    pub fn new() -> Self {
        let mut mt = Mt19937 { state: [0; N], index: N };
        mt.init_genrand(19650218);
        mt
    }

    pub fn init_genrand(&mut self, s: u32) {
        self.state[0] = s;
        for i in 1..N {
            let prev = self.state[i - 1];
            self.state[i] = 1812433253u32.wrapping_mul(prev ^ (prev >> 30)).wrapping_add(i as u32);
        }
        self.index = N;
    }

    /// Seeds from a little-endian array of 32-bit words (CPython's `init_by_array`).
    pub fn init_by_array(&mut self, key: &[u32]) {
        self.init_genrand(19650218);
        let mut i = 1usize;
        let mut j = 0usize;
        let mut k = N.max(key.len());
        while k > 0 {
            let prev = self.state[i - 1];
            self.state[i] = (self.state[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1664525)).wrapping_add(key[j]).wrapping_add(j as u32);
            i += 1;
            j += 1;
            if i >= N {
                self.state[0] = self.state[N - 1];
                i = 1;
            }
            if j >= key.len() {
                j = 0;
            }
            k -= 1;
        }
        k = N - 1;
        while k > 0 {
            let prev = self.state[i - 1];
            self.state[i] = (self.state[i] ^ (prev ^ (prev >> 30)).wrapping_mul(1566083941)).wrapping_sub(i as u32);
            i += 1;
            if i >= N {
                self.state[0] = self.state[N - 1];
                i = 1;
            }
            k -= 1;
        }
        self.state[0] = 0x8000_0000;
    }

    pub fn next_u32(&mut self) -> u32 {
        if self.index >= N {
            self.twist();
        }
        let mut y = self.state[self.index];
        self.index += 1;
        y ^= y >> 11;
        y ^= (y << 7) & 0x9d2c_5680;
        y ^= (y << 15) & 0xefc6_0000;
        y ^= y >> 18;
        y
    }

    fn twist(&mut self) {
        let mag = |y: u32| if y & 1 != 0 { MATRIX_A } else { 0 };
        for kk in 0..N - M {
            let y = (self.state[kk] & UPPER_MASK) | (self.state[kk + 1] & LOWER_MASK);
            self.state[kk] = self.state[kk + M] ^ (y >> 1) ^ mag(y);
        }
        for kk in N - M..N - 1 {
            let y = (self.state[kk] & UPPER_MASK) | (self.state[kk + 1] & LOWER_MASK);
            self.state[kk] = self.state[kk + M - N] ^ (y >> 1) ^ mag(y);
        }
        let y = (self.state[N - 1] & UPPER_MASK) | (self.state[0] & LOWER_MASK);
        self.state[N - 1] = self.state[M - 1] ^ (y >> 1) ^ mag(y);
        self.index = 0;
    }

    /// A float in [0, 1) with 53 random bits.
    pub fn next_f64(&mut self) -> f64 {
        let a = (self.next_u32() >> 5) as f64;
        let b = (self.next_u32() >> 6) as f64;
        (a * 67108864.0 + b) * (1.0 / 9007199254740992.0)
    }

    /// `k` random bits as little-endian 32-bit words; `k` must be positive.
    pub fn random_bits(&mut self, k: u64) -> Vec<u32> {
        let words = ((k - 1) / 32 + 1) as usize;
        let mut out = Vec::with_capacity(words);
        let mut left = k;
        for _ in 0..words {
            let mut r = self.next_u32();
            if left < 32 {
                r >>= 32 - left;
            }
            out.push(r);
            left = left.saturating_sub(32);
        }
        out
    }

    pub fn state(&self) -> (&[u32; N], usize) {
        (&self.state, self.index)
    }

    pub fn set_state(&mut self, state: [u32; N], index: usize) {
        self.state = state;
        self.index = index;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_reference_seed_42() {
        let mut mt = Mt19937::new();
        mt.init_by_array(&[42]);
        assert_eq!(mt.next_f64(), 0.6394267984578837);
        assert_eq!(mt.next_f64(), 0.025010755222666936);
    }
}
