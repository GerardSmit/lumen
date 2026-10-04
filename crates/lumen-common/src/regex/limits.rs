//! Interruption of a running match: the embedder's interrupt flag, script deadline and heap
//! budget, polled from the matcher's step check.

use crate::limits::{Abort, HeapBudget, StopFlags};
use std::cell::{Cell, RefCell};

/// A match ran out of its backtracking budget (time or memory) and must fail the caller's
/// operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BacktrackLimit;

pub const BACKTRACK_LIMIT_MSG: &str = "Maximum regular expression backtracking exceeded";

thread_local! {
    static HOST_POLL: RefCell<(StopFlags, HeapBudget)> = const { RefCell::new((StopFlags::new(), HeapBudget::NONE)) };
    static ABORT: Cell<Abort> = const { Cell::new(Abort::None) };
}

/// Install the interrupt and deadline flags and the heap budget for this thread's matcher, which
/// polls them at its step check.
pub fn set_host_poll(stop: StopFlags, heap: HeapBudget) {
    HOST_POLL.with(|p| *p.borrow_mut() = (stop, heap));
}

/// The reason the last failed match was aborted (cleared by the read).
pub fn take_abort() -> Abort {
    ABORT.with(|a| a.replace(Abort::None))
}

pub(super) fn record_abort(abort: Abort) {
    ABORT.with(|a| a.set(abort));
}

/// Check the installed flags and heap budget; `pending` is the matcher's own backtrack memory
/// not yet reflected in the allocator's count.
pub(super) fn poll(pending: usize) -> Abort {
    HOST_POLL.with(|p| {
        let (stop, heap) = &*p.borrow();
        match stop.poll() {
            Abort::None if heap.exceeded_by(pending) => Abort::Heap,
            abort => abort,
        }
    })
}
