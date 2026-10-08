//! Compact ordered hash table after V8's OrderedHashTable: one insertion-ordered entry array,
//! found through a separate open-addressed index of `u32` slots (see `index`). Deletes leave
//! tombstones that the next resize compacts away.
//!
//! Cursors are logical insertion sequence numbers rather than slots, so a live iterator resumes
//! correctly after compaction or clear without the table tracking it. While the live entries'
//! sequence numbers are consecutive (no deletes, or only the oldest entries deleted), slot `p`
//! is simply `seq_base + p`; once a compaction closes an interior gap, `seqs` records each
//! slot's offset from `seq_base` until a later compaction makes them consecutive again.

use super::dense::DenseIndex;
use super::index::Index;
use super::key_hash;
use crate::builtins::same_value_zero;
use crate::value::{PACK_EMPTY, PackedValue, Value};
use std::cell::Cell;

const MIN_BUCKETS: usize = 8;

/// Entries an index of `buckets` serves: 7/8 of it, so a probe always meets an empty bucket.
fn capacity(buckets: usize) -> usize {
    buckets - buckets / 8
}

fn hash32(key: &Value) -> u32 {
    key_hash(key) as u32
}

pub(super) trait Entry {
    fn new(key: PackedValue, value: Value, hash: u32) -> Self;
    fn key(&self) -> &PackedValue;
    /// A Set has no separate value: its key doubles as one.
    fn value(&self) -> &PackedValue;
    fn set_value(&mut self, value: Value);
    /// The key's hash, or 0 while it sits in the dense integer index.
    fn hash(&self) -> u32;
    fn set_hash(&mut self, hash: u32);
    fn vacate(&mut self);

    fn is_live(&self) -> bool {
        self.key().bits() != PACK_EMPTY
    }
}

pub(super) struct PairEntry {
    key: PackedValue,
    value: PackedValue,
    hash: u32,
}

pub(super) struct KeyEntry {
    key: PackedValue,
    hash: u32,
}

const _: () = assert!(std::mem::size_of::<PairEntry>() == 24);
const _: () = assert!(std::mem::size_of::<KeyEntry>() == 16);

macro_rules! entry_links {
    () => {
        fn key(&self) -> &PackedValue {
            &self.key
        }
        fn hash(&self) -> u32 {
            self.hash
        }
        fn set_hash(&mut self, hash: u32) {
            self.hash = hash;
        }
    };
}

impl Entry for PairEntry {
    entry_links!();
    fn new(key: PackedValue, value: Value, hash: u32) -> Self {
        Self {
            key,
            value: PackedValue::pack(value),
            hash,
        }
    }
    fn value(&self) -> &PackedValue {
        &self.value
    }
    fn set_value(&mut self, value: Value) {
        self.value = PackedValue::pack(value);
    }
    fn vacate(&mut self) {
        self.key = PackedValue::pack(Value::Empty);
        self.value = PackedValue::pack(Value::Undefined);
    }
}

impl Entry for KeyEntry {
    entry_links!();
    fn new(key: PackedValue, _: Value, hash: u32) -> Self {
        Self { key, hash }
    }
    fn value(&self) -> &PackedValue {
        &self.key
    }
    fn set_value(&mut self, _: Value) {}
    fn vacate(&mut self) {
        self.key = PackedValue::pack(Value::Empty);
    }
}

pub(super) struct Table<E> {
    entries: Vec<E>,
    index: Index,
    dense: DenseIndex,
    live: usize,
    // Live entries reachable through `index` rather than `dense`.
    hashed: usize,
    seq_base: u64,
    next_seq: u64,
    // Empty while slot `p` holds sequence number `seq_base + p`, else one offset per slot.
    seqs: Vec<u32>,
    // The last cursor `next` returned and the slot to resume from, so stepping an iterator
    // over sparse sequence numbers needs no search.
    hint: Cell<(u64, usize)>,
}

impl<E> Default for Table<E> {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            index: Index::default(),
            dense: DenseIndex::default(),
            live: 0,
            hashed: 0,
            seq_base: 0,
            next_seq: 0,
            seqs: Vec::new(),
            hint: Cell::new((u64::MAX, 0)),
        }
    }
}

fn canonical_bits(key: &Value) -> u64 {
    match key {
        Value::Num(n) if *n == 0.0 => 0,
        _ => PackedValue::bits_of(key),
    }
}

fn same_key(stored: &PackedValue, bits: u64, key: &Value) -> bool {
    stored.bits() == bits
        || matches!(key, Value::Str(_) | Value::BigInt(_)) && same_value_zero(&stored.get(), key)
}

impl<E: Entry> Table<E> {
    pub(super) fn len(&self) -> usize {
        self.live
    }

    #[cfg(test)]
    pub(super) fn slots(&self) -> usize {
        self.entries.len()
    }

    /// The index bucket holding `key`.
    fn find(&self, key: &Value, hash: u32) -> Option<usize> {
        let bits = canonical_bits(key);
        self.index.find(hash, |slot| {
            let entry = &self.entries[slot as usize];
            entry.hash() == hash && same_key(entry.key(), bits, key)
        })
    }

    /// The slot holding `key`, or the hash computed while looking (if any) for an insert.
    fn locate(&self, key: &Value) -> Result<usize, Option<u32>> {
        if matches!(key, Value::Num(_)) {
            if let Some(slot) = self.dense.lookup(key) {
                return Ok(slot);
            }
        }
        if self.hashed == 0 {
            return Err(None);
        }
        let hash = hash32(key);
        match self.find(key, hash) {
            Some(bucket) => Ok(self.index.slot(bucket) as usize),
            None => Err(Some(hash)),
        }
    }

    pub(super) fn lookup(&self, key: &Value) -> Option<&PackedValue> {
        let slot = self.locate(key).ok()?;
        Some(self.entries[slot].value())
    }

    pub(super) fn contains(&self, key: &Value) -> bool {
        self.locate(key).is_ok()
    }

    #[cfg(feature = "embed")]
    pub(super) fn contains_string(&self,key:&str,hash:u32)->bool {
        self.index.find(hash,|slot| {
            let entry=&self.entries[slot as usize];
            entry.hash()==hash && matches!(&*entry.key().get(),Value::Str(value) if value.as_str()==key)
        }).is_some()
    }

    pub(super) fn insert(&mut self, key: Value, value: Value) {
        debug_assert!(!matches!(key, Value::Empty));
        let hash = match self.locate(&key) {
            Ok(slot) => return self.entries[slot].set_value(value),
            Err(hash) => hash,
        };
        let buckets = self.index.buckets();
        if self.entries.len() == capacity(buckets) {
            self.resize(if buckets == 0 {
                MIN_BUCKETS
            } else if self.live >= capacity(buckets) / 2 {
                buckets * 2
            } else {
                buckets
            });
        }
        if !self.seqs.is_empty() {
            if self.next_seq - self.seq_base > u32::MAX as u64 {
                self.rebase();
            }
            if !self.seqs.is_empty() {
                self.seqs.push((self.next_seq - self.seq_base) as u32);
            }
        }
        let slot = self.entries.len();
        let hash = if let Some(index) = self.dense.candidate(&key, slot) {
            self.dense.insert(index, slot);
            0
        } else {
            let hash = hash.unwrap_or_else(|| hash32(&key));
            self.index.insert(hash, slot as u32);
            self.hashed += 1;
            hash
        };
        self.entries
            .push(E::new(PackedValue::pack(key), value, hash));
        self.next_seq += 1;
        self.live += 1;
    }

    pub(super) fn remove(&mut self, key: &Value) -> bool {
        if let Some(slot) = self.dense.remove(key) {
            self.vacate(slot);
            return true;
        }
        if self.hashed == 0 {
            return false;
        }
        let Some(bucket) = self.find(key, hash32(key)) else {
            return false;
        };
        let slot = self.index.slot(bucket) as usize;
        self.index.erase(bucket);
        self.hashed -= 1;
        self.vacate(slot);
        true
    }

    fn vacate(&mut self, slot: usize) {
        self.entries[slot].vacate();
        self.live -= 1;
        let buckets = self.index.buckets();
        if buckets > MIN_BUCKETS && self.live < capacity(buckets) / 4 {
            self.resize(buckets / 2);
        }
    }

    fn offset(&self, slot: usize) -> u32 {
        if self.seqs.is_empty() {
            slot as u32
        } else {
            self.seqs[slot]
        }
    }

    /// Drop tombstones, keeping each survivor's sequence number, and rebuild the index with
    /// `buckets` buckets.
    fn resize(&mut self, buckets: usize) {
        let capacity = capacity(buckets);
        // Free the old index first so it never coexists with the reallocated entries.
        self.index = Index::default();
        if self.live < self.entries.len() {
            let mut kept = Vec::with_capacity(self.live);
            for (slot, entry) in self.entries.iter().enumerate() {
                if entry.is_live() {
                    kept.push(self.offset(slot));
                }
            }
            self.entries.retain(E::is_live);
            self.seqs = Vec::new();
            match (kept.first(), kept.last()) {
                (Some(&first), Some(&last)) => {
                    self.seq_base += first as u64;
                    let consecutive = (last - first) as usize == kept.len() - 1
                        && self.seq_base + kept.len() as u64 == self.next_seq;
                    if !consecutive {
                        kept.iter_mut().for_each(|seq| *seq -= first);
                        self.seqs = kept;
                    }
                }
                _ => self.seq_base = self.next_seq,
            }
        }
        if capacity > self.entries.capacity() {
            self.entries.reserve_exact(capacity - self.entries.len());
        } else {
            self.entries.shrink_to(capacity);
        }
        if !self.seqs.is_empty() {
            self.seqs.reserve_exact(capacity - self.seqs.len());
        }
        self.index = Index::new(buckets);
        self.dense.clear();
        self.hashed = 0;
        for (slot, entry) in self.entries.iter_mut().enumerate() {
            let key = entry.key().get();
            if let Some(index) = self.dense.candidate(&key, slot) {
                self.dense.insert(index, slot);
                entry.set_hash(0);
                continue;
            }
            // Only a number can have left the dense index, so only numbers lack a stored hash.
            let hash = match *key {
                Value::Num(_) => hash32(&key),
                _ => entry.hash(),
            };
            drop(key);
            self.hashed += 1;
            self.index.insert(hash, slot as u32);
            entry.set_hash(hash);
        }
        self.hint.set((u64::MAX, 0));
    }

    /// Keep sequence offsets within `u32`. Compaction moves `seq_base` up to the oldest entry.
    #[cold]
    fn rebase(&mut self) {
        self.resize(self.index.buckets());
        if !self.seqs.is_empty() && self.next_seq - self.seq_base > u32::MAX as u64 {
            // An entry outlived 2^32 later insertions. Renumber densely: a cursor parked inside
            // the table may then revisit or skip entries, but nothing else changes.
            self.seq_base = self.next_seq - self.entries.len() as u64;
            self.seqs = Vec::new();
        }
    }

    /// The first slot whose entry was inserted at or after `cursor`.
    #[inline]
    fn position(&self, cursor: u64) -> usize {
        let len = self.entries.len();
        if cursor <= self.seq_base {
            return 0;
        }
        let target = cursor - self.seq_base;
        if self.seqs.is_empty() {
            return target.min(len as u64) as usize;
        }
        let (hint, slot) = self.hint.get();
        if hint == cursor {
            return slot;
        }
        // Offsets rise by at least one per slot, so the answer is at most `target`.
        let end = target.min(len as u64) as usize;
        self.seqs[..end].partition_point(|&seq| (seq as u64) < target)
    }

    #[inline]
    pub(super) fn next(&self, cursor: &mut usize) -> Option<(&PackedValue, &PackedValue)> {
        let start = self.position(*cursor as u64);
        let Some(found) = self.entries[start..].iter().position(Entry::is_live) else {
            *cursor = (*cursor).max(self.next_seq as usize);
            return None;
        };
        let slot = start + found;
        let next = if self.seqs.is_empty() {
            self.seq_base + slot as u64 + 1
        } else {
            let next = self.seq_base + self.seqs[slot] as u64 + 1;
            self.hint.set((next, slot + 1));
            next
        };
        *cursor = next as usize;
        let entry = &self.entries[slot];
        Some((entry.key(), entry.value()))
    }

    pub(super) fn iter(&self) -> Iter<'_, E> {
        Iter(self.entries.iter())
    }

    /// Release every entry. Cursors from before the clear fall below `seq_base` and so resume
    /// at whatever is inserted afterwards.
    pub(super) fn clear(&mut self) {
        let next_seq = self.next_seq;
        *self = Self {
            seq_base: next_seq,
            next_seq,
            ..Self::default()
        };
    }
}

pub(super) struct Iter<'a, E>(std::slice::Iter<'a, E>);

impl<'a, E: Entry> Iterator for Iter<'a, E> {
    type Item = (&'a PackedValue, &'a PackedValue);
    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.0.find(|entry| entry.is_live())?;
        Some((entry.key(), entry.value()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain(table: &Table<PairEntry>, cursor: &mut usize) -> Vec<f64> {
        std::iter::from_fn(|| table.next(cursor))
            .map(|(_, value)| match value.unpack() {
                Value::Num(n) => n,
                _ => f64::NAN,
            })
            .collect()
    }

    #[test]
    fn cursors_survive_compaction_and_sequence_rebase() {
        let mut table = Table::<PairEntry>::default();
        for n in 0..64 {
            table.insert(Value::Num(n as f64 + 0.5), Value::Num(n as f64));
        }
        let mut cursor = 0;
        for _ in 0..10 {
            table.next(&mut cursor);
        }
        for n in 0..60 {
            if n != 10 && n != 40 {
                assert!(table.remove(&Value::Num(n as f64 + 0.5)));
            }
        }
        assert!(table.slots() < 64);
        assert!(!table.seqs.is_empty());
        assert_eq!(
            drain(&table, &mut cursor),
            [10.0, 40.0, 60.0, 61.0, 62.0, 63.0]
        );

        let mut cursor = 0;
        table.next(&mut cursor);
        table.next_seq = table.seq_base + u32::MAX as u64 + 1;
        assert!(table.remove(&Value::Num(10.5)));
        table.insert(Value::str("late"), Value::Num(99.0));
        assert_eq!(table.seq_base, 40);
        assert_eq!(
            drain(&table, &mut cursor),
            [40.0, 60.0, 61.0, 62.0, 63.0, 99.0]
        );
    }

    #[test]
    fn deleting_the_oldest_entries_keeps_sequence_numbers_implicit() {
        let mut table = Table::<PairEntry>::default();
        let mut cursor = 0;
        for n in 0..10_000 {
            table.insert(Value::Num(n as f64), Value::Num(n as f64));
            if n >= 100 {
                assert!(table.remove(&Value::Num((n - 100) as f64)));
            }
            if n == 5_000 {
                assert_eq!(drain(&table, &mut cursor).len(), 100);
            }
        }
        assert!(table.seqs.is_empty());
        assert!(table.slots() <= 256);
        let tail: Vec<f64> = (9_900..10_000).map(f64::from).collect();
        assert_eq!(drain(&table, &mut cursor), tail);
    }

    #[test]
    fn sets_store_keys_only_and_shrink_after_deletes() {
        let mut table = Table::<KeyEntry>::default();
        for n in 0..1000 {
            table.insert(Value::str(&format!("k{n}")), Value::Undefined);
        }
        assert!(
            matches!(table.lookup(&Value::str("k7")).unwrap().unpack(), Value::Str(s) if &*s == "k7")
        );
        for n in 0..990 {
            assert!(table.remove(&Value::str(&format!("k{n}"))));
        }
        assert_eq!(table.len(), 10);
        assert!(table.index.buckets() <= 64);
        assert!(table.contains(&Value::str("k995")));
        assert!(!table.contains(&Value::str("k5")));
    }
}
