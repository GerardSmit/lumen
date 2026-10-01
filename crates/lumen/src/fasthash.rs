//! The hasher behind the interpreter's hot maps (scope variables, property tables, interning,
//! Map/Set). Scripts choose most of the keys, so a fixed hash function would let one precompute
//! keys that all collide and turn every insert into a linear probe (hash flooding). Each process
//! therefore draws a random seed (from std's `RandomState`) that enters every multiply, in the
//! foldhash style: `folded_multiply(word ^ state, seed)`, where the 64x64->128-bit product folds
//! its halves together. That costs about what FxHash's multiply-rotate does per word, consumes
//! up to 16 bytes of a string per multiply, and has no seed-independent collisions — unlike a
//! seeded FxHash, where flipping bit 63 of one word and bit 4 of the next collides for any seed.
//! Nothing JS-visible may depend on hash order: it changes from run to run.

use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

#[inline(always)]
fn folded_multiply(x: u64, y: u64) -> u64 {
    let full = (x as u128).wrapping_mul(y as u128);
    (full as u64) ^ ((full >> 64) as u64)
}

static SEED: AtomicU64 = AtomicU64::new(0);
static FOLD: AtomicU64 = AtomicU64::new(0);

/// The process's (initial state, multiplier) pair. Plain atomics rather than a `OnceLock` keep
/// the per-hash cost to two loads.
#[inline(always)]
fn seeds() -> (u64, u64) {
    let fold = FOLD.load(Ordering::Acquire);
    if fold == 0 {
        return init_seeds();
    }
    (SEED.load(Ordering::Relaxed), fold)
}

#[cold]
#[inline(never)]
fn init_seeds() -> (u64, u64) {
    static SEEDS: OnceLock<(u64, u64)> = OnceLock::new();
    let seeds = *SEEDS.get_or_init(|| {
        let random = std::collections::hash_map::RandomState::new();
        // A multiplier with few set bits would barely mix; keep it odd and dense.
        let mut fold = random.hash_one(1u64) | 1;
        while !(24..=40).contains(&fold.count_ones()) {
            fold = folded_multiply(fold, 0x9e37_79b9_7f4a_7c15) | 1;
        }
        (random.hash_one(0u64), fold)
    });
    SEED.store(seeds.0, Ordering::Relaxed);
    FOLD.store(seeds.1, Ordering::Release);
    seeds
}

#[inline(always)]
fn read_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

#[inline(always)]
fn read_u32(b: &[u8], at: usize) -> u64 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as u64
}

/// [`FxHasher::write`] for more than 16 bytes: a multiply per 16, the last (overlapping) 16
/// read as a pair. Out of line to keep the inlined short-key path small.
#[inline(never)]
fn write_long(mut acc: u64, fold: u64, bytes: &[u8]) -> u64 {
    let len = bytes.len();
    let mut at = 0;
    while len - at > 16 {
        acc = folded_multiply(read_u64(bytes, at) ^ acc, read_u64(bytes, at + 8) ^ fold);
        at += 16;
    }
    folded_multiply(
        read_u64(bytes, len - 16) ^ acc,
        read_u64(bytes, len - 8) ^ fold,
    )
}

pub struct FxHasher {
    acc: u64,
    fold: u64,
}

impl Default for FxHasher {
    #[inline]
    fn default() -> Self {
        let (acc, fold) = seeds();
        FxHasher { acc, fold }
    }
}

impl FxHasher {
    #[inline(always)]
    fn add(&mut self, word: u64) {
        self.acc = folded_multiply(self.acc ^ word, self.fold);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let len = bytes.len();
        // Rotating by the length keeps equal-content reads of different lengths apart.
        let acc = self.acc.rotate_right(len as u32);
        // Two (possibly overlapping) reads cover every byte, so for a fixed length the pair
        // determines the bytes.
        let (lo, hi) = if len <= 16 {
            if len >= 8 {
                (read_u64(bytes, 0), read_u64(bytes, len - 8))
            } else if len >= 4 {
                (read_u32(bytes, 0), read_u32(bytes, len - 4))
            } else if len > 0 {
                (
                    bytes[0] as u64,
                    (bytes[len / 2] as u64) << 8 | bytes[len - 1] as u64,
                )
            } else {
                (0, 0)
            }
        } else {
            self.acc = write_long(acc, self.fold, bytes);
            return;
        };
        self.acc = folded_multiply(lo ^ acc, hi ^ self.fold);
    }
    #[inline]
    fn write_u8(&mut self, i: u8) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        self.acc
    }
}

/// The seeded hash of a byte string alone (no `Hash for str` terminator write).
#[inline]
pub fn hash_bytes(bytes: &[u8]) -> u64 {
    let mut h = FxHasher::default();
    h.write(bytes);
    h.finish()
}

/// Builds [`FxHasher`]s from seeds copied into each map at creation, so a hash costs no load
/// of (or initialization check on) the process-wide seeds.
#[derive(Clone, Copy)]
pub struct FastBuild {
    seed: u64,
    fold: u64,
}

impl Default for FastBuild {
    #[inline]
    fn default() -> Self {
        let (seed, fold) = seeds();
        FastBuild { seed, fold }
    }
}

impl BuildHasher for FastBuild {
    type Hasher = FxHasher;
    #[inline(always)]
    fn build_hasher(&self) -> FxHasher {
        FxHasher {
            acc: self.seed,
            fold: self.fold,
        }
    }
}

pub type FastMap<K, V> = std::collections::HashMap<K, V, FastBuild>;
pub type FastSet<K> = std::collections::HashSet<K, FastBuild>;

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::hash::Hash;
    use std::time::{Duration, Instant};

    /// The previous unseeded FxHash, to construct keys an attacker could have precomputed.
    #[derive(Default)]
    pub(crate) struct OldFx(u64);

    const OLD_K: u64 = 0x51_7c_c1_b7_27_22_0a_95;

    impl Hasher for OldFx {
        fn write(&mut self, mut bytes: &[u8]) {
            while bytes.len() >= 8 {
                self.write_u64(u64::from_le_bytes(bytes[..8].try_into().unwrap()));
                bytes = &bytes[8..];
            }
            if bytes.len() >= 4 {
                self.write_u64(u32::from_le_bytes(bytes[..4].try_into().unwrap()) as u64);
                bytes = &bytes[4..];
            }
            for &b in bytes {
                self.write_u64(b as u64);
            }
        }
        fn write_u8(&mut self, i: u8) {
            self.write_u64(i as u64);
        }
        fn write_u32(&mut self, i: u32) {
            self.write_u64(i as u64);
        }
        fn write_u64(&mut self, i: u64) {
            self.0 = (self.0.rotate_left(5) ^ i).wrapping_mul(OLD_K);
        }
        fn write_usize(&mut self, i: usize) {
            self.write_u64(i as u64);
        }
        fn finish(&self) -> u64 {
            self.0.rotate_left(26)
        }
    }

    fn inverse(k: u64) -> u64 {
        let mut x = k;
        for _ in 0..6 {
            x = x.wrapping_mul(2u64.wrapping_sub(k.wrapping_mul(x)));
        }
        x
    }

    /// `n` distinct 16-byte ASCII strings whose old `Hash for str` digests, continued from
    /// `prefix`'s state, are all identical: the first word is random, the second is solved for
    /// by inverting the old rounds and kept when every byte is ASCII (about 1 try in 256).
    pub(crate) fn old_colliding_keys(prefix: &OldFx, n: usize) -> Vec<String> {
        let kinv = inverse(OLD_K);
        let target_state = 0x0123_4567_89ab_cdefu64;
        // State before the 0xff terminator; then before the second word.
        let before_term = (target_state.wrapping_mul(kinv) ^ 0xff).rotate_right(5);
        let before_w2 = before_term.wrapping_mul(kinv);
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        while out.len() < n {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            // Printable, and not a `#` (private name) or NUL (internal slot) prefix.
            let mut w1 = rng.to_le_bytes().map(|b| 0x20 + b % 95);
            if w1[0] == b'#' {
                w1[0] = b'$';
            }
            let w1 = u64::from_le_bytes(w1);
            let after_w1 = (prefix.0.rotate_left(5) ^ w1).wrapping_mul(OLD_K);
            let w2 = after_w1.rotate_left(5) ^ before_w2;
            if w2 & 0x8080_8080_8080_8080 != 0 || !seen.insert((w1, w2)) {
                continue;
            }
            let mut bytes = w1.to_le_bytes().to_vec();
            bytes.extend_from_slice(&w2.to_le_bytes());
            out.push(String::from_utf8(bytes).unwrap());
        }
        out
    }

    fn benign_keys(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("benign-key-{i:05}")).collect()
    }

    fn time_inserts(keys: &[String]) -> Duration {
        let start = Instant::now();
        let mut set: FastSet<&str> = FastSet::default();
        for k in keys {
            set.insert(k);
        }
        assert_eq!(set.len(), keys.len());
        start.elapsed()
    }

    #[test]
    fn hash_flooding_keys_for_the_old_hash_do_not_collide() {
        let keys = old_colliding_keys(&OldFx::default(), 20_000);
        let old = |k: &String| {
            let mut h = OldFx::default();
            k.as_str().hash(&mut h);
            h.finish()
        };
        assert!(keys.iter().all(|k| old(k) == old(&keys[0])));
        let new: FastSet<u64> = keys
            .iter()
            .map(|k| FastBuild::default().hash_one(k))
            .collect();
        assert!(new.len() > keys.len() - 4);
        let hostile = time_inserts(&keys);
        let benign = time_inserts(&benign_keys(keys.len()));
        // Under the old hash this was ~2*10^8 probes against ~2*10^4.
        assert!(
            hostile < benign * 20 + Duration::from_millis(50),
            "hostile {hostile:?} vs benign {benign:?}"
        );
    }

    #[test]
    fn writes_of_every_short_length_differ() {
        let base = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let mut hashes = FastSet::default();
        for len in 0..=base.len() {
            assert!(hashes.insert(hash_bytes(&base[..len])));
            if len > 0 {
                let mut flipped = base[..len].to_vec();
                flipped[len / 2] ^= 1;
                assert!(hashes.insert(hash_bytes(&flipped)));
            }
        }
    }

    #[test]
    fn seed_is_stable_within_the_process() {
        assert_eq!(hash_bytes(b"stable"), hash_bytes(b"stable"));
        let a = FxHasher::default();
        let b = FxHasher::default();
        assert_eq!((a.acc, a.fold), (b.acc, b.fold));
    }
}
