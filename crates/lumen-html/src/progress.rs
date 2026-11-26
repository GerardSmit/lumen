//! A host hook that long layouts call as they go.
//!
//! Layout runs on the thread that also drives the host's network requests. A
//! layout of a large page can take hundreds of milliseconds, and a request
//! whose connection opens meanwhile would wait for it to finish before it can
//! send. The host registers a hook that advances such work; layout calls it
//! every [`TICKS_PER_CALL`] boxes. The hook must not touch the document, the
//! session or script state: it runs in the middle of a layout.

use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

/// Boxes laid out between two calls of the hook.
pub const TICKS_PER_CALL: u32 = 32;

static HOOK: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());
static TICKS: AtomicU32 = AtomicU32::new(0);

/// Registers the hook for every later layout, or removes it with `None`.
pub fn set_layout_progress_hook(hook: Option<fn()>) {
    HOOK.store(hook.map_or(core::ptr::null_mut(), |hook| hook as *mut ()), Ordering::Release);
}

/// Counts one box and calls the hook every [`TICKS_PER_CALL`] boxes.
#[inline]
pub(crate) fn tick() {
    let hook = HOOK.load(Ordering::Acquire);
    if hook.is_null() || TICKS.fetch_add(1, Ordering::Relaxed) % TICKS_PER_CALL != TICKS_PER_CALL - 1 {
        return;
    }
    // SAFETY: `HOOK` only ever holds null or a `fn()` stored by
    // `set_layout_progress_hook`.
    let hook: fn() = unsafe { core::mem::transmute::<*mut (), fn()>(hook) };
    hook();
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicUsize;

    static CALLS: AtomicUsize = AtomicUsize::new(0);
    fn count() { CALLS.fetch_add(1, Ordering::Relaxed); }

    #[test]
    fn the_hook_runs_once_per_batch_of_boxes() {
        // Layouts of tests on other threads tick too, and only add calls.
        set_layout_progress_hook(Some(count));
        for _ in 0..TICKS_PER_CALL * 4 { tick(); }
        set_layout_progress_hook(None);
        assert!(CALLS.load(Ordering::Relaxed) >= 4);
    }
}

