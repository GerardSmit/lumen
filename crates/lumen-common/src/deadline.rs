//! A min-heap of one-shot or re-armable deadlines with lazy cancellation.
//!
//! Removing an entry is O(1): its heap node stays behind as a stale node and is skipped (and
//! popped) when it surfaces. A node is live only while its deadline equals the entry's current
//! deadline, so re-arming an entry (`rearm`) also just pushes a new node. Stale nodes are bounded
//! by [`DeadlineQueue::compact`], and an emptied queue gives its capacity back. The queue never
//! reads a clock: callers pass `now`, so it works for any instant type and stays OS-free.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

pub struct DeadlineQueue<I: Ord + Copy, T> {
    next_id: u64,
    heap: BinaryHeap<Reverse<(I, u64)>>,
    entries: HashMap<u64, (I, T)>,
}

impl<I: Ord + Copy, T> Default for DeadlineQueue<I, T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I: Ord + Copy, T> DeadlineQueue<I, T> {
    pub fn new() -> Self {
        Self { next_id: 0, heap: BinaryHeap::new(), entries: HashMap::new() }
    }

    /// Live entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Adds an entry and returns its id; ids start at 1 and are never reused.
    pub fn insert(&mut self, deadline: I, value: T) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.entries.insert(id, (deadline, value));
        self.heap.push(Reverse((deadline, id)));
        id
    }

    pub fn get(&self, id: u64) -> Option<&T> {
        self.entries.get(&id).map(|(_, value)| value)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut T> {
        self.entries.get_mut(&id).map(|(_, value)| value)
    }

    pub fn deadline(&self, id: u64) -> Option<I> {
        self.entries.get(&id).map(|(deadline, _)| *deadline)
    }

    pub fn values(&self) -> impl Iterator<Item = &T> {
        self.entries.values().map(|(_, value)| value)
    }

    /// Cancels an entry lazily; its heap node is skipped when it surfaces.
    pub fn remove(&mut self, id: u64) -> Option<T> {
        self.entries.remove(&id).map(|(_, value)| value)
    }

    /// Moves a live entry to a new deadline. False when the id is not live.
    pub fn rearm(&mut self, id: u64, deadline: I) -> bool {
        let Some(entry) = self.entries.get_mut(&id) else {
            return false;
        };
        entry.0 = deadline;
        self.heap.push(Reverse((deadline, id)));
        true
    }

    /// Keeps the entries `keep` accepts. Stale heap nodes are left for `compact`.
    pub fn retain(&mut self, mut keep: impl FnMut(u64, &mut T) -> bool) {
        self.entries.retain(|id, (_, value)| keep(*id, value));
    }

    fn is_live(&self, id: u64, deadline: I) -> bool {
        self.entries.get(&id).is_some_and(|(live, _)| *live == deadline)
    }

    /// The earliest live deadline. Pops cancelled and stale nodes on the way, so a removed or
    /// re-armed entry cannot cause a busy wakeup.
    pub fn next_deadline(&mut self) -> Option<I> {
        while let Some(Reverse((deadline, id))) = self.heap.peek().copied() {
            if self.is_live(id, deadline) {
                return Some(deadline);
            }
            self.heap.pop();
        }
        None
    }

    /// Pops the earliest live node due at `now` and returns its id and deadline. The entry stays
    /// in the queue: the caller then `rearm`s or `remove`s it (an entry that is neither has no
    /// heap node and never fires again).
    pub fn pop_due(&mut self, now: I) -> Option<(u64, I)> {
        while let Some(Reverse((deadline, id))) = self.heap.peek().copied() {
            if deadline > now {
                return None;
            }
            self.heap.pop();
            if self.is_live(id, deadline) {
                return Some((id, deadline));
            }
        }
        None
    }

    /// Removes and returns the earliest entry due at `now`.
    pub fn take_due(&mut self, now: I) -> Option<(I, T)> {
        let (id, deadline) = self.pop_due(now)?;
        self.entries.remove(&id).map(|(_, value)| (deadline, value))
    }

    /// Bounds stale nodes left by lazy cancellation so remove-heavy callers cannot grow the heap
    /// without limit, then releases spare capacity.
    pub fn compact(&mut self) {
        if self.heap.len() > self.entries.len().saturating_mul(2) + 32 {
            let entries = &self.entries;
            self.heap
                .retain(|Reverse((deadline, id))| {
                    entries.get(id).is_some_and(|(live, _)| live == deadline)
                });
            self.release_idle_capacity();
        }
    }

    /// An empty queue drops its heap and table when they grew large; a sparse one shrinks.
    pub fn release_idle_capacity(&mut self) {
        if self.entries.is_empty() {
            self.heap.clear();
            if self.heap.capacity() > 64 {
                self.heap = BinaryHeap::new();
            }
            if self.entries.capacity() > 64 {
                self.entries = HashMap::new();
            }
        } else {
            if self.heap.capacity() > 256 && self.heap.len() * 4 < self.heap.capacity() {
                self.heap.shrink_to(self.heap.len().max(64) * 2);
            }
            if self.entries.capacity() > 256 && self.entries.len() * 4 < self.entries.capacity() {
                self.entries.shrink_to(self.entries.len().max(64) * 2);
            }
        }
    }

    #[cfg(test)]
    fn heap_len(&self) -> usize {
        self.heap.len()
    }

    #[cfg(test)]
    fn heap_capacity(&self) -> usize {
        self.heap.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_due_entries_in_deadline_order() {
        let mut q = DeadlineQueue::new();
        q.insert(30u64, "c");
        q.insert(10, "a");
        q.insert(20, "b");
        assert_eq!(q.next_deadline(), Some(10));
        assert_eq!(q.take_due(25), Some((10, "a")));
        assert_eq!(q.take_due(25), Some((20, "b")));
        assert_eq!(q.take_due(25), None);
        assert_eq!(q.take_due(30), Some((30, "c")));
        assert!(q.is_empty());
    }

    #[test]
    fn equal_deadlines_fire_in_insertion_order() {
        let mut q = DeadlineQueue::new();
        q.insert(5u64, 1);
        q.insert(5, 2);
        q.insert(5, 3);
        let order: Vec<_> = std::iter::from_fn(|| q.take_due(5)).map(|(_, v)| v).collect();
        assert_eq!(order, [1, 2, 3]);
    }

    #[test]
    fn removed_entries_are_skipped() {
        let mut q = DeadlineQueue::new();
        let a = q.insert(1u64, "a");
        q.insert(2, "b");
        assert_eq!(q.remove(a), Some("a"));
        assert_eq!(q.remove(a), None);
        assert_eq!(q.next_deadline(), Some(2));
        assert_eq!(q.take_due(10), Some((2, "b")));
        assert_eq!(q.next_deadline(), None);
    }

    #[test]
    fn nothing_is_due_before_its_deadline() {
        let mut q = DeadlineQueue::new();
        q.insert(10u64, ());
        assert_eq!(q.pop_due(9), None);
        assert_eq!(q.len(), 1);
        assert_eq!(q.next_deadline(), Some(10));
    }

    #[test]
    fn rearm_supersedes_the_old_node() {
        let mut q = DeadlineQueue::new();
        let id = q.insert(10u64, "x");
        assert!(q.rearm(id, 50));
        assert_eq!(q.deadline(id), Some(50));
        assert_eq!(q.next_deadline(), Some(50));
        assert_eq!(q.take_due(20), None);
        assert_eq!(q.take_due(50), Some((50, "x")));
        assert!(!q.rearm(id, 60));
    }

    #[test]
    fn pop_due_leaves_the_entry_for_the_caller_to_rearm() {
        let mut q = DeadlineQueue::new();
        let id = q.insert(10u64, "tick");
        assert_eq!(q.pop_due(10), Some((id, 10)));
        assert_eq!(q.get(id), Some(&"tick"));
        assert_eq!(q.pop_due(10), None);
        assert!(q.rearm(id, 20));
        assert_eq!(q.pop_due(20), Some((id, 20)));
        assert_eq!(q.remove(id), Some("tick"));
    }

    #[test]
    fn ids_are_not_reused() {
        let mut q = DeadlineQueue::new();
        let a = q.insert(1u64, ());
        q.remove(a);
        let b = q.insert(1u64, ());
        assert_ne!(a, b);
        assert_eq!(a, 1);
    }

    #[test]
    fn retain_filters_entries_and_their_nodes_go_stale() {
        let mut q = DeadlineQueue::new();
        for n in 0..6u64 {
            q.insert(n, n);
        }
        q.retain(|_, v| *v % 2 == 0);
        assert_eq!(q.len(), 3);
        let order: Vec<_> = std::iter::from_fn(|| q.take_due(100)).map(|(_, v)| v).collect();
        assert_eq!(order, [0, 2, 4]);
    }

    #[test]
    fn compact_bounds_stale_nodes() {
        let mut q = DeadlineQueue::new();
        let keep = q.insert(u64::MAX, ());
        let ids: Vec<_> = (0..500u64).map(|n| q.insert(n, ())).collect();
        for id in ids {
            q.remove(id);
        }
        assert!(q.heap_len() > 400);
        q.compact();
        assert_eq!(q.heap_len(), 1);
        assert_eq!(q.next_deadline(), Some(u64::MAX));
        assert!(q.get(keep).is_some());
    }

    #[test]
    fn idle_queue_releases_capacity() {
        let mut q = DeadlineQueue::new();
        let ids: Vec<_> = (0..1000u64).map(|n| q.insert(n, ())).collect();
        assert!(q.heap_capacity() > 64);
        for id in ids {
            q.remove(id);
        }
        q.release_idle_capacity();
        assert!(q.is_empty());
        assert_eq!(q.heap_len(), 0);
        assert!(q.heap_capacity() <= 64);
    }
}
