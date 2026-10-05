//! The process clock behind `performance.now()` / `timeOrigin` and the milestones and loop
//! counters behind `performance.nodeTiming`. The clock is process-wide (workers share the
//! main thread's origin, as in Node); milestones and loop counters belong to the thread whose
//! event loop they describe.

use crate::time::Instant;
use std::cell::Cell;
use std::sync::OnceLock;
use std::time::Duration;

static ORIGIN: OnceLock<(Instant, f64)> = OnceLock::new();

fn origin() -> &'static (Instant, f64) {
    ORIGIN.get_or_init(|| (Instant::now(), crate::time::unix_ms()))
}

/// Start the clock now if nothing has read it yet.
pub fn start_clock() {
    origin();
}

pub fn now_ms() -> f64 {
    ms_since_origin(Instant::now())
}

/// The shared web clock uses 100-microsecond buckets. Integer quantization
/// keeps exposed event and performance timestamps monotonic and on the same
/// grid, without changing the internal clock used for deadlines and timings.
pub fn web_now_ms() -> f64 {
    let elapsed = Instant::now().saturating_duration_since(origin().0);
    (elapsed.as_micros() / 100) as f64 / 10.0
}

pub fn ms_since_origin(at: Instant) -> f64 {
    at.saturating_duration_since(origin().0).as_secs_f64() * 1000.0
}

/// Unix-epoch milliseconds at the clock's zero point.
pub fn time_origin_ms() -> f64 {
    origin().1
}

#[derive(Clone, Copy)]
pub enum Milestone {
    NodeStart = 0,
    V8Start = 1,
    Environment = 2,
    BootstrapComplete = 3,
    LoopStart = 4,
    LoopExit = 5,
}

thread_local! {
    static MILESTONES: Cell<[f64; 6]> = const { Cell::new([-1.0; 6]) };
    static IDLE_MS: Cell<f64> = const { Cell::new(0.0) };
}

/// Record `milestone` as reached now; an already-recorded one keeps its time.
pub fn mark(milestone: Milestone) {
    let now = now_ms();
    MILESTONES.with(|m| {
        let mut all = m.get();
        if all[milestone as usize] < 0.0 {
            all[milestone as usize] = now;
            m.set(all);
        }
    });
}

/// The loop ended: the latest exit wins, since `beforeExit` listeners can restart it.
pub fn mark_loop_exit() {
    let now = now_ms();
    MILESTONES.with(|m| {
        let mut all = m.get();
        all[Milestone::LoopExit as usize] = now;
        m.set(all);
    });
}

pub fn add_idle(blocked: Duration) {
    IDLE_MS.with(|idle| idle.set(idle.get() + blocked.as_secs_f64() * 1000.0));
}

/// `[nodeStart, v8Start, environment, bootstrapComplete, loopStart, loopExit, idleTime]`, with -1
/// for a milestone not reached yet.
pub fn snapshot() -> [f64; 7] {
    let m = MILESTONES.with(Cell::get);
    [m[0], m[1], m[2], m[3], m[4], m[5], IDLE_MS.with(Cell::get)]
}
