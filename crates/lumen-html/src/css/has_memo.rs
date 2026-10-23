//! Per-document memo of `:has()` results. A relative selector scans the
//! anchor's subtree or following siblings, so restyling many elements under
//! one anchor would otherwise repeat that scan for each of them. Entries are
//! valid for one document version only.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};

const MAX_ENTRIES: usize = 1 << 16;

static NEXT_RELATIVE_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_relative_id() -> u64 {
    NEXT_RELATIVE_ID.fetch_add(1, Ordering::Relaxed)
}

pub(crate) type HasKey = (u64, u128, Option<u128>, Option<u128>);

#[derive(Default)]
pub(crate) struct HasMemo {
    version: u64,
    entries: BTreeMap<HasKey, bool>,
}

impl HasMemo {
    fn sync(&mut self, version: u64) {
        if self.version != version {
            self.entries.clear();
            self.version = version;
        }
    }

    pub(crate) fn get(&mut self, version: u64, key: &HasKey) -> Option<bool> {
        self.sync(version);
        self.entries.get(key).copied()
    }

    pub(crate) fn insert(&mut self, version: u64, key: HasKey, found: bool) {
        self.sync(version);
        if self.entries.len() >= MAX_ENTRIES {
            self.entries.clear();
        }
        self.entries.insert(key, found);
    }
}
