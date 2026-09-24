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

/// Exact allocation counters for `LUMEN_MEM_STATS` (see [`crate::memstats`]). They sit on the
/// allocation fast path, so they only exist with the `mem-stats` cargo feature.
#[cfg(feature = "mem-stats")]
mod stats {
    use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering::Relaxed};
    pub static LIVE: AtomicIsize = AtomicIsize::new(0);
    pub static PEAK: AtomicIsize = AtomicIsize::new(0);
    pub static CACHED: AtomicIsize = AtomicIsize::new(0);
    pub static COUNT: AtomicUsize = AtomicUsize::new(0);
    #[inline(always)]
    pub fn alloc(n: usize) {
        let now = LIVE.fetch_add(n as isize, Relaxed) + n as isize;
        COUNT.fetch_add(1, Relaxed);
        if now > PEAK.load(Relaxed) {
            PEAK.store(now, Relaxed);
        }
    }
    #[inline(always)]
    pub fn free(n: usize) {
        LIVE.fetch_sub(n as isize, Relaxed);
    }
    #[inline(always)]
    pub fn cached(delta: isize) {
        CACHED.fetch_add(delta, Relaxed);
    }
    pub static CATS: [AtomicIsize; 32] = [const { AtomicIsize::new(0) }; 32];
    #[inline(always)]
    pub fn cat_add(cat: u8, delta: isize) {
        CATS[(cat & 31) as usize].fetch_add(delta, Relaxed);
    }
    /// Live bytes per category and block-size bucket (see [`bucket`]).
    pub static SIZES: [[AtomicIsize; 8]; 32] =
        [const { [const { AtomicIsize::new(0) }; 8] }; 32];
    /// Block-size buckets: <=32, <=64, <=128, <=256, <=1K, <=4K, <=64K, larger.
    #[inline(always)]
    pub fn bucket(n: usize) -> usize {
        match n {
            0..=32 => 0,
            33..=64 => 1,
            65..=128 => 2,
            129..=256 => 3,
            257..=1024 => 4,
            1025..=4096 => 5,
            4097..=65536 => 6,
            _ => 7,
        }
    }
    #[inline(always)]
    pub fn size_add(cat: u8, size: usize, delta: isize) {
        SIZES[(cat & 31) as usize][bucket(size)].fetch_add(delta, Relaxed);
    }
    /// Allocations at least this big print a backtrace (0: off; see `memstats`).
    pub static TRACE_AT: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        pub static TRACING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    #[cold]
    pub fn trace(size: usize, cat: u8) {
        if TRACING.with(|t| t.replace(true)) {
            return;
        }
        let bt = std::backtrace::Backtrace::force_capture();
        eprintln!("[mem] alloc {size} B in category {cat}:
{bt}");
        TRACING.with(|t| t.set(false));
    }
}

/// Live bytes per category and block-size bucket (<=32, <=64, <=128, <=256, <=1K, <=4K, <=64K,
/// larger).
#[cfg(feature = "mem-stats")]
pub fn size_buckets() -> [[isize; 8]; 32] {
    std::array::from_fn(|c| {
        std::array::from_fn(|b| stats::SIZES[c][b].load(std::sync::atomic::Ordering::Relaxed))
    })
}

/// Print a backtrace for every later allocation of at least `bytes` (0 turns it off).
#[cfg(feature = "mem-stats")]
pub fn trace_allocations_from(bytes: usize) {
    stats::TRACE_AT.store(bytes, std::sync::atomic::Ordering::Relaxed);
}

/// Slab chunks mapped outside the allocator (see `value::heap::chunk_alloc`).
#[cfg(feature = "mem-stats")]
pub(crate) fn note_slab(delta: isize) {
    stats::cat_add(crate::memstats::Cat::Slab as u8, delta);
    let _ = stats::LIVE.fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
}

/// Live bytes per [`crate::memstats::Cat`] (see the `mem-stats` allocator below).
#[cfg(feature = "mem-stats")]
pub fn category_bytes() -> [isize; 32] {
    std::array::from_fn(|k| stats::CATS[k].load(std::sync::atomic::Ordering::Relaxed))
}

/// `(live bytes, peak live bytes, free-list cached bytes, allocation count)`.
#[cfg(feature = "mem-stats")]
pub fn counters() -> (usize, usize, usize, usize) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        stats::LIVE.load(Relaxed).max(0) as usize,
        stats::PEAK.load(Relaxed).max(0) as usize,
        stats::CACHED.load(Relaxed).max(0) as usize,
        stats::COUNT.load(Relaxed),
    )
}

macro_rules! stat {
    ($f:ident($e:expr)) => {
        #[cfg(feature = "mem-stats")]
        stats::$f($e);
    };
}

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
        stat!(cached(-((cache.counts[k].get() * layout.size()) as isize)));
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

impl ClassAlloc {
    #[inline(always)]
    unsafe fn raw_alloc(&self, layout: Layout) -> *mut u8 {
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
                stat!(cached(-(class_layout(class).size() as isize)));
                return cached;
            }
            return System.alloc(class_layout(class));
        }
        System.alloc(layout)
    }

    #[inline(always)]
    unsafe fn raw_dealloc(&self, ptr: *mut u8, layout: Layout) {
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
                    stat!(cached(class_layout(class).size() as isize));
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

    #[inline(always)]
    unsafe fn raw_realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
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
        let new_ptr = unsafe { self.raw_alloc(new_layout) };
        if !new_ptr.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(ptr, new_ptr, layout.size().min(new_size));
                self.raw_dealloc(ptr, layout);
            }
        }
        new_ptr
    }
}

#[cfg(not(feature = "mem-stats"))]
unsafe impl GlobalAlloc for ClassAlloc {
    #[inline(always)]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.raw_alloc(layout)
    }
    #[inline(always)]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.raw_dealloc(ptr, layout)
    }
    #[inline(always)]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        self.raw_realloc(ptr, layout, new_size)
    }
}

/// With `mem-stats`, every block of alignment <= 16 carries a 16-byte header recording the
/// [`crate::memstats::Cat`] that was current when it was allocated, so live bytes can be
/// attributed by category (parser, AST decode, bytecode, ...). Over-aligned blocks (the object
/// slab's chunks) carry none and are counted as [`crate::memstats::Cat::Slab`].
#[cfg(feature = "mem-stats")]
unsafe impl GlobalAlloc for ClassAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        stats::alloc(layout.size());
        if layout.align() > STEP {
            stats::cat_add(crate::memstats::Cat::Slab as u8, layout.size() as isize);
            return self.raw_alloc(layout);
        }
        let cat = crate::memstats::current_cat();
        let inner = Layout::from_size_align_unchecked(layout.size() + STEP, STEP);
        let p = self.raw_alloc(inner);
        if p.is_null() {
            return p;
        }
        *(p as *mut usize) = cat as usize;
        stats::cat_add(cat, layout.size() as isize);
        stats::size_add(cat, layout.size(), layout.size() as isize);
        let at = stats::TRACE_AT.load(std::sync::atomic::Ordering::Relaxed);
        if at != 0 && layout.size() >= at {
            stats::trace(layout.size(), cat);
        }
        p.add(STEP)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        stats::free(layout.size());
        if layout.align() > STEP {
            stats::cat_add(crate::memstats::Cat::Slab as u8, -(layout.size() as isize));
            return self.raw_dealloc(ptr, layout);
        }
        let p = ptr.sub(STEP);
        let cat = *(p as *const usize) as u8;
        stats::cat_add(cat, -(layout.size() as isize));
        stats::size_add(cat, layout.size(), -(layout.size() as isize));
        self.raw_dealloc(p, Layout::from_size_align_unchecked(layout.size() + STEP, STEP))
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if layout.align() > STEP {
            let n = self.raw_realloc(ptr, layout, new_size);
            if !n.is_null() {
                stats::free(layout.size());
                stats::alloc(new_size);
                let d = new_size as isize - layout.size() as isize;
                stats::cat_add(crate::memstats::Cat::Slab as u8, d);
            }
            return n;
        }
        let p = ptr.sub(STEP);
        let cat = *(p as *const usize) as u8;
        let old = Layout::from_size_align_unchecked(layout.size() + STEP, STEP);
        let n = self.raw_realloc(p, old, new_size + STEP);
        if n.is_null() {
            return n;
        }
        stats::free(layout.size());
        stats::alloc(new_size);
        stats::cat_add(cat, new_size as isize - layout.size() as isize);
        stats::size_add(cat, layout.size(), -(layout.size() as isize));
        stats::size_add(cat, new_size, new_size as isize);
        let at = stats::TRACE_AT.load(std::sync::atomic::Ordering::Relaxed);
        if at != 0 && new_size >= at && layout.size() < at {
            stats::trace(new_size, cat);
        }
        n.add(STEP)
    }
}
