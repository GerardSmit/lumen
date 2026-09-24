//! A thread-local size-class caching allocator (std-only).
//!
//! The engine's workloads are allocation-bound in exactly the way general-purpose system
//! allocators are slowest: millions of short-lived, same-sized blocks (an `RcBox<RefCell<Object>>`
//! per JS object, an `RcBox<RefCell<Scope>>` per activation, `Props` entry vectors, `LStr`
//! buffers). On macOS in particular, `malloc`/`free` pairs dominate parser-shaped profiles.
//!
//! This allocator sits in front of [`std::alloc::System`]: small blocks (≤ [`MAX_CLASS`] bytes,
//! alignment ≤ 16) are rounded up to a 16-byte size class and served from a per-thread
//! INTRUSIVE free list — a freed block's first word points at the next free block, so the
//! allocator itself never allocates (re-entrancy is what a `Vec`-backed cache dies on).
//! Every cacheable request is allocated from the system with its CLASS layout, never the
//! caller's exact layout, so a block can migrate between call sites of the same class and the
//! system layout contract still holds. Large or over-aligned requests pass straight through.
//!
//! Threads (coroutine parking) are handled by construction: each thread caches its own frees,
//! and a block freed on a different thread than it was allocated on simply joins that thread's
//! cache — the backing system allocation is thread-agnostic. Thread teardown drains the lists
//! back to the system (`Drop`); allocation during teardown falls through to the system
//! (`try_with`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// Largest cached block size, in bytes.
const MAX_CLASS: usize = 1024;
/// 16-byte class granularity (also the maximum supported alignment for cached blocks).
const STEP: usize = 16;
const NUM_CLASSES: usize = MAX_CLASS / STEP;
/// Maximum cached storage for one size class. Keeping the bound in bytes rather than items is
/// important: a 65,536-item limit allowed the 64 classes to retain just over 2 GiB per thread.
/// This bounds the complete thread-local cache to 16 MiB while preserving a deeper free list for
/// the small, hot allocation classes.
const BYTES_PER_CLASS: usize = 256 * 1024;
/// The classes up to [`SMALL_CLASS_BYTES`] (property-entry blocks of one to eight properties,
/// small strings, scope maps) get a deeper list: a cycle collection frees one block per garbage
/// object in a burst of 100k+, and a shallow cap sent all but the first few thousand back to the
/// system heap only for the next interval to allocate them from it again. Adds at most 8 x 4 MiB
/// per thread; `trim` still drains everything after a large collection (rate-limited).
const SMALL_CLASS_BYTES: usize = 128;
const BYTES_PER_SMALL_CLASS: usize = 4 * 1024 * 1024;

const fn class_caps() -> [usize; NUM_CLASSES] {
    let mut caps = [0; NUM_CLASSES];
    let mut class = 0;
    while class < NUM_CLASSES {
        let size = (class + 1) * STEP;
        caps[class] = if size <= SMALL_CLASS_BYTES {
            BYTES_PER_SMALL_CLASS / size
        } else {
            BYTES_PER_CLASS / size
        };
        class += 1;
    }
    caps
}

const CLASS_CAPS: [usize; NUM_CLASSES] = class_caps();

struct Cache {
    heads: [Cell<*mut u8>; NUM_CLASSES],
    counts: [Cell<usize>; NUM_CLASSES],
    /// [`GUARD_NONE`] until the first cached free registers the teardown guard, then
    /// [`GUARD_LIVE`]; [`GUARD_DEAD`] once the guard drained the lists at thread exit (later
    /// frees go straight to the system).
    guard: Cell<u8>,
}

const GUARD_NONE: u8 = 0;
const GUARD_LIVE: u8 = 1;
const GUARD_DEAD: u8 = 2;

/// Drains [`CACHE`] when the thread's locals are destroyed. Kept separate so the cache itself
/// is a const-initialized, destructor-free local: every allocation reads it with a plain TLS
/// load instead of a lazy-init / destructor-state check.
struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        let _ = CACHE.try_with(|c| {
            c.guard.set(GUARD_DEAD);
            drain(c);
        });
    }
}

thread_local! {
    static CACHE: Cache = const {
        Cache {
            heads: [const { Cell::new(std::ptr::null_mut()) }; NUM_CLASSES],
            counts: [const { Cell::new(0) }; NUM_CLASSES],
            guard: Cell::new(GUARD_NONE),
        }
    };
    static GUARD: Guard = const { Guard };
}

#[cold]
#[inline(never)]
fn register_guard(c: &Cache) {
    // Mark first: registering the destructor may itself allocate and free.
    c.guard.set(GUARD_LIVE);
    let _ = GUARD.try_with(|_| {});
}

#[inline]
fn class_of(size: usize, align: usize) -> Option<usize> {
    if align <= STEP && size <= MAX_CLASS && size > 0 {
        Some((size + STEP - 1) / STEP - 1)
    } else {
        None
    }
}

#[inline]
fn class_layout(class: usize) -> Layout {
    // Size is a non-zero multiple of STEP with STEP alignment: always valid.
    unsafe { Layout::from_size_align_unchecked((class + 1) * STEP, STEP) }
}

fn drain(cache: &Cache) {
    for (k, head) in cache.heads.iter().enumerate() {
        let layout = class_layout(k);
        let mut p = head.replace(std::ptr::null_mut());
        cache.counts[k].set(0);
        while !p.is_null() {
            let next = unsafe { *(p as *mut *mut u8) };
            unsafe { System.dealloc(p, layout) };
            p = next;
        }
    }
}

/// Return cached small blocks to the system and ask the platform allocator to release unused
/// pages. Called only after a major GC: doing this on every collection would throw away the hot
/// allocation cache and turn steady-state churn back into malloc/free traffic.
pub(crate) fn trim() {
    let _ = CACHE.try_with(drain);
    #[cfg(target_os = "macos")]
    unsafe {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // A null zone requests pressure relief from every registered malloc zone (including the
        // nano/tiny zones that may own allocations returned by `System`).
        let _ = malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
}

/// See the module docs.
pub struct ClassAlloc;

unsafe impl GlobalAlloc for ClassAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if let Some(class) = class_of(layout.size(), layout.align()) {
            let cached = CACHE.with(|c| {
                let p = c.heads[class].get();
                if !p.is_null() {
                    c.heads[class].set(unsafe { *(p as *mut *mut u8) });
                    c.counts[class].set(c.counts[class].get() - 1);
                }
                p
            });
            if !cached.is_null() {
                return cached;
            }
            return System.alloc(class_layout(class));
        }
        System.alloc(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if let Some(class) = class_of(layout.size(), layout.align()) {
            let cached = CACHE.with(|c| {
                if c.guard.get() != GUARD_LIVE {
                    if c.guard.get() == GUARD_DEAD {
                        return false;
                    }
                    register_guard(c);
                }
                if c.counts[class].get() < CLASS_CAPS[class] {
                    unsafe { *(ptr as *mut *mut u8) = c.heads[class].get() };
                    c.heads[class].set(ptr);
                    c.counts[class].set(c.counts[class].get() + 1);
                    true
                } else {
                    false
                }
            });
            if !cached {
                System.dealloc(ptr, class_layout(class));
            }
            return;
        }
        System.dealloc(ptr, layout)
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Within one size class a grow/shrink is free; otherwise allocate-copy-free through
        // the same class discipline.
        if let (Some(a), Some(b)) = (
            class_of(layout.size(), layout.align()),
            class_of(new_size, layout.align()),
        ) {
            if a == b {
                return ptr;
            }
        }
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let new_ptr = unsafe { self.alloc(new_layout) };
        if !new_ptr.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
        }
        new_ptr
    }
}
