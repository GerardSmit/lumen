//! Open-addressed hash index from a key's hash to its entry slot, probed eight control bytes
//! at a time like hashbrown's SwissTable: each control byte holds 7 hash bits of an occupied
//! bucket, so a miss usually costs one load of a word of control bytes and a hit touches a
//! single entry.

const EMPTY: u8 = 0xff;
const DELETED: u8 = 0x80;
const GROUP: usize = 8;
const LSB: u64 = 0x0101_0101_0101_0101;
const MSB: u64 = 0x8080_8080_8080_8080;

#[derive(Default)]
pub(super) struct Index {
    ctrl: Box<[u8]>,
    slots: Box<[u32]>,
}

fn tag(hash: u32) -> u8 {
    (hash >> 25) as u8
}

/// The bytes of `group` equal to `byte`, as their high bits. May report a false match next to
/// a true one, which the caller's key comparison rejects.
fn matching(group: u64, byte: u8) -> u64 {
    let cmp = group ^ (LSB * byte as u64);
    cmp.wrapping_sub(LSB) & !cmp & MSB
}

fn empties(group: u64) -> u64 {
    group & (group << 1) & MSB
}

impl Index {
    /// An index of `buckets` (a power of two, at least `GROUP`) buckets.
    pub(super) fn new(buckets: usize) -> Self {
        Self {
            ctrl: vec![EMPTY; buckets].into_boxed_slice(),
            slots: vec![0; buckets].into_boxed_slice(),
        }
    }

    pub(super) fn buckets(&self) -> usize {
        self.ctrl.len()
    }

    fn group(&self, start: usize) -> u64 {
        u64::from_le_bytes(self.ctrl[start..start + GROUP].try_into().unwrap())
    }

    /// Triangular probing over aligned groups, which visits every group once.
    fn probe(&self, hash: u32) -> impl Iterator<Item = usize> {
        let mask = self.ctrl.len() - 1;
        let mut start = hash as usize & mask & !(GROUP - 1);
        let mut stride = 0;
        std::iter::from_fn(move || {
            let current = start;
            stride += GROUP;
            start = (start + stride) & mask;
            Some(current)
        })
    }

    /// The bucket whose slot satisfies `is_key`, among those holding `hash`'s tag.
    #[inline]
    pub(super) fn find(&self, hash: u32, mut is_key: impl FnMut(u32) -> bool) -> Option<usize> {
        if self.ctrl.is_empty() {
            return None;
        }
        let tag = tag(hash);
        for start in self.probe(hash) {
            let group = self.group(start);
            let mut candidates = matching(group, tag);
            while candidates != 0 {
                let bucket = start + candidates.trailing_zeros() as usize / 8;
                if is_key(self.slots[bucket]) {
                    return Some(bucket);
                }
                candidates &= candidates - 1;
            }
            if empties(group) != 0 {
                return None;
            }
        }
        unreachable!()
    }

    pub(super) fn slot(&self, bucket: usize) -> u32 {
        self.slots[bucket]
    }

    /// Record `slot` under `hash`, which no bucket holds yet. The caller keeps at least one
    /// bucket empty.
    pub(super) fn insert(&mut self, hash: u32, slot: u32) {
        for start in self.probe(hash) {
            let free = self.group(start) & MSB;
            if free != 0 {
                let bucket = start + free.trailing_zeros() as usize / 8;
                self.ctrl[bucket] = tag(hash);
                self.slots[bucket] = slot;
                return;
            }
        }
    }

    pub(super) fn erase(&mut self, bucket: usize) {
        let start = bucket & !(GROUP - 1);
        // A probe stops at the first group with an empty bucket, so none continues past this
        // group if it already has one, and the bucket can become empty rather than deleted.
        self.ctrl[bucket] = if empties(self.group(start)) != 0 {
            EMPTY
        } else {
            DELETED
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colliding_tags_and_deleted_buckets_keep_probes_correct() {
        let mut index = Index::new(16);
        // All share one start group and one tag, so they fill it and spill into the next.
        let hashes: Vec<u32> = (0..12).map(|n| 0xfe00_0000 | n << 4).collect();
        for (slot, &hash) in hashes.iter().enumerate() {
            index.insert(hash & !0xf, slot as u32);
        }
        for slot in 0..12u32 {
            let bucket = index.find(0xfe00_0000, |s| s == slot).unwrap();
            assert_eq!(index.slot(bucket), slot);
        }
        let bucket = index.find(0xfe00_0000, |s| s == 3).unwrap();
        index.erase(bucket);
        assert!(index.find(0xfe00_0000, |s| s == 3).is_none());
        assert!(index.find(0xfe00_0000, |s| s == 11).is_some());
        assert!(index.find(0x0100_0000, |_| true).is_none());
    }
}
