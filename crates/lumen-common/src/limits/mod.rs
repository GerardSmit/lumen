//! Resource limits shared by the engines: stopping a run (interrupt flag, deadline flag, the
//! reason a run was cut short), a byte budget over the allocator's counters, checked sizes and
//! fallible reservation, a cancellable timer for deadlines, and a hard-capped global allocator for
//! harness processes. Each engine keeps its own limit
//! values and maps these neutral results onto its own exceptions.

mod alloc;
#[cfg(not(target_arch = "wasm32"))]
mod deadline;
mod heap;
pub mod size;
mod stop;

pub use alloc::CappedAlloc;
#[cfg(not(target_arch = "wasm32"))]
pub use deadline::Deadline;
pub use heap::{HeapBudget, HeapScope};
pub use stop::{Abort, InterruptHandle, InterruptSubscription, StopFlags};
