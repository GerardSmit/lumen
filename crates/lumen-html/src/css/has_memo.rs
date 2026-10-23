//! Per-document memo of `:has()` results. A relative selector scans the
//! anchor's subtree or following siblings, so restyling many elements under
//! one anchor would otherwise repeat that scan for each of them. Entries are
//! valid for one document version only, and remember the selector work the
//! evaluation cost so a hit can charge the same quota.

use alloc::collections::BTreeMap;
use core::sync::atomic::{AtomicU64, Ordering};

const MAX_ENTRIES: usize = 1 << 14;

static NEXT_RELATIVE_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_relative_id() -> u64 {
    NEXT_RELATIVE_ID.fetch_add(1, Ordering::Relaxed)
}

pub(crate) type HasKey = (u64, u128, Option<u128>, Option<u128>);

#[derive(Clone, Copy)]
pub(crate) struct HasResult {
    pub(crate) found: bool,
    pub(crate) work: usize,
}

#[derive(Default)]
pub(crate) struct HasMemo {
    version: u64,
    entries: BTreeMap<HasKey, HasResult>,
}

impl HasMemo {
    pub(crate) fn get(&self, version: u64, key: &HasKey) -> Option<HasResult> {
        if self.version != version {
            return None;
        }
        self.entries.get(key).copied()
    }

    pub(crate) fn insert(&mut self, version: u64, key: HasKey, result: HasResult) {
        if self.version != version || self.entries.len() >= MAX_ENTRIES {
            self.entries.clear();
            self.version = version;
        }
        self.entries.insert(key, result);
    }
}
