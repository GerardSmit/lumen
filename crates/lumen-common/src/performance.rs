//! Bounded, realm-independent Performance Timeline storage. Values belong to the caller;
//! the host adapter traces them and owns the clock and task delivery.
use alloc::{collections::{BTreeMap, VecDeque}, rc::Rc, string::String, vec::Vec};

pub const ENTRY_LIMIT: usize = 65_536;
pub const OBSERVER_LIMIT: usize = 1_024;
pub const OBSERVER_ENTRY_LIMIT: usize = 16_384;
/// Aggregate live queued references, including fanout, across every observer in one store.
pub const OBSERVER_TOTAL_ENTRY_LIMIT: usize = 65_536;

pub struct Entry<T> {
    pub name: String,
    pub kind: String,
    pub start: f64,
    pub value: T,
}

struct Observer<T> {
    owner: T,
    types: Vec<String>,
    queue: VecDeque<Rc<Entry<T>>>,
    pending: bool,
    dropped: u64,
}

pub struct Timeline<T> {
    entries: VecDeque<Rc<Entry<T>>>,
    observers: BTreeMap<u32, Observer<T>>,
    next_observer: u32,
    queued_entries: usize,
}

impl<T> Default for Timeline<T> {
    fn default() -> Self {
        Self { entries: VecDeque::new(), observers: BTreeMap::new(), next_observer: 1, queued_entries: 0 }
    }
}

impl<T: Clone> Timeline<T> {
    pub fn add(&mut self, name: String, kind: String, start: f64, value: T, retain: bool) -> bool {
        let entry = Rc::new(Entry { name, kind, start, value });
        let mut queued = false;
        for observer in self.observers.values_mut() {
            if observer.types.iter().any(|kind| kind == &entry.kind) {
                Self::enqueue(observer, entry.clone(), &mut self.queued_entries);
                queued = true;
            }
        }
        if retain {
            if self.entries.len() == ENTRY_LIMIT { self.entries.pop_front(); }
            self.entries.push_back(entry);
        }
        queued
    }

    fn enqueue(observer: &mut Observer<T>, entry: Rc<Entry<T>>, queued_entries: &mut usize) {
        if observer.queue.len() >= OBSERVER_ENTRY_LIMIT || *queued_entries >= OBSERVER_TOTAL_ENTRY_LIMIT {
            observer.dropped += 1;
        } else {
            observer.queue.push_back(entry);
            *queued_entries += 1;
        }
        observer.pending = true;
    }

    pub fn entries(&self, name: Option<&str>, kind: Option<&str>) -> Vec<T> {
        let mut entries: Vec<_> = self.entries.iter().filter(|entry| {
            name.is_none_or(|name| name == entry.name) && kind.is_none_or(|kind| kind == entry.kind)
        }).collect();
        // Stable ordering retains insertion order for equal timestamps, across entry types.
        entries.sort_by(|a, b| a.start.total_cmp(&b.start));
        entries.into_iter().map(|entry| entry.value.clone()).collect()
    }

    pub fn resolve(&self, name: &str) -> Option<f64> {
        self.entries.iter().rev().find(|entry| entry.kind == "mark" && entry.name == name).map(|entry| entry.start)
    }

    pub fn clear(&mut self, kind: &str, name: Option<&str>) {
        self.entries.retain(|entry| entry.kind != kind || name.is_some_and(|name| entry.name != name));
        // Clearing a burst must release its large ring allocation. Keep small buffers warm
        // and avoid reallocating on ordinary per-name clear calls in active timelines.
        if self.entries.capacity() >= 1_024 && self.entries.len() < self.entries.capacity() / 8 {
            if self.entries.is_empty() {
                self.entries = VecDeque::new();
            } else {
                self.entries.shrink_to(self.entries.len().max(256));
            }
        }
    }

    pub fn observe(&mut self, id: u32, owner: T, types: Vec<String>, replace: bool, buffered: bool) -> Option<u32> {
        if !self.observers.contains_key(&id) && self.observers.len() >= OBSERVER_LIMIT { return None; }
        let id = if id == 0 {
            let id = self.next_observer;
            self.next_observer = self.next_observer.checked_add(1)?;
            id
        } else { id };
        let observer = self.observers.entry(id).or_insert_with(|| Observer {
            owner, types: Vec::new(), queue: VecDeque::new(), pending: false, dropped: 0,
        });
        if replace { observer.types.clear(); }
        for kind in &types {
            if !observer.types.contains(kind) { observer.types.push(kind.clone()); }
        }
        if buffered {
            for entry in &self.entries {
                if types.iter().any(|kind| kind == &entry.kind) {
                    Self::enqueue(observer, entry.clone(), &mut self.queued_entries);
                }
            }
        }
        Some(id)
    }

    pub fn disconnect(&mut self, id: u32) {
        if let Some(observer) = self.observers.remove(&id) {
            self.queued_entries -= observer.queue.len();
        }
    }

    /// An adapter may supply buffered entries from a source with its own retention policy
    /// (Node resource timings). This does not notify any other observer.
    pub fn buffer(&mut self, id: u32, name: String, kind: String, start: f64, value: T) {
        if let Some(observer) = self.observers.get_mut(&id) {
            if observer.types.contains(&kind) {
                Self::enqueue(observer, Rc::new(Entry { name, kind, start, value }), &mut self.queued_entries);
            }
        }
    }

    pub fn pending(&mut self) -> Vec<T> {
        self.observers.values_mut().filter_map(|observer| {
            if !observer.pending { return None; }
            observer.pending = false;
            Some(observer.owner.clone())
        }).collect()
    }

    pub fn take(&mut self, id: u32) -> Vec<T> {
        let Some(observer) = self.observers.get_mut(&id) else { return Vec::new(); };
        observer.pending = false;
        self.queued_entries -= observer.queue.len();
        // VecDeque -> Vec reuses the drained ring's allocation. The observer immediately
        // returns to zero capacity; no 16K-slot empty ring survives a callback or takeRecords.
        let mut entries: Vec<_> = core::mem::take(&mut observer.queue).into();
        entries.sort_by(|a, b| a.start.total_cmp(&b.start));
        entries.into_iter().map(|entry| entry.value.clone()).collect()
    }

    pub fn dropped(&mut self, id: u32) -> u64 {
        self.observers.get_mut(&id).map(|observer| core::mem::take(&mut observer.dropped)).unwrap_or(0)
    }

    pub fn visit(&self, visit: &mut dyn FnMut(&T)) {
        for entry in &self.entries { visit(&entry.value); }
        for observer in self.observers.values() {
            visit(&observer.owner);
            for entry in &observer.queue { visit(&entry.value); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_preserves_queued_entries_and_latest_mark_uses_insertion_order() {
        let mut timeline = Timeline::default();
        let observer = timeline.observe(0, 99, vec!["mark".into()], true, false).unwrap();
        timeline.add("a".into(), "mark".into(), 10.0, 1, true);
        timeline.add("a".into(), "mark".into(), 2.0, 2, true);
        assert_eq!(timeline.resolve("a"), Some(2.0));
        assert_eq!(timeline.entries(None, None), [2, 1]);
        timeline.clear("mark", Some("a"));
        assert_eq!(timeline.resolve("a"), None);
        assert_eq!(timeline.take(observer), [2, 1]);
        timeline.disconnect(observer);
        assert!(timeline.pending().is_empty());
    }

    #[test]
    fn retained_and_observer_buffers_are_bounded() {
        let mut timeline = Timeline::default();
        let observer = timeline.observe(0, 0, vec!["mark".into()], true, false).unwrap();
        for i in 0..ENTRY_LIMIT + 3 { timeline.add("a".into(), "mark".into(), i as f64, i, true); }
        assert_eq!(timeline.entries(None, None).len(), ENTRY_LIMIT);
        assert_eq!(timeline.dropped(observer), (ENTRY_LIMIT + 3 - OBSERVER_ENTRY_LIMIT) as u64);
        assert_eq!(timeline.take(observer).len(), OBSERVER_ENTRY_LIMIT);
        assert_eq!(timeline.dropped(observer), 0);
    }

    #[test]
    fn aggregate_fanout_admission_reclaims_capacity_and_releases_budget() {
        let mut timeline = Timeline::default();
        let observers: Vec<_> = (0..8).map(|owner| {
            timeline.observe(0, owner, vec!["mark".into()], true, false).unwrap()
        }).collect();
        for i in 0..ENTRY_LIMIT {
            timeline.add("burst".into(), "mark".into(), i as f64, i, true);
        }
        assert_eq!(timeline.queued_entries, OBSERVER_TOTAL_ENTRY_LIMIT);
        for &id in &observers {
            assert_eq!(timeline.observers[&id].queue.len(), OBSERVER_TOTAL_ENTRY_LIMIT / 8);
            assert_eq!(timeline.dropped(id), (ENTRY_LIMIT - OBSERVER_TOTAL_ENTRY_LIMIT / 8) as u64);
        }
        timeline.clear("mark", None);
        assert_eq!(timeline.entries.capacity(), 0, "cleared burst releases the retained ring");
        assert_eq!(timeline.queued_entries, OBSERVER_TOTAL_ENTRY_LIMIT, "clear preserves undelivered observer records");

        let first = observers[0];
        let records = timeline.take(first);
        assert_eq!(records.len(), OBSERVER_TOTAL_ENTRY_LIMIT / 8);
        assert_eq!(timeline.observers[&first].queue.capacity(), 0, "drain frees the observer ring");
        drop(records);
        // Re-observing adds no queued references by itself; explicit source buffering consumes
        // one aggregate slot, and disconnect releases that slot along with existing records.
        timeline.observe(first, 0, vec!["mark".into()], true, false).unwrap();
        timeline.buffer(first, "resource-adapter".into(), "mark".into(), 0.0, 9);
        assert_eq!(timeline.queued_entries, OBSERVER_TOTAL_ENTRY_LIMIT - OBSERVER_TOTAL_ENTRY_LIMIT / 8 + 1);
        timeline.disconnect(first);
        for &id in &observers[1..] { timeline.disconnect(id); }
        assert_eq!(timeline.queued_entries, 0);

        timeline.add("fresh".into(), "mark".into(), 0.0, 1, true);
        let buffered = timeline.observe(0, 0, vec!["mark".into()], true, true).unwrap();
        assert_eq!(timeline.queued_entries, 1, "buffered admission uses the reclaimed aggregate budget");
        assert_eq!(timeline.take(buffered), [1]);
        assert_eq!(timeline.queued_entries, 0);
        assert!(timeline.take(buffered).is_empty());
        timeline.disconnect(buffered);
        timeline.disconnect(buffered);
        assert_eq!(timeline.queued_entries, 0, "repeated disconnect cannot underflow admission budget");
    }
}
