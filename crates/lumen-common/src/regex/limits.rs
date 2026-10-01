//! Interruption of a running match: the embedder's interrupt flag, script deadline and heap
//! limit, polled from the matcher's step check, and the reason a match was cut short.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::Arc;

/// A match ran out of its backtracking budget (time or memory) and must fail the caller's
/// operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BacktrackLimit;

pub const BACKTRACK_LIMIT_MSG: &str = "Maximum regular expression backtracking exceeded";

/// Why a match was cut short beyond its own budget; read back by the caller that reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abort {
    None,
    Interrupt,
    Deadline,
    Heap,
}

#[derive(Default)]
struct HostPoll {
    interrupt: Option<Arc<AtomicBool>>,
    deadline: Option<Arc<AtomicBool>>,
    heap_limit: usize,
}

thread_local! {
    static HOST_POLL: RefCell<HostPoll> = RefCell::new(HostPoll::default());
    static ABORT: Cell<Abort> = const { Cell::new(Abort::None) };
}

/// Install the interrupt flag, script-deadline flag and heap limit (bytes, 0 = none) for this
/// thread's matcher, which polls them at its step check.
pub fn set_host_poll(
    interrupt: Option<Arc<AtomicBool>>,
    deadline: Option<Arc<AtomicBool>>,
    heap_limit: usize,
) {
    HOST_POLL.with(|p| {
        *p.borrow_mut() = HostPoll {
            interrupt,
            deadline,
            heap_limit,
        };
    });
}

/// The reason the last failed match was aborted (cleared by the read).
pub fn take_abort() -> Abort {
    ABORT.with(|a| a.replace(Abort::None))
}

pub(super) fn record_abort(abort: Abort) {
    ABORT.with(|a| a.set(abort));
}

/// Check the installed flags and heap limit; `pending` is the matcher's own backtrack memory
/// not yet reflected in the allocator's count.
pub(super) fn poll(pending: usize) -> Abort {
    HOST_POLL.with(|p| {
        let p = p.borrow();
        if p.interrupt.as_ref().is_some_and(|f| f.load(Relaxed)) {
            return Abort::Interrupt;
        }
        if p.deadline.as_ref().is_some_and(|f| f.load(Relaxed)) {
            return Abort::Deadline;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if p.heap_limit != 0 {
            if let Some(used) = crate::fastalloc::heap_bytes() {
                if used.saturating_add(pending) > p.heap_limit {
                    return Abort::Heap;
                }
            }
        }
        #[cfg(target_arch = "wasm32")]
        let _ = pending;
        Abort::None
    })
}
