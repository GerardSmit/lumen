//! Native stack bounds. Recursion guards count frames (`MAX_EVAL_DEPTH`) sized for the large
//! stacks the CLI and workers run on; this adds a byte-based guard so an engine on a small stack
//! (a default 2 MiB `std::thread`) throws a RangeError instead of overflowing. Like V8's
//! `StackGuard` / QuickJS's `js_check_stack_overflow`: compare the address of a local against a
//! per-thread limit computed once from the thread's stack bounds.

#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

use std::cell::Cell;

/// Native stack size for threads that run an engine (workers, agents, coroutines, host pool
/// threads): room for `MAX_EVAL_DEPTH` interpreter frames, as the CLI's main thread has.
pub const THREAD_STACK_SIZE: usize = 64 * 1024 * 1024;

/// Bytes kept free below the limit for code that runs between checks (natives, host calls, the
/// unchecked tail of a recursion). Debug builds have far larger frames.
#[cfg(not(debug_assertions))]
const MIN_MARGIN: usize = 256 * 1024;
#[cfg(debug_assertions)]
const MIN_MARGIN: usize = 384 * 1024;
const MAX_MARGIN: usize = 4 * 1024 * 1024;

/// Upper bound on the native stack one unit of interpreter depth costs where no byte check runs
/// (JIT-to-JIT direct calls); see [`depth_limit`].
#[cfg(not(debug_assertions))]
const UNIT_BYTES: usize = 8 * 1024;
#[cfg(debug_assertions)]
const UNIT_BYTES: usize = 48 * 1024;

thread_local! {
    /// The lowest stack address checked code may reach; 0 = not computed yet, 1 = unknown.
    static LIMIT: Cell<usize> = const { Cell::new(0) };
    /// `(base, size)` an embedder recorded for this thread (see [`set_thread_stack_size`]).
    static RECORDED: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

#[inline(always)]
fn sp() -> usize {
    let x = 0u8;
    std::hint::black_box(&x) as *const u8 as usize
}

#[inline(always)]
fn limit() -> usize {
    let lim = LIMIT.with(Cell::get);
    if lim == 0 {
        init()
    } else {
        lim
    }
}

/// Bytes of native stack left above the limit on the current thread; `None` when exhausted.
#[inline]
pub(crate) fn headroom() -> Option<usize> {
    #[cfg(target_arch = "wasm32")]
    {
        Some(usize::MAX)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        sp().checked_sub(limit()).filter(|&r| r > 0)
    }
}

/// Whether the current thread's native stack is within the safety margin of its end.
#[inline]
pub(crate) fn exhausted() -> bool {
    headroom().is_none()
}

/// A recursion ceiling for code paths without a byte check (JIT direct calls): `depth` plus the
/// number of [`UNIT_BYTES`] units that fit in `room`, capped at `max`.
#[inline]
pub(crate) fn depth_cap(depth: u32, room: usize, max: u32) -> u32 {
    let units = (room / UNIT_BYTES).min(u32::MAX as usize) as u32;
    depth.saturating_add(units).min(max)
}

/// Record the size of the current thread's stack, for platforms where it cannot be queried
/// (anything but Linux, macOS and Windows). Call it first thing on the thread; elsewhere the
/// queried bounds win.
pub fn set_thread_stack_size(bytes: usize) {
    RECORDED.with(|r| r.set((sp(), bytes)));
    LIMIT.with(|l| l.set(0));
}

#[cold]
#[inline(never)]
fn init() -> usize {
    let lim = bounds()
        .or_else(|| {
            let (base, size) = RECORDED.with(Cell::get);
            (size != 0).then(|| (base.saturating_sub(size), size))
        })
        .map(|(low, size)| {
            let margin = safety_margin(size);
            (low + margin).min(sp()).max(1)
        })
        .unwrap_or(1);
    LIMIT.with(|l| l.set(lim));
    lim
}

fn safety_margin(size: usize) -> usize {
    // A fixed desktop reserve must not consume an embedded stack's entire budget.
    (size / 8).clamp(MIN_MARGIN, MAX_MARGIN).min(size / 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_stack_keeps_evaluation_headroom() {
        assert_eq!(safety_margin(256 * 1024), 64 * 1024);
        assert_eq!(safety_margin(512 * 1024), 128 * 1024);
        assert_eq!(safety_margin(64 * 1024), 16 * 1024);
    }

    #[test]
    fn desktop_stack_retains_its_reserve() {
        assert_eq!(safety_margin(2 * 1024 * 1024), MIN_MARGIN);
        assert_eq!(safety_margin(64 * 1024 * 1024), MAX_MARGIN);
    }
}

/// `(lowest address, size)` of the current thread's stack.
#[cfg(target_os = "macos")]
fn bounds() -> Option<(usize, usize)> {
    extern "C" {
        fn pthread_self() -> usize;
        fn pthread_get_stackaddr_np(thread: usize) -> *mut u8;
        fn pthread_get_stacksize_np(thread: usize) -> usize;
    }
    // SAFETY: plain queries about the calling thread.
    unsafe {
        let t = pthread_self();
        let size = pthread_get_stacksize_np(t);
        Some(((pthread_get_stackaddr_np(t) as usize).checked_sub(size)?, size))
    }
}

#[cfg(target_os = "linux")]
fn bounds() -> Option<(usize, usize)> {
    extern "C" {
        fn pthread_self() -> usize;
        fn pthread_getattr_np(thread: usize, attr: *mut u64) -> i32;
        fn pthread_attr_getstack(attr: *const u64, addr: *mut *mut u8, size: *mut usize) -> i32;
        fn pthread_attr_destroy(attr: *mut u64) -> i32;
    }
    // Larger than any libc's `pthread_attr_t` (56 bytes on glibc x86-64, 64 on aarch64).
    let mut attr = [0u64; 16];
    // SAFETY: `attr` is initialized by `pthread_getattr_np` before use and destroyed after.
    unsafe {
        if pthread_getattr_np(pthread_self(), attr.as_mut_ptr()) != 0 {
            return None;
        }
        let (mut addr, mut size) = (std::ptr::null_mut(), 0usize);
        let ok = pthread_attr_getstack(attr.as_ptr(), &mut addr, &mut size) == 0;
        pthread_attr_destroy(attr.as_mut_ptr());
        ok.then_some((addr as usize, size))
    }
}

#[cfg(windows)]
fn bounds() -> Option<(usize, usize)> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
    }
    let (mut low, mut high) = (0usize, 0usize);
    // SAFETY: writes the calling thread's stack bounds to the two out-pointers.
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    Some((low, high - low))
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn bounds() -> Option<(usize, usize)> {
    None
}
