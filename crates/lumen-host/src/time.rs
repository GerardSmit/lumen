//! Clocks that work on every target. `std::time::Instant::now()` and `SystemTime::now()` panic
//! on `wasm32-unknown-unknown`; `web-time` re-exports std's types natively and backs them with
//! `performance.now()` / `Date.now()` in a browser, so the runtime crates import from here.

pub use web_time::{Instant, SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, as `Date.now()` reports them.
pub fn unix_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

/// Point the engine's `Date` / `Temporal.Now` at the platform wall clock (a no-op natively,
/// where the engine's `SystemTime` fallback already works).
pub fn install_engine_clock() {
    lumen::set_host_clock(unix_ms);
}
