//! A `System`-backed global allocator with a hard cap on live bytes, for test harness processes
//! that must die cleanly on a runaway program: an allocation past the cap returns null (so
//! `handle_alloc_error` aborts the process) before any memory is committed.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct CappedAlloc {
    live: AtomicUsize,
    cap: AtomicUsize,
}

impl CappedAlloc {
    pub const fn new(cap: usize) -> CappedAlloc {
        CappedAlloc { live: AtomicUsize::new(0), cap: AtomicUsize::new(cap) }
    }

    pub fn set_cap(&self, cap: usize) {
        self.cap.store(cap, Ordering::Relaxed);
    }

    /// Bytes currently allocated through this allocator.
    pub fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
    }

    fn reserve(&self, n: usize) -> bool {
        if self.live.fetch_add(n, Ordering::Relaxed) + n > self.cap.load(Ordering::Relaxed) {
            self.live.fetch_sub(n, Ordering::Relaxed);
            return false;
        }
        true
    }
}

// SAFETY: every call delegates to `System`; the counters never affect layout or pointer validity.
unsafe impl GlobalAlloc for CappedAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !self.reserve(layout.size()) {
            return std::ptr::null_mut();
        }
        let p = System.alloc(layout);
        if p.is_null() {
            self.live.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout);
        self.live.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let old = layout.size();
        if new_size > old && !self.reserve(new_size - old) {
            return std::ptr::null_mut();
        }
        let p = System.realloc(ptr, layout, new_size);
        if p.is_null() {
            if new_size > old {
                self.live.fetch_sub(new_size - old, Ordering::Relaxed);
            }
        } else if new_size < old {
            self.live.fetch_sub(old - new_size, Ordering::Relaxed);
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_refuses_and_counts() {
        let a = CappedAlloc::new(64);
        let small = Layout::from_size_align(32, 8).unwrap();
        // SAFETY: the layouts are valid and every successful allocation is freed with its layout.
        unsafe {
            let p = a.alloc(small);
            assert!(!p.is_null());
            assert_eq!(a.live(), 32);
            assert!(a.alloc(Layout::from_size_align(40, 8).unwrap()).is_null());
            assert!(a.realloc(p, small, 100).is_null());
            let q = a.realloc(p, small, 16);
            assert_eq!(a.live(), 16);
            a.dealloc(q, Layout::from_size_align(16, 8).unwrap());
        }
        assert_eq!(a.live(), 0);
    }
}
