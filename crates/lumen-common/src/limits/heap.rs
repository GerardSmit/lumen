/// Which allocator counter a [`HeapBudget`] measures.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HeapScope {
    /// Bytes the whole process holds through the allocator (every realm and thread).
    #[default]
    Process,
    /// Bytes the calling thread allocated since the budget was set: exact for one interpreter
    /// per thread.
    Thread,
}

/// A cap on allocated bytes, read from the counters of [`crate::fastalloc::ClassAlloc`]. Without
/// that allocator installed a process budget reads nothing (it never trips) and a thread budget
/// reads zero, so it only refuses single requests larger than the cap.
#[derive(Clone, Copy, Debug, Default)]
pub struct HeapBudget {
    limit: usize,
    base: isize,
    scope: HeapScope,
}

impl HeapBudget {
    pub const NONE: HeapBudget = HeapBudget {
        limit: 0,
        base: 0,
        scope: HeapScope::Process,
    };

    /// Caps the process-wide count at `limit` bytes (0 = no cap).
    pub fn process(limit: usize) -> HeapBudget {
        HeapBudget {
            limit,
            base: 0,
            scope: HeapScope::Process,
        }
    }

    /// Caps what the calling thread allocates from now at `limit` bytes (0 = no cap).
    pub fn thread(limit: usize) -> HeapBudget {
        HeapBudget {
            limit,
            base: thread_bytes(),
            scope: HeapScope::Thread,
        }
    }

    pub fn is_set(&self) -> bool {
        self.limit != 0
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Bytes counted against the budget, or `None` when the counter is unavailable.
    pub fn used(&self) -> Option<usize> {
        match self.scope {
            HeapScope::Process => process_bytes(),
            HeapScope::Thread => Some((thread_bytes() - self.base).max(0) as usize),
        }
    }

    /// Whether `extra` more bytes would pass the cap.
    pub fn exceeded_by(&self, extra: usize) -> bool {
        self.limit != 0
            && self
                .used()
                .is_some_and(|used| used.saturating_add(extra) > self.limit)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn process_bytes() -> Option<usize> {
    crate::fastalloc::heap_bytes()
}

#[cfg(target_arch = "wasm32")]
fn process_bytes() -> Option<usize> {
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn thread_bytes() -> isize {
    crate::fastalloc::thread_live_bytes()
}

#[cfg(target_arch = "wasm32")]
fn thread_bytes() -> isize {
    0
}
