//! A least-recently-used map bounded by the summed byte size of its values.
use alloc::collections::BTreeMap;
use std::collections::HashMap;
use std::hash::Hash;

struct Entry<V> {
    value: V,
    bytes: usize,
    tick: u64,
}

pub struct ByteLru<K, V> {
    entries: HashMap<K, Entry<V>>,
    order: BTreeMap<u64, K>,
    tick: u64,
    bytes: usize,
    budget: usize,
}

impl<K: Hash + Eq + Copy, V: Clone> ByteLru<K, V> {
    pub fn new(budget: usize) -> Self {
        Self { entries: HashMap::new(), order: BTreeMap::new(), tick: 0, bytes: 0, budget }
    }

    pub fn budget(&self) -> usize {
        self.budget
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
        self.evict(None);
    }

    pub fn get(&mut self, key: &K) -> Option<V> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(key)?;
        self.order.remove(&entry.tick);
        entry.tick = tick;
        self.order.insert(tick, *key);
        Some(entry.value.clone())
    }

    /// Inserts `value`, then evicts least recently used entries until the budget holds.
    /// The entry just inserted always stays, even when it alone exceeds the budget.
    pub fn insert(&mut self, key: K, value: V, bytes: usize) {
        self.remove(&key);
        let tick = self.next_tick();
        self.order.insert(tick, key);
        self.entries.insert(key, Entry { value, bytes, tick });
        self.bytes += bytes;
        self.evict(Some(key));
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let entry = self.entries.remove(key)?;
        self.order.remove(&entry.tick);
        self.bytes -= entry.bytes;
        Some(entry.value)
    }

    fn next_tick(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    fn evict(&mut self, keep: Option<K>) {
        while self.bytes > self.budget {
            let Some((&tick, &key)) = self.order.iter().find(|(_, key)| Some(**key) != keep) else { break };
            self.order.remove(&tick);
            if let Some(entry) = self.entries.remove(&key) {
                self.bytes -= entry.bytes;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evicts_least_recently_used_over_budget() {
        let mut cache = ByteLru::new(10);
        cache.insert(1u32, "a", 4);
        cache.insert(2, "b", 4);
        assert_eq!(cache.get(&1), Some("a"));
        cache.insert(3, "c", 4);
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&1), Some("a"));
        assert_eq!(cache.get(&3), Some("c"));
        assert_eq!(cache.bytes(), 8);
    }

    #[test]
    fn oversized_newest_entry_is_kept_and_replaces_the_rest() {
        let mut cache = ByteLru::new(10);
        cache.insert(1u32, "a", 4);
        cache.insert(2, "huge", 50);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&2), Some("huge"));
        cache.insert(3, "b", 2);
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&3), Some("b"));
    }

    #[test]
    fn reinsert_and_remove_track_bytes_and_shrinking_the_budget_evicts() {
        let mut cache = ByteLru::new(100);
        cache.insert(1u32, "a", 30);
        cache.insert(1, "a2", 10);
        assert_eq!(cache.bytes(), 10);
        cache.insert(2, "b", 30);
        cache.set_budget(30);
        assert_eq!(cache.get(&1), None);
        assert_eq!(cache.remove(&2), Some("b"));
        assert_eq!(cache.bytes(), 0);
    }
}
