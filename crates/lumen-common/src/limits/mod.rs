//! Resource limits shared by the engines: stopping a run (interrupt flag, deadline flag, the
//! reason a run was cut short), a byte budget over the allocator's counters, checked sizes and
//! fallible reservation, and a cancellable timer for deadlines. Each engine keeps its own limit
//! values and maps these neutral results onto its own exceptions.

#[cfg(not(target_arch = "wasm32"))]
mod deadline;
mod heap;
pub mod size;
mod stop;

#[cfg(not(target_arch = "wasm32"))]
pub use deadline::Deadline;
pub use heap::{HeapBudget, HeapScope};
pub use stop::{Abort, InterruptHandle, StopFlags};
