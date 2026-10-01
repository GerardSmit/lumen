//! A thread-local size-class caching allocator (std-only).
//!
//! The engine's workloads are allocation-bound in exactly the way general-purpose system
//! allocators are slowest: millions of short-lived, same-sized blocks (an `ObjCell`
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
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed};

/// Largest cached block size, in bytes.
const MAX_CLASS: usize = 1024;
/// 16-byte class granularity (also the maximum supported alignment for cached blocks).
const STEP: usize = 16;
const NUM_CLASSES: usize = MAX_CLASS / STEP;
/// Maximum cached storage for one size class. Keeping the bound in bytes rather than items is
/// important: a 65,536-item limit allowed the 64 classes to retain just over 2 GiB per thread.
const BYTES_PER_CLASS: usize = 128 * 1024;
/// The classes up to [`SMALL_CLASS_BYTES`] (property-entry blocks of one to eight properties,
/// small strings, scope maps) get a deeper list: a cycle collection frees one block per garbage
/// object in a burst of 100k+, and a shallow cap sent all but the first few thousand back to the
/// system heap only for the next interval to allocate them from it again. `trim` still drains
/// everything after a large collection (rate-limited).
const SMALL_CLASS_BYTES: usize = 128;
const BYTES_PER_SMALL_CLASS: usize = 4 * 1024 * 1024;
/// Bound on the whole thread-local cache. The per-class caps sum to more: a burst concentrates
/// in a few classes, which may each fill to their cap, but not all of them at once.
const TOTAL_CACHE_BYTES: usize = 16 * 1024 * 1024;

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

/// Exact allocation counters for `LUMEN_MEM_STATS` (see the engine's `memstats`). They sit on the
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
pub fn note_slab(delta: isize) {
    stats::cat_add(CAT_SLAB, delta);
    let _ = stats::LIVE.fetch_add(delta, std::sync::atomic::Ordering::Relaxed);
}

/// Live bytes per allocation category (see the `mem-stats` allocator below).
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

/// The category the object slab's chunks are counted under (`memstats::Cat::Slab`).
#[cfg(feature = "mem-stats")]
const CAT_SLAB: u8 = 1;

macro_rules! stat {
    ($f:ident($e:expr)) => {
        #[cfg(feature = "mem-stats")]
        stats::$f($e);
    };
}

struct Cache {
    heads: [Cell<*mut u8>; NUM_CLASSES],
    counts: [Cell<usize>; NUM_CLASSES],
    /// Bytes on all the lists (see [`TOTAL_CACHE_BYTES`]).
    bytes: Cell<usize>,
    /// [`GUARD_NONE`] until the first cached free registers the teardown guard, then
    /// [`GUARD_LIVE`]; [`GUARD_DEAD`] once the guard drained the lists at thread exit (later
    /// frees go straight to the system).
    guard: Cell<u8>,
    /// Bytes allocated (+) or freed (-) on this thread not yet folded into [`HEAP_BYTES`].
    pending: Cell<isize>,
    /// Requested bytes this thread allocated minus the ones it freed (see [`thread_live_bytes`]).
    live: Cell<isize>,
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
            flush_pending(c);
            drain(c);
        });
    }
}

thread_local! {
    static CACHE: Cache = const {
        Cache {
            heads: [const { Cell::new(std::ptr::null_mut()) }; NUM_CLASSES],
            counts: [const { Cell::new(0) }; NUM_CLASSES],
            bytes: Cell::new(0),
            guard: Cell::new(GUARD_NONE),
            pending: Cell::new(0),
            live: Cell::new(0),
        }
    };
    static GUARD: Guard = const { Guard };
}

/// Process-wide bytes [`ClassAlloc`] holds from the system: live blocks (rounded to their size
/// class) plus the blocks cached on the per-thread free lists. Counting at the system boundary
/// keeps the cache-hit fast path free of accounting. Threads batch their deltas in `Cache::pending` and
/// fold them in here once they pass [`FLUSH_BYTES`], so the total lags by at most that much per
/// thread. Coroutine threads allocate and free each other's blocks, so only the sum is meaningful.
static HEAP_BYTES: AtomicIsize = AtomicIsize::new(0);
const FLUSH_BYTES: isize = 256 * 1024;
/// Set by the first allocation through [`ClassAlloc`]: without it [`heap_bytes`] means nothing.
static ACTIVE: AtomicBool = AtomicBool::new(false);

#[inline(always)]
fn note(c: &Cache, delta: isize) {
    let p = c.pending.get() + delta;
    c.pending.set(p);
    if (p + FLUSH_BYTES) as usize > 2 * FLUSH_BYTES as usize {
        flush_pending(c);
    }
}

#[cold]
#[inline(never)]
fn flush_pending(c: &Cache) {
    let p = c.pending.replace(0);
    HEAP_BYTES.fetch_add(p, Relaxed);
    if !ACTIVE.load(Relaxed) {
        ACTIVE.store(true, Relaxed);
    }
}

/// A thread whose cache is gone (teardown) counts straight into the shared total.
#[cold]
#[inline(never)]
fn note_dead(delta: isize) {
    HEAP_BYTES.fetch_add(delta, Relaxed);
}

/// This thread's cache. `CACHE` is const-initialized and has no destructor, so it stays valid
/// for the whole life of the thread (teardown included); reading it through a plain reference
/// keeps the allocation paths free of `LocalKey::with` closures, whose inlining is fragile.
#[inline(always)]
fn cache() -> &'static Cache {
    CACHE.with(|c| unsafe { &*(c as *const Cache) })
}

/// Bytes currently allocated through [`ClassAlloc`] by the whole process (this thread's pending
/// delta included), or `None` when `ClassAlloc` is not the global allocator.
pub fn heap_bytes() -> Option<usize> {
    if !ACTIVE.load(Relaxed) {
        return None;
    }
    let local = CACHE.try_with(|c| c.pending.get()).unwrap_or(0);
    Some((HEAP_BYTES.load(Relaxed) + local).max(0) as usize)
}

/// Bytes the calling thread has allocated through [`ClassAlloc`] and not freed, counted at the
/// requested sizes, so a per-thread budget (one interpreter per thread) is exact. A block freed
/// on another thread counts against that thread, so only a thread that owns what it allocates
/// reads a meaningful value; zero when `ClassAlloc` is not the global allocator.
#[inline]
pub fn thread_live_bytes() -> isize {
    cache().live.get()
}

/// Memory mapped outside the allocator (the object slab's chunks on Windows) that should still
/// count toward [`heap_bytes`].
#[allow(dead_code)]
pub fn note_external(delta: isize) {
    HEAP_BYTES.fetch_add(delta, Relaxed);
}

#[doc(hidden)]
pub fn cached_bytes_for_test() -> usize {
    CACHE.with(|cache| {
        cache
            .counts
            .iter()
            .enumerate()
            .map(|(class, count)| count.get() * (class + 1) * STEP)
            .sum()
    })
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
        Some(size.div_ceil(STEP) - 1)
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
    HEAP_BYTES.fetch_sub(cache.bytes.get() as isize, Relaxed);
    cache.bytes.set(0);
}

/// Return cached small blocks to the system and ask the platform allocator to release unused
/// pages. Called only after a major GC: doing this on every collection would throw away the hot
/// allocation cache and turn steady-state churn back into malloc/free traffic.
pub fn trim() {
    let _ = CACHE.try_with(drain);
    release_free_pages();
}

/// Ask the platform allocator to return its free pages to the OS, keeping this thread's cache.
pub fn release_free_pages() {
    #[cfg(target_os = "macos")]
    unsafe {
        unsafe extern "C" {
            fn malloc_zone_pressure_relief(zone: *mut std::ffi::c_void, goal: usize) -> usize;
        }
        // A null zone requests pressure relief from every registered malloc zone (including the
        // nano/tiny zones that may own allocations returned by `System`).
        let _ = malloc_zone_pressure_relief(std::ptr::null_mut(), 0);
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        // glibc keeps freed arena pages resident after parse/compile bursts.
        // This only releases unused pages; it cannot move live Rust/JS objects.
        // No equivalent symbol is assumed on musl or Android's bionic.
        let _ = malloc_trim(0);
    }
}

/// See the module docs.
pub struct ClassAlloc;

impl ClassAlloc {
    #[inline(always)]
    unsafe fn raw_alloc(&self, layout: Layout) -> *mut u8 {
        let c = cache();
        c.live.set(c.live.get() + layout.size() as isize);
        if let Some(class) = class_of(layout.size(), layout.align()) {
            let p = c.heads[class].get();
            if !p.is_null() {
                c.heads[class].set(unsafe { *(p as *mut *mut u8) });
                c.counts[class].set(c.counts[class].get() - 1);
                c.bytes.set(c.bytes.get() - (class + 1) * STEP);
                stat!(cached(-(class_layout(class).size() as isize)));
                return p;
            }
            note(c, ((class + 1) * STEP) as isize);
            let p = System.alloc(class_layout(class));
            if p.is_null() {
                c.live.set(c.live.get() - layout.size() as isize);
            }
            return p;
        }
        note(c, layout.size() as isize);
        let p = System.alloc(layout);
        if p.is_null() {
            c.live.set(c.live.get() - layout.size() as isize);
        }
        p
    }

    #[inline(always)]
    unsafe fn raw_dealloc(&self, ptr: *mut u8, layout: Layout) {
        let c = cache();
        c.live.set(c.live.get() - layout.size() as isize);
        if let Some(class) = class_of(layout.size(), layout.align()) {
            if c.guard.get() != GUARD_LIVE {
                if c.guard.get() == GUARD_DEAD {
                    note_dead(-(((class + 1) * STEP) as isize));
                    return System.dealloc(ptr, class_layout(class));
                }
                register_guard(c);
            }
            let bytes = c.bytes.get() + (class + 1) * STEP;
            if c.counts[class].get() < CLASS_CAPS[class] && bytes <= TOTAL_CACHE_BYTES {
                unsafe { *(ptr as *mut *mut u8) = c.heads[class].get() };
                c.heads[class].set(ptr);
                c.counts[class].set(c.counts[class].get() + 1);
                c.bytes.set(bytes);
                stat!(cached(class_layout(class).size() as isize));
                return;
            }
            note(c, -(((class + 1) * STEP) as isize));
            return System.dealloc(ptr, class_layout(class));
        }
        if c.guard.get() == GUARD_DEAD {
            note_dead(-(layout.size() as isize));
        } else {
            note(c, -(layout.size() as isize));
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
                let c = cache();
                c.live.set(c.live.get() + new_size as isize - layout.size() as isize);
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
/// allocation category that was current when it was allocated, so live bytes can be
/// attributed by category (parser, AST decode, bytecode, ...). Over-aligned blocks (the object
/// slab's chunks) carry none and are counted as the slab category.
#[cfg(feature = "mem-stats")]
unsafe impl GlobalAlloc for ClassAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        stats::alloc(layout.size());
        if layout.align() > STEP {
            stats::cat_add(CAT_SLAB, layout.size() as isize);
            return self.raw_alloc(layout);
        }
        let cat = crate::memcat::current();
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
            stats::cat_add(CAT_SLAB, -(layout.size() as isize));
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
                stats::cat_add(CAT_SLAB, d);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_live_bytes_follows_requested_sizes() {
        std::thread::spawn(|| {
            let allocator = ClassAlloc;
            let small = Layout::from_size_align(40, 8).unwrap();
            let large = Layout::from_size_align(5000, 8).unwrap();
            unsafe {
                let a = allocator.raw_alloc(small);
                let b = allocator.raw_alloc(large);
                assert_eq!(thread_live_bytes(), 5040);
                let a = allocator.raw_realloc(a, small, 100);
                assert_eq!(thread_live_bytes(), 5100);
                allocator.raw_dealloc(a, Layout::from_size_align(100, 8).unwrap());
                allocator.raw_dealloc(b, large);
            }
            assert_eq!(thread_live_bytes(), 0);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn the_whole_cache_stays_within_its_budget() {
        std::thread::spawn(|| {
            let allocator = ClassAlloc;
            for class in 0..8 {
                let layout = class_layout(class);
                let n = BYTES_PER_SMALL_CLASS / layout.size();
                let blocks: Vec<_> = (0..n).map(|_| unsafe { allocator.raw_alloc(layout) }).collect();
                for b in blocks {
                    unsafe { allocator.raw_dealloc(b, layout) };
                }
            }
            let cached = cached_bytes_for_test();
            assert!(cached <= TOTAL_CACHE_BYTES, "{cached} bytes cached");
            assert!(cached >= TOTAL_CACHE_BYTES - MAX_CLASS, "{cached} bytes cached");
            trim();
            assert_eq!(cached_bytes_for_test(), 0);
        })
        .join()
        .unwrap();
    }
}
