//! Host-neutral Web Storage areas (`localStorage`, `sessionStorage`).
//!
//! A storage area is an ordered list of string pairs per serialized origin with the HTML
//! specification's quota rule: the total size of keys and values of one origin stays within
//! [`QUOTA_BYTES`], counted as two bytes per UTF-16 code unit. Embedders persist
//! `localStorage` by implementing [`StorageBackend`]; [`MemoryBackend`] is the in-memory default.
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

/// Per-origin quota of keys plus values, in bytes of UTF-16.
pub const QUOTA_BYTES: usize = 5 * 1024 * 1024;

/// Origins a [`MemoryBackend`] keeps areas for.
pub const MAX_ORIGINS: usize = 128;

/// A write would exceed the origin's quota.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuotaExceeded;

/// Storage areas keyed by serialized origin. All methods are synchronous; backends that
/// persist are expected to write through.
pub trait StorageBackend: Send {
    fn len(&self, origin: &str) -> usize;
    fn key(&self, origin: &str, index: usize) -> Option<String>;
    fn get_item(&self, origin: &str, key: &str) -> Option<String>;
    /// Whether `key` is stored, without copying its value.
    fn contains(&self, origin: &str, key: &str) -> bool;
    /// Whether `key` is stored with exactly `value`, without copying either.
    fn item_equals(&self, origin: &str, key: &str, value: &str) -> bool;
    fn keys(&self, origin: &str) -> Vec<String>;
    /// Stores `value` and returns the previous value of `key`.
    fn set_item(&mut self, origin: &str, key: &str, value: &str) -> Result<Option<String>, QuotaExceeded>;
    /// Removes `key` and returns its value.
    fn remove_item(&mut self, origin: &str, key: &str) -> Option<String>;
    /// Empties the area and reports whether it held anything.
    fn clear(&mut self, origin: &str) -> bool;
}

pub type SharedBackend = Arc<Mutex<dyn StorageBackend>>;

fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

/// One origin's ordered entries and their accounted size. `index` maps a key to its position in
/// `entries`, so lookups are O(1) while `key(n)` keeps insertion order.
#[derive(Clone, Debug, Default)]
pub struct StorageArea {
    entries: Vec<(String, String)>,
    index: HashMap<String, usize>,
    bytes: usize,
}

impl StorageArea {
    pub fn from_entries(entries: Vec<(String, String)>) -> Self {
        let mut area = Self::default();
        for (key, value) in entries {
            if let Some(&position) = area.index.get(&key) {
                area.bytes -= units(&area.entries[position].1) * 2;
                area.bytes += units(&value) * 2;
                area.entries[position].1 = value;
            } else {
                area.bytes += (units(&key) + units(&value)) * 2;
                area.index.insert(key.clone(), area.entries.len());
                area.entries.push((key, value));
            }
        }
        area
    }

    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn key(&self, index: usize) -> Option<&str> {
        self.entries.get(index).map(|(key, _)| key.as_str())
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.index.get(key).map(|&position| self.entries[position].1.as_str())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    pub fn set(&mut self, key: &str, value: &str) -> Result<Option<String>, QuotaExceeded> {
        let new_value = units(value) * 2;
        if let Some(&position) = self.index.get(key) {
            let entry = &mut self.entries[position];
            let old_value = units(&entry.1) * 2;
            let bytes = self.bytes - old_value + new_value;
            if bytes > QUOTA_BYTES {
                return Err(QuotaExceeded);
            }
            self.bytes = bytes;
            return Ok(Some(std::mem::replace(&mut entry.1, value.to_owned())));
        }
        let bytes = self.bytes + units(key) * 2 + new_value;
        if bytes > QUOTA_BYTES {
            return Err(QuotaExceeded);
        }
        self.bytes = bytes;
        self.index.insert(key.to_owned(), self.entries.len());
        self.entries.push((key.to_owned(), value.to_owned()));
        Ok(None)
    }

    pub fn remove(&mut self, key: &str) -> Option<String> {
        let position = self.index.remove(key)?;
        let (name, value) = self.entries.remove(position);
        self.bytes -= (units(&name) + units(&value)) * 2;
        if position < self.entries.len() {
            for later in self.index.values_mut() {
                if *later > position {
                    *later -= 1;
                }
            }
        }
        Some(value)
    }

    pub fn clear(&mut self) -> bool {
        let had = !self.entries.is_empty();
        self.entries.clear();
        self.index.clear();
        self.bytes = 0;
        had
    }
}

/// Bounded in-memory storage without persistence.
#[derive(Default)]
pub struct MemoryBackend {
    areas: BTreeMap<String, StorageArea>,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn shared() -> SharedBackend {
        Arc::new(Mutex::new(Self::new()))
    }

    pub fn area(&self, origin: &str) -> Option<&StorageArea> {
        self.areas.get(origin)
    }

    /// Seeds an origin's area, for backends that load persisted entries.
    pub fn insert_area(&mut self, origin: &str, area: StorageArea) {
        self.areas.insert(origin.to_owned(), area);
    }
}

impl StorageBackend for MemoryBackend {
    fn len(&self, origin: &str) -> usize {
        self.areas.get(origin).map_or(0, StorageArea::len)
    }

    fn key(&self, origin: &str, index: usize) -> Option<String> {
        self.areas.get(origin)?.key(index).map(str::to_owned)
    }

    fn get_item(&self, origin: &str, key: &str) -> Option<String> {
        self.areas.get(origin)?.get(key).map(str::to_owned)
    }

    fn contains(&self, origin: &str, key: &str) -> bool {
        self.areas.get(origin).is_some_and(|area| area.contains(key))
    }

    fn item_equals(&self, origin: &str, key: &str, value: &str) -> bool {
        self.areas.get(origin).and_then(|area| area.get(key)) == Some(value)
    }

    fn keys(&self, origin: &str) -> Vec<String> {
        self.areas
            .get(origin)
            .map_or_else(Vec::new, |area| area.entries().iter().map(|(key, _)| key.clone()).collect())
    }

    fn set_item(&mut self, origin: &str, key: &str, value: &str) -> Result<Option<String>, QuotaExceeded> {
        if !self.areas.contains_key(origin) && self.areas.len() >= MAX_ORIGINS {
            return Err(QuotaExceeded);
        }
        self.areas.entry(origin.to_owned()).or_default().set(key, value)
    }

    fn remove_item(&mut self, origin: &str, key: &str) -> Option<String> {
        let area = self.areas.get_mut(origin)?;
        let removed = area.remove(key);
        if area.is_empty() {
            self.areas.remove(origin);
        }
        removed
    }

    fn clear(&mut self, origin: &str) -> bool {
        self.areas.remove(origin).is_some_and(|area| !area.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_keep_insertion_order_and_replace_in_place() {
        let mut backend = MemoryBackend::new();
        assert_eq!(backend.set_item("https://a.test", "x", "1"), Ok(None));
        assert_eq!(backend.set_item("https://a.test", "y", "2"), Ok(None));
        assert_eq!(backend.set_item("https://a.test", "x", "3"), Ok(Some("1".into())));
        assert_eq!(backend.keys("https://a.test"), ["x", "y"]);
        assert_eq!(backend.key("https://a.test", 1).as_deref(), Some("y"));
        assert_eq!(backend.get_item("https://a.test", "x").as_deref(), Some("3"));
        assert_eq!(backend.len("https://b.test"), 0);
    }

    #[test]
    fn quota_counts_utf16_and_rejects_without_changing_the_area() {
        let mut area = StorageArea::default();
        let big = "a".repeat(QUOTA_BYTES / 2 - 1);
        assert!(area.set("k", &big).is_ok());
        let before = area.bytes();
        assert_eq!(area.set("other", "v"), Err(QuotaExceeded));
        assert_eq!(area.bytes(), before);
        assert!(area.set("k", "small").is_ok());
        assert!(area.set("other", "v").is_ok());
        assert_eq!(area.remove("k").as_deref(), Some("small"));
        assert_eq!(area.bytes(), ("other".len() + 1) * 2);
        let astral = "\u{1F600}";
        assert!(area.set("e", astral).is_ok());
        assert_eq!(area.bytes(), ("other".len() + 1 + 1 + 2) * 2);
    }

    #[test]
    fn removal_keeps_order_and_lookups_consistent() {
        let mut area = StorageArea::default();
        for key in ["a", "b", "c", "d"] {
            area.set(key, key).unwrap();
        }
        assert_eq!(area.remove("b").as_deref(), Some("b"));
        assert_eq!(area.remove("b"), None);
        assert_eq!(area.key(1), Some("c"));
        assert_eq!(area.get("d"), Some("d"));
        assert!(area.contains("c") && !area.contains("b"));
        area.set("c", "C").unwrap();
        area.set("b", "B").unwrap();
        let keys: Vec<_> = area.entries().iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(keys, ["a", "c", "d", "b"]);
        assert_eq!(area.get("b"), Some("B"));
        let rebuilt = StorageArea::from_entries(area.entries().to_vec());
        assert_eq!(rebuilt.bytes(), area.bytes());
        assert_eq!(rebuilt.get("d"), Some("d"));
    }

    #[test]
    fn a_large_area_is_indexed_without_linear_scans() {
        let mut area = StorageArea::default();
        for index in 0..20_000 {
            area.set(&index.to_string(), "v").unwrap();
        }
        for index in 0..20_000 {
            assert_eq!(area.get(&index.to_string()), Some("v"));
        }
        for index in (0..20_000).rev() {
            assert!(area.remove(&index.to_string()).is_some());
        }
        assert!(area.is_empty());
    }

    #[test]
    fn contains_and_item_equals_borrow_instead_of_copying() {
        let mut backend = MemoryBackend::new();
        backend.set_item("o", "k", "v").unwrap();
        assert!(backend.contains("o", "k") && !backend.contains("o", "x") && !backend.contains("p", "k"));
        assert!(backend.item_equals("o", "k", "v") && !backend.item_equals("o", "k", "w") && !backend.item_equals("o", "x", "v"));
    }

    #[test]
    fn clear_and_remove_report_what_changed_and_drop_empty_origins() {
        let mut backend = MemoryBackend::new();
        backend.set_item("o", "a", "b").unwrap();
        assert_eq!(backend.remove_item("o", "missing"), None);
        assert_eq!(backend.remove_item("o", "a").as_deref(), Some("b"));
        assert!(backend.area("o").is_none());
        assert!(!backend.clear("o"));
        backend.set_item("o", "a", "b").unwrap();
        assert!(backend.clear("o"));
    }

    #[test]
    fn origin_count_is_bounded() {
        let mut backend = MemoryBackend::new();
        for index in 0..MAX_ORIGINS {
            backend.set_item(&format!("https://{index}.test"), "k", "v").unwrap();
        }
        assert_eq!(backend.set_item("https://extra.test", "k", "v"), Err(QuotaExceeded));
        assert!(backend.set_item("https://0.test", "k2", "v").is_ok());
    }
}
