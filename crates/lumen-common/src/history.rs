//! Line-editor history: an ordered list of entered lines with an optional size limit and the
//! text file format (one entry per line). Language-neutral; used by Python's `readline`.

#[derive(Debug, Default, Clone)]
pub struct History {
    items: Vec<String>,
    max: Option<usize>,
}

impl History {
    pub fn new() -> History {
        History::default()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Appends `line`, dropping the oldest entries beyond the limit.
    pub fn add(&mut self, line: &str) {
        self.items.push(line.to_string());
        self.enforce_limit();
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// The entry at 1-based `index`, as GNU readline numbers them.
    pub fn get(&self, index: i64) -> Option<&str> {
        let i = usize::try_from(index.checked_sub(1)?).ok()?;
        self.items.get(i).map(String::as_str)
    }

    /// Removes the entry at 0-based `index`.
    pub fn remove(&mut self, index: usize) -> Option<String> {
        (index < self.items.len()).then(|| self.items.remove(index))
    }

    /// Replaces the entry at 0-based `index`; `false` when there is none.
    pub fn replace(&mut self, index: usize, line: &str) -> bool {
        match self.items.get_mut(index) {
            Some(slot) => {
                *slot = line.to_string();
                true
            }
            None => false,
        }
    }

    /// The size limit; a negative value means unlimited.
    pub fn set_max_length(&mut self, n: i64) {
        self.max = usize::try_from(n).ok();
        self.enforce_limit();
    }

    /// The size limit, -1 when unlimited.
    pub fn max_length(&self) -> i64 {
        self.max.map_or(-1, |n| n as i64)
    }

    fn enforce_limit(&mut self) {
        if let Some(max) = self.max {
            if self.items.len() > max {
                let excess = self.items.len() - max;
                self.items.drain(..excess);
            }
        }
    }

    /// Adds every line of `text`.
    pub fn load(&mut self, text: &str) {
        for line in text.lines() {
            self.add(line);
        }
    }

    /// The last `n` entries (`None`: all), one per line, as written to a history file.
    pub fn serialize(&self, n: Option<usize>) -> String {
        let start = n.map_or(0, |n| self.items.len().saturating_sub(n));
        let mut out = String::new();
        for line in &self.items[start..] {
            out.push_str(line);
            out.push('\n');
        }
        out
    }
}

/// Keeps the last `max` lines of history-file `text` (all when `max` is negative).
pub fn truncate_file_text(text: &str, max: i64) -> String {
    let Ok(max) = usize::try_from(max) else { return text.to_string() };
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    for line in &lines[lines.len().saturating_sub(max)..] {
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_drops_oldest_entries() {
        let mut h = History::new();
        h.set_max_length(2);
        for l in ["a", "b", "c"] {
            h.add(l);
        }
        assert_eq!(h.len(), 2);
        assert_eq!(h.get(1), Some("b"));
        assert_eq!(h.get(3), None);
        assert_eq!(h.get(0), None);
    }

    #[test]
    fn round_trips_through_text() {
        let mut h = History::new();
        h.load("x\ny\nz\n");
        assert_eq!(h.serialize(None), "x\ny\nz\n");
        assert_eq!(h.serialize(Some(2)), "y\nz\n");
        assert_eq!(truncate_file_text("x\ny\nz\n", 1), "z\n");
        assert_eq!(truncate_file_text("x\ny\n", -1), "x\ny\n");
    }

    #[test]
    fn remove_and_replace_use_zero_based_positions() {
        let mut h = History::new();
        h.load("a\nb\n");
        assert!(h.replace(1, "B"));
        assert_eq!(h.remove(0).as_deref(), Some("a"));
        assert_eq!(h.get(1), Some("B"));
        assert!(!h.replace(5, "q"));
    }
}
