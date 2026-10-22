//! Lazily filled per-index cache for values computed once from a fixed
//! registry position (`no_std`, lock-free). Each slot holds `0` while unset
//! and `value + 1` once stored.

use core::sync::atomic::{AtomicU16, Ordering};

pub struct LazyIndexTable<const N: usize> {
    slots: [AtomicU16; N],
}

impl<const N: usize> LazyIndexTable<N> {
    #[allow(clippy::declare_interior_mutable_const)]
    const UNSET: AtomicU16 = AtomicU16::new(0);

    pub const fn new() -> Self {
        Self { slots: [Self::UNSET; N] }
    }

    /// Stored value for `index`, or `None` when unset or out of range.
    pub fn get(&self, index: usize) -> Option<u16> {
        let held = self.slots.get(index)?.load(Ordering::Relaxed);
        held.checked_sub(1)
    }

    /// Stores `value` (below `u16::MAX`); out-of-range indices are ignored.
    pub fn set(&self, index: usize, value: u16) {
        debug_assert!(value < u16::MAX);
        if let Some(slot) = self.slots.get(index) {
            slot.store(value + 1, Ordering::Relaxed);
        }
    }

    /// Cached value for `index`, computing and storing it on first use.
    pub fn get_or_init(&self, index: usize, init: impl FnOnce() -> u16) -> u16 {
        if let Some(value) = self.get(index) {
            return value;
        }
        let value = init();
        self.set(index, value);
        value
    }
}

impl<const N: usize> Default for LazyIndexTable<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_once_and_ignores_out_of_range() {
        static TABLE: LazyIndexTable<4> = LazyIndexTable::new();
        assert_eq!(TABLE.get(1), None);
        assert_eq!(TABLE.get_or_init(1, || 0), 0);
        assert_eq!(TABLE.get_or_init(1, || 9), 0);
        assert_eq!(TABLE.get(1), Some(0));
        TABLE.set(9, 3);
        assert_eq!(TABLE.get(9), None);
    }
}
