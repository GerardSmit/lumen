//! A bounded two-generation cache: lookups promote from the old generation, and a full young
//! generation becomes the old one, dropping the previous old generation. Memory stays below
//! `2 * N` entries and the entries in use survive.

use alloc::collections::BTreeMap;

pub struct Generations<K, V, const N: usize> {
    young: BTreeMap<K, V>,
    old: BTreeMap<K, V>,
}

impl<K, V, const N: usize> Default for Generations<K, V, N> {
    fn default() -> Self {
        Self { young: BTreeMap::new(), old: BTreeMap::new() }
    }
}

impl<K: Ord + Copy, V: Clone, const N: usize> Generations<K, V, N> {
    pub fn get(&mut self, key: &K) -> Option<V> {
        if let Some(value) = self.young.get(key) {
            return Some(value.clone());
        }
        let value = self.old.remove(key)?;
        self.insert(*key, value.clone());
        Some(value)
    }

    pub fn insert(&mut self, key: K, value: V) {
        if self.young.len() >= N && !self.young.contains_key(&key) {
            self.old = core::mem::take(&mut self.young);
        }
        self.young.insert(key, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_recent_entries() {
        let mut cache: Generations<u32, u32, 2> = Generations::default();
        cache.insert(1, 1);
        cache.insert(2, 2);
        cache.insert(3, 3);
        assert_eq!(cache.get(&1), Some(1));
        cache.insert(4, 4);
        cache.insert(5, 5);
        assert_eq!(cache.get(&2), None);
    }
}
