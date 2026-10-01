//! Collection notifications for `perf_hooks`: while a [`GcObserver`] sits in the host state, each
//! collection is logged (start, duration, whether the program forced it) and the observer's
//! callback is queued as a microtask, so a program that never observes pays one map lookup.

use crate::value::Value;
use std::time::{Duration, Instant};

pub struct GcEvent {
    /// Offset from [`GcObserver::epoch`].
    pub start: Duration,
    pub duration: Duration,
    pub forced: bool,
}

pub struct GcObserver {
    pub callback: Value,
    pub epoch: Instant,
    /// The embedder's clock reading at `epoch`.
    pub epoch_ms: f64,
    pub events: Vec<GcEvent>,
    pub forced_next: bool,
    pub queued: bool,
}

impl GcObserver {
    pub fn new(callback: Value) -> Self {
        GcObserver { callback, epoch: Instant::now(), epoch_ms: 0.0, events: Vec::new(), forced_next: false, queued: false }
    }
}
