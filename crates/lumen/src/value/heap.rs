//! The object slab: every [`GcBox`] lives in a 256 KiB, size-aligned chunk owned by one
//! [`GcState`](super::GcState). This replaces the old raw-pointer registry: the collector
//! enumerates objects by walking chunks, allocation bumps or pops a chunk-local free list, and
//! freeing a box needs no thread-local lookup because the chunk is found by masking the pointer.
//!
//! Several slot classes share the design ([`SlotClass`]): plain boxes, boxes followed by two,
//! four or eight property slots that a small ordinary object uses as its entry storage (see
//! `EntryVec`'s inline mode), so `new C()` / `{x, y}` / `{}` allocate one block, not two, and
//! array boxes: one property slot (`length`), the element sidecar (see `DenseStorage`) and up
//! to four element slots, so `[a, b]` is one block too. Each chunk holds one class; its header
//! records the slot size.
//!
//! A free slot is marked by a zero weak count (a live box always holds at least the implicit
//! weak reference of its strong owners). The free slots of one class form ONE list across all
//! its chunks, threaded through their second words and headed in the heap itself, so the
//! allocation fast path is a single pop (no chunk lookup, no `RefCell` flag) and the slot just
//! freed — still in cache — is the next one handed out; a chunk bump-allocates only slots it
//! never handed out. A free pushes onto that list through its chunk header's pointer to it.
//! Chunks that become empty are returned to the system by [`ObjHeap::trim`], which the
//! collector calls after a sweep and which rebuilds the lists fullest-chunk-first, so sparse
//! chunks drain. Objects that die by reference count never reach a sweep, so a free that empties
//! a chunk bumps a per-heap counter, and [`ObjHeap::trim_if_emptied`] (polled at the
//! interpreter's safe points) trims once enough chunks have emptied and stayed so.
//!
//! Exclusivity follows `GcState`: a heap is touched by one thread at a time (the driver, or a
//! coroutine worker during the strict ping-pong handoff), so chunk headers are plain fields.

use super::props::DenseBuffers;
use super::{GcBox, PackedValue, Property};
use std::alloc::Layout;
use std::cell::{Cell, UnsafeCell};
use std::ptr::NonNull;

/// Chunk size and alignment: a box's chunk header is at `ptr & !(CHUNK_BYTES - 1)`.
const CHUNK_BYTES: usize = 256 << 10;
/// Slots start at a cache-line boundary after the header.
const SLOTS_OFF: usize = 64;
/// Property slots trailing a [`SlotClass::Inline2`] box.
pub(crate) const INLINE_PROPS_NARROW: usize = 2;
/// Property slots trailing a [`SlotClass::Inline`] box.
pub(crate) const INLINE_PROPS: usize = 4;
/// Property slots trailing a [`SlotClass::Inline8`] box.
pub(crate) const INLINE_PROPS_WIDE: usize = 8;
/// Named-property slots of an array box: its `length` (more named keys spill to the heap).
pub(crate) const ARRAY_NAMED: usize = 1;
/// The most element slots an array box carries ([`SlotClass::Array4`]).
pub(crate) const ARRAY_SLOTS_MAX: usize = 4;
/// Number of slot classes.
pub(crate) const CLASSES: usize = 7;
/// Slot class names, by index (the `LUMEN_MEM_STATS` report).
pub(crate) const CLASS_NAMES: [&str; CLASSES] =
    ["plain", "inline2", "inline4", "inline8", "array0", "array2", "array4"];
/// How long empty chunks must stay unused before [`ObjHeap::trim_if_emptied`] returns them.
const PURGE_DELAY: std::time::Duration = std::time::Duration::from_secs(1);

const fn round_up(n: usize, align: usize) -> usize {
    n.div_ceil(align) * align
}
const fn max(a: usize, b: usize) -> usize {
    if a > b {
        a
    } else {
        b
    }
}
/// Offset of the inline property area from the start of its box. Sizes and alignments differ
/// by pointer width (on wasm32 / armv7 `Property` can be more aligned than `GcBox`'s size), so
/// both the offset and the slot size are rounded rather than assumed.
const INLINE_OFF: usize = round_up(
    std::mem::size_of::<GcBox>(),
    std::mem::align_of::<Property>(),
);
/// Alignment every slot start must satisfy.
const SLOT_ALIGN: usize = max(
    std::mem::align_of::<GcBox>(),
    std::mem::align_of::<Property>(),
);

/// Offset of an array box's element sidecar: after its named property slot.
const DENSE_OFF: usize = round_up(
    INLINE_OFF + ARRAY_NAMED * std::mem::size_of::<Property>(),
    std::mem::align_of::<DenseBuffers>(),
);
/// Alignment every slot start must satisfy.
const SLOT_ALIGN_ALL: usize = max(SLOT_ALIGN, std::mem::align_of::<DenseBuffers>());

const fn inline_size(n: usize) -> usize {
    round_up(INLINE_OFF + n * std::mem::size_of::<Property>(), SLOT_ALIGN)
}
/// An array box: the sidecar, then `k` element slots (see `DenseStorage::write_in_box`).
const fn array_size(k: usize) -> usize {
    round_up(
        DENSE_OFF + std::mem::size_of::<DenseBuffers>() + k * std::mem::size_of::<PackedValue>(),
        SLOT_ALIGN_ALL,
    )
}

/// The box sizes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SlotClass {
    /// A bare [`GcBox`]: functions and every object built around an existing map.
    Plain = 0,
    /// A [`GcBox`] followed by [`INLINE_PROPS_NARROW`] property slots: objects known to get
    /// one or two properties (a literal or constructor template that size).
    Inline2 = 1,
    /// A [`GcBox`] followed by [`INLINE_PROPS`] uninitialized [`Property`] slots.
    Inline = 2,
    /// A [`GcBox`] followed by [`INLINE_PROPS_WIDE`] property slots: objects of five to
    /// eight properties (a literal or constructor template that size) in one block.
    Inline8 = 3,
    /// An array: a [`GcBox`], [`ARRAY_NAMED`] property slot and the element sidecar
    /// ([`DenseBuffers`]), whose packed elements live in a heap buffer.
    Array0 = 4,
    /// An [`Array0`](SlotClass::Array0) box followed by two element slots its packed elements
    /// start out in: a small array is one block.
    Array2 = 5,
    /// As [`Array2`](SlotClass::Array2) with [`ARRAY_SLOTS_MAX`] element slots.
    Array4 = 6,
}

impl SlotClass {
    pub(super) const fn size(self) -> usize {
        match self {
            SlotClass::Plain => std::mem::size_of::<GcBox>(),
            SlotClass::Inline2 => inline_size(INLINE_PROPS_NARROW),
            SlotClass::Inline => inline_size(INLINE_PROPS),
            SlotClass::Inline8 => inline_size(INLINE_PROPS_WIDE),
            SlotClass::Array0 | SlotClass::Array2 | SlotClass::Array4 => {
                array_size(self.array_slots())
            }
        }
    }
    const fn slots(self) -> usize {
        (CHUNK_BYTES - SLOTS_OFF) / self.size()
    }
    /// Property slots trailing a box of this class (see [`inline_props`]).
    #[inline(always)]
    pub(super) const fn inline_cap(self) -> usize {
        match self {
            SlotClass::Plain => 0,
            SlotClass::Inline2 => INLINE_PROPS_NARROW,
            SlotClass::Inline => INLINE_PROPS,
            SlotClass::Inline8 => INLINE_PROPS_WIDE,
            SlotClass::Array0 | SlotClass::Array2 | SlotClass::Array4 => ARRAY_NAMED,
        }
    }
    /// Element slots following an array box's sidecar.
    #[inline(always)]
    pub(super) const fn array_slots(self) -> usize {
        match self {
            SlotClass::Array2 => 2,
            SlotClass::Array4 => ARRAY_SLOTS_MAX,
            _ => 0,
        }
    }
    /// The inline class for an object expected to get `n` named properties (at most
    /// [`INLINE_PROPS_WIDE`]). Zero means unknown: room for a few properties added one by one.
    #[inline(always)]
    pub(super) const fn inline_for(n: usize) -> SlotClass {
        if n == 0 {
            SlotClass::Inline
        } else if n <= INLINE_PROPS_NARROW {
            SlotClass::Inline2
        } else if n <= INLINE_PROPS {
            SlotClass::Inline
        } else {
            SlotClass::Inline8
        }
    }
    /// The array class for `n` initial elements: the smallest whose slots hold them, and room
    /// for a few pushes for an empty array. Longer arrays keep their elements on the heap.
    #[inline(always)]
    pub(super) const fn array_for(n: usize) -> SlotClass {
        if n == 0 {
            SlotClass::Array4
        } else if n <= 2 {
            SlotClass::Array2
        } else if n <= ARRAY_SLOTS_MAX {
            SlotClass::Array4
        } else {
            SlotClass::Array0
        }
    }
}

/// The inline property area of a box of any class but [`SlotClass::Plain`].
#[inline(always)]
pub(super) fn inline_props(b: *mut GcBox) -> *mut Property {
    unsafe { b.cast::<u8>().add(INLINE_OFF).cast::<Property>() }
}

/// The element sidecar area of an array class box.
#[inline(always)]
pub(super) fn inline_dense(b: *mut GcBox) -> *mut DenseBuffers {
    unsafe { b.cast::<u8>().add(DENSE_OFF).cast::<DenseBuffers>() }
}

#[repr(C)]
struct ChunkHdr {
    /// The free list this chunk's slots return to: its class's [`ClassHeap::free`] in the
    /// owning heap (which lives, pinned, inside its `Arc`ed `GcState`). Repointed at a leaked
    /// sink when the heap is dropped with this chunk still in use.
    free_head: *const Cell<*mut GcBox>,
    /// The owning state's live-object count (`GcState::live`): a dying object decrements it
    /// through its chunk, with no thread-local lookup. Repointed like `free_head`.
    live: *const Cell<i64>,
    /// The owning heap's [`ObjHeap::emptied`] counter. Repointed like `free_head`.
    emptied: *const Cell<u32>,
    /// Slots `[0, bump)` have been handed out at least once (the rest were never touched).
    bump: u32,
    /// Slots currently holding a box (live or weak-only).
    used: u32,
    /// Bytes per slot ([`SlotClass::size`]) and the slot count that fits the chunk.
    slot_size: u32,
    slots: u32,
    /// Bytes of the chunk backed by committed memory, from its start (see [`chunk_alloc`]).
    committed: u32,
}

const _: () = assert!(std::mem::size_of::<ChunkHdr>() <= SLOTS_OFF);
const _: () = assert!(SLOTS_OFF % SLOT_ALIGN == 0);
const _: () = assert!(SlotClass::Inline2.size() % SLOT_ALIGN == 0);
const _: () = assert!(SlotClass::Inline.size() % SLOT_ALIGN == 0);
const _: () = assert!(SlotClass::Inline8.size().is_multiple_of(SLOT_ALIGN));
const _: () = assert!(SlotClass::Array0.size().is_multiple_of(SLOT_ALIGN_ALL));
const _: () = assert!(SlotClass::Array4.size().is_multiple_of(SLOT_ALIGN_ALL));
const _: () = assert!(SLOTS_OFF.is_multiple_of(SLOT_ALIGN_ALL));
const _: () = assert!(SlotClass::Array4 as usize + 1 == CLASSES);
// Inline storage must stay within `EntryVec`'s reach check (see `props::entries`).
const _: () = assert!(INLINE_OFF <= 224);

fn chunk_layout() -> Layout {
    Layout::from_size_align(CHUNK_BYTES, CHUNK_BYTES).expect("chunk layout")
}

/// Granule in which a chunk's memory is committed as bump allocation reaches it (Windows).
#[cfg(windows)]
const COMMIT_STEP: usize = 64 << 10;

#[cfg(windows)]
mod os {
    use std::ffi::c_void;
    pub const MEM_COMMIT: u32 = 0x1000;
    pub const MEM_RESERVE: u32 = 0x2000;
    pub const MEM_RELEASE: u32 = 0x8000;
    pub const PAGE_READWRITE: u32 = 0x04;
    #[link(name = "kernel32")]
    extern "system" {
        pub fn VirtualAlloc(
            addr: *mut c_void,
            size: usize,
            kind: u32,
            protect: u32,
        ) -> *mut c_void;
        pub fn VirtualFree(addr: *mut c_void, size: usize, kind: u32) -> i32;
    }
}

/// A fresh chunk: `CHUNK_BYTES` of address space aligned to `CHUNK_BYTES`, and how many bytes
/// of it (from the start) are committed.
///
/// On Windows the system allocator serves an over-aligned request by allocating size + align
/// from the heap and committing all of it — a 256 KiB chunk cost 512 KiB of private commit.
/// Instead the chunk is reserved directly (an aligned reservation found by reserving twice the
/// size, releasing it and re-reserving the aligned sub-range) and committed in [`COMMIT_STEP`]
/// granules as bump allocation reaches them, so a class that holds a handful of objects
/// commits 64 KiB, not 512. Elsewhere untouched pages cost no memory and the system allocator
/// is used.
fn chunk_alloc() -> (*mut u8, usize) {
    #[cfg(windows)]
    unsafe {
        use os::*;
        for _ in 0..8 {
            let probe = VirtualAlloc(
                std::ptr::null_mut(),
                2 * CHUNK_BYTES,
                MEM_RESERVE,
                PAGE_READWRITE,
            );
            if probe.is_null() {
                break;
            }
            let aligned = (probe as usize + CHUNK_BYTES - 1) & !(CHUNK_BYTES - 1);
            VirtualFree(probe, 0, MEM_RELEASE);
            // Another thread may take the range between the release and this reserve: retry.
            let p = VirtualAlloc(aligned as *mut _, CHUNK_BYTES, MEM_RESERVE, PAGE_READWRITE);
            if p.is_null() {
                continue;
            }
            if VirtualAlloc(p, COMMIT_STEP, MEM_COMMIT, PAGE_READWRITE).is_null() {
                VirtualFree(p, 0, MEM_RELEASE);
                break;
            }
            #[cfg(feature = "mem-stats")]
            crate::fastalloc::note_slab(CHUNK_BYTES as isize);
            crate::fastalloc::note_external(CHUNK_BYTES as isize);
            return (p.cast(), COMMIT_STEP);
        }
        std::alloc::handle_alloc_error(chunk_layout());
    }
    #[cfg(not(windows))]
    {
        let raw = unsafe { std::alloc::alloc(chunk_layout()) };
        if raw.is_null() {
            std::alloc::handle_alloc_error(chunk_layout());
        }
        (raw, CHUNK_BYTES)
    }
}

/// Commit the chunk's memory up to at least `end` bytes from its start (Windows; see
/// [`chunk_alloc`]).
#[cfg(windows)]
#[cold]
#[inline(never)]
fn chunk_commit(hdr: &mut ChunkHdr, chunk: *mut ChunkHdr, end: usize) {
    let want = (end.div_ceil(COMMIT_STEP) * COMMIT_STEP).min(CHUNK_BYTES);
    let from = hdr.committed as usize;
    let p = unsafe {
        os::VirtualAlloc(
            chunk.cast::<u8>().add(from).cast(),
            want - from,
            os::MEM_COMMIT,
            os::PAGE_READWRITE,
        )
    };
    if p.is_null() {
        std::alloc::handle_alloc_error(chunk_layout());
    }
    hdr.committed = want as u32;
}

/// Release a chunk from [`chunk_alloc`].
unsafe fn chunk_free(c: *mut ChunkHdr) {
    #[cfg(windows)]
    {
        os::VirtualFree(c.cast(), 0, os::MEM_RELEASE);
        #[cfg(feature = "mem-stats")]
        crate::fastalloc::note_slab(-(CHUNK_BYTES as isize));
        crate::fastalloc::note_external(-(CHUNK_BYTES as isize));
    }
    #[cfg(not(windows))]
    std::alloc::dealloc(c.cast(), chunk_layout());
}

#[inline(always)]
fn chunk_of(p: *const GcBox) -> *mut ChunkHdr {
    ((p as usize) & !(CHUNK_BYTES - 1)) as *mut ChunkHdr
}

#[inline(always)]
fn slot(chunk: NonNull<ChunkHdr>, i: usize) -> *mut GcBox {
    unsafe {
        let size = (*chunk.as_ptr()).slot_size as usize;
        chunk
            .as_ptr()
            .cast::<u8>()
            .add(SLOTS_OFF + i * size)
            .cast::<GcBox>()
    }
}

/// The free list's link lives in the dead box's second word, after the header word that holds
/// the zero weak count marking the slot free.
#[inline(always)]
unsafe fn free_next(p: *mut GcBox) -> *mut GcBox {
    p.cast::<*mut GcBox>().add(1).read()
}

#[inline(always)]
unsafe fn set_free_next(p: *mut GcBox, next: *mut GcBox) {
    p.cast::<*mut GcBox>().add(1).write(next)
}

#[inline]
fn is_free(b: *const GcBox) -> bool {
    unsafe { (*b).weak.get() == 0 }
}

/// One slot class: its free list and chunks.
struct ClassHeap {
    /// Head of the class-wide free list (threaded through each free box's second word).
    free: Cell<*mut GcBox>,
    /// The chunk bump allocation draws from once the free list is empty, or null.
    cur: Cell<*mut ChunkHdr>,
    /// Every chunk of this class. Only touched on the slow paths (a chunk switch, the
    /// collector's walk, trimming), never while a caller holds a reference into it.
    chunks: UnsafeCell<Vec<NonNull<ChunkHdr>>>,
}

impl ClassHeap {
    const fn new() -> ClassHeap {
        ClassHeap {
            free: Cell::new(std::ptr::null_mut()),
            cur: Cell::new(std::ptr::null_mut()),
            chunks: UnsafeCell::new(Vec::new()),
        }
    }

    #[allow(clippy::mut_from_ref)]
    fn chunks(&self) -> &mut Vec<NonNull<ChunkHdr>> {
        // SAFETY: see the field docs; the heap is used by one thread at a time.
        unsafe { &mut *self.chunks.get() }
    }
}

/// The slab. Interior-mutable (plain cells, no `RefCell` flag on the allocation path): a heap
/// is used by one thread at a time (see the module docs), and it must not move once a chunk
/// exists (chunks point at its free-list heads) — it lives inside its `Arc`ed `GcState`.
pub(super) struct ObjHeap {
    classes: [ClassHeap; CLASSES],
    /// Frees that left a chunk empty since the last trim (see [`ObjHeap::trim_if_emptied`]).
    emptied: Cell<u32>,
    /// When a pending purge of empty chunks is due.
    purge_at: Cell<Option<std::time::Instant>>,
}

impl ObjHeap {
    /// Move all chunks from an exclusively owned heap, preserving box addresses.
    /// Both heaps must stay pinned, and no allocator or collector may run during
    /// the move. The caller transfers the live count and remaps object shapes.
    #[cfg(any(test, feature = "parallel"))]
    pub(super) fn absorb(&self, other: &Self, live: &Cell<i64>) {
        assert!(!std::ptr::eq(self, other), "cannot absorb the same heap");
        for (target, source) in self.classes.iter().zip(&other.classes) {
            let mut head = source.free.replace(std::ptr::null_mut());
            if !head.is_null() {
                let first = head;
                unsafe {
                    while !free_next(head).is_null() {
                        head = free_next(head);
                    }
                    set_free_next(head, target.free.get());
                }
                target.free.set(first);
            }
            source.cur.set(std::ptr::null_mut());
            for chunk in source.chunks().drain(..) {
                unsafe {
                    let header = &mut *chunk.as_ptr();
                    header.free_head = &target.free;
                    header.live = live;
                    header.emptied = &self.emptied;
                }
                target.chunks().push(chunk);
            }
        }
        self.emptied.set(self.emptied.get().saturating_add(other.emptied.replace(0)));
        other.purge_at.set(None);
    }

    pub(super) const fn new() -> ObjHeap {
        ObjHeap {
            classes: [const { ClassHeap::new() }; CLASSES],
            emptied: Cell::new(0),
            purge_at: Cell::new(None),
        }
    }

    /// An uninitialized box slot of `class`. The caller writes a complete `GcBox` before
    /// anything can observe it (in particular a nonzero weak count, which is what marks the
    /// slot in use).
    ///
    /// `live` is the owning state's live count; it is bumped here (and decremented through the
    /// chunk by [`note_dead`]).
    #[inline(always)]
    pub(super) fn alloc(&self, live: &Cell<i64>, class: SlotClass) -> *mut GcBox {
        live.set(live.get() + 1);
        let h = &self.classes[class as usize];
        let p = h.free.get();
        if !p.is_null() {
            unsafe {
                h.free.set(free_next(p));
                (*chunk_of(p)).used += 1;
            }
            return p;
        }
        self.alloc_slow(live, class)
    }

    /// The free list is empty: bump-allocate from the current chunk, switching to another
    /// chunk with untouched slots (or a new one) when it is full.
    #[cold]
    #[inline(never)]
    fn alloc_slow(&self, live: &Cell<i64>, class: SlotClass) -> *mut GcBox {
        let emptied = &self.emptied;
        let h = &self.classes[class as usize];
        let mut c = h.cur.get();
        if c.is_null() || unsafe { (*c).bump == (*c).slots } {
            let chunks = h.chunks();
            c = match chunks
                .iter()
                .find(|c| unsafe { c.as_ref().bump < c.as_ref().slots })
            {
                Some(c) => c.as_ptr(),
                None => {
                    let (raw, committed) = chunk_alloc();
                    let Some(c) = NonNull::new(raw.cast::<ChunkHdr>()) else {
                        std::alloc::handle_alloc_error(chunk_layout());
                    };
                    unsafe {
                        c.as_ptr().write(ChunkHdr {
                            free_head: &h.free,
                            live,
                            emptied,
                            bump: 0,
                            used: 0,
                            slot_size: class.size() as u32,
                            slots: class.slots() as u32,
                            committed: committed as u32,
                        })
                    };
                    chunks.push(c);
                    c.as_ptr()
                }
            };
            h.cur.set(c);
        }
        unsafe {
            let hdr = &mut *c;
            #[cfg(windows)]
            {
                let end = SLOTS_OFF + (hdr.bump as usize + 1) * hdr.slot_size as usize;
                if end > hdr.committed as usize {
                    chunk_commit(hdr, c, end);
                }
            }
            let p = slot(NonNull::new_unchecked(c), hdr.bump as usize);
            hdr.bump += 1;
            hdr.used += 1;
            p
        }
    }

    /// Visit every box whose value is alive (strong count above zero). `f` must not allocate
    /// or free heap objects (it may take counted handles).
    pub(super) fn for_each_live(&self, mut f: impl FnMut(*mut GcBox)) {
        for h in &self.classes {
            for &c in h.chunks().iter() {
                let hdr = unsafe { c.as_ref() };
                if hdr.used == 0 {
                    continue;
                }
                for i in 0..hdr.bump as usize {
                    let b = slot(c, i);
                    if !is_free(b) && unsafe { (*b).strong.get() } > 0 {
                        f(b);
                    }
                }
            }
        }
    }

    /// Return empty chunks to the system, keeping `keep` (at least one) spare per class so a
    /// steady allocate/free cycle does not map and unmap chunks each time, and rebuild each
    /// class's free list: the kept empty chunks start over as untouched bump space, and the
    /// free slots of partly used chunks are listed fullest chunk first, so allocation refills
    /// dense chunks and sparse ones get a chance to drain and be returned.
    pub(super) fn trim(&self, keep: usize) {
        let keep = keep.max(1);
        self.emptied.set(0);
        self.purge_at.set(None);
        for h in &self.classes {
            let chunks = h.chunks();
            let mut spare = 0;
            chunks.retain(|&c| {
                let hdr = unsafe { &mut *c.as_ptr() };
                if hdr.used != 0 {
                    return true;
                }
                if spare < keep {
                    spare += 1;
                    hdr.bump = 0;
                    return true;
                }
                unsafe { chunk_free(c.as_ptr()) };
                false
            });
            // Emptiest first: the list is built by pushing, so the fullest chunk's free slots
            // end up at its head.
            chunks.sort_by_key(|c| unsafe { c.as_ref().used });
            let mut head: *mut GcBox = std::ptr::null_mut();
            for &c in chunks.iter() {
                let hdr = unsafe { c.as_ref() };
                if hdr.used == 0 {
                    continue;
                }
                for i in (0..hdr.bump as usize).rev() {
                    let b = slot(c, i);
                    if is_free(b) {
                        unsafe { set_free_next(b, head) };
                        head = b;
                    }
                }
            }
            h.free.set(head);
            h.cur.set(
                chunks
                    .iter()
                    .find(|c| unsafe { c.as_ref().bump < c.as_ref().slots })
                    .map_or(std::ptr::null_mut(), |c| c.as_ptr()),
            );
        }
    }

    /// Return empty chunks beyond `spare` once frees have emptied more than `spare` and they are
    /// still empty [`PURGE_DELAY`] later, keeping a quarter of `spare` per class. The delay is
    /// the hysteresis: a program that drops a big structure and immediately builds another
    /// refills the same chunks instead of unmapping and faulting them back in. Returns whether
    /// chunks were freed.
    ///
    /// A counter compare while nothing is pending; while a purge is pending (only after a
    /// burst of frees), a clock read. Scans read one header per chunk.
    #[inline]
    pub(super) fn trim_if_emptied(&self, spare: usize) -> bool {
        if (self.emptied.get() as usize) < spare && self.purge_at.get().is_none() {
            return false;
        }
        self.trim_if_emptied_slow(spare)
    }

    #[cold]
    #[inline(never)]
    fn trim_if_emptied_slow(&self, spare: usize) -> bool {
        let now = std::time::Instant::now();
        match self.purge_at.get() {
            Some(at) if now < at => return false,
            Some(_) => {
                self.purge_at.set(None);
                if self.empty_chunks() > spare {
                    self.trim(spare / 4);
                    return true;
                }
            }
            None => {
                self.emptied.set(0);
                if self.empty_chunks() > spare {
                    self.purge_at.set(Some(now + PURGE_DELAY));
                }
            }
        }
        false
    }

    fn empty_chunks(&self) -> usize {
        self.classes
            .iter()
            .map(|h| {
                h.chunks()
                    .iter()
                    .filter(|c| unsafe { c.as_ref().used == 0 })
                    .count()
            })
            .sum()
    }

    /// Per slot class: `(chunks, slots in use, slots ever handed out, slot size)` — for the
    /// `LUMEN_MEM_STATS` report.
    pub(super) fn census(&self) -> [(usize, usize, usize, usize); CLASSES] {
        std::array::from_fn(|k| {
            let h = &self.classes[k];
            let (mut used, mut bumped, mut size) = (0, 0, 0);
            for c in h.chunks().iter() {
                let hdr = unsafe { c.as_ref() };
                used += hdr.used as usize;
                bumped += hdr.bump as usize;
                size = hdr.slot_size as usize;
            }
            (h.chunks().len(), used, bumped, size)
        })
    }

    #[cfg(test)]
    pub(super) fn expire_purge_delay(&self) {
        if self.purge_at.get().is_some() {
            self.purge_at.set(Some(std::time::Instant::now()));
        }
    }

    #[cfg(test)]
    pub(super) fn chunk_count(&self) -> usize {
        self.classes.iter().map(|h| h.chunks().len()).sum()
    }
}

impl Drop for ObjHeap {
    fn drop(&mut self) {
        // Boxes still in use belong to objects that outlive this heap's thread (process exit,
        // or handles leaked across threads); their chunks are leaked rather than freed under
        // them. Empty chunks go back to the system. A leaked chunk's live-count and free-list
        // pointers would dangle once the owning state is gone, so they are repointed at
        // leaked sinks (slots freed into the sink list are simply never reused).
        let mut sink: Option<&'static Cell<i64>> = None;
        let mut free_sink: Option<&'static Cell<*mut GcBox>> = None;
        let mut emptied_sink: Option<&'static Cell<u32>> = None;
        for h in &self.classes {
            for &c in h.chunks().iter() {
                let hdr = unsafe { &mut *c.as_ptr() };
                if hdr.used == 0 {
                    unsafe { chunk_free(c.as_ptr()) };
                } else {
                    hdr.live = *sink.get_or_insert_with(|| Box::leak(Box::new(Cell::new(0))));
                    hdr.free_head = *free_sink.get_or_insert_with(|| {
                        Box::leak(Box::new(Cell::new(std::ptr::null_mut())))
                    });
                    hdr.emptied =
                        *emptied_sink.get_or_insert_with(|| Box::leak(Box::new(Cell::new(0))));
                }
            }
        }
    }
}

/// Count a box's object as dead (its value is about to be dropped).
///
/// # Safety
/// `p` must be a box allocated by [`ObjHeap::alloc`] whose chunk is still mapped.
#[inline(always)]
pub(super) unsafe fn note_dead(p: *mut GcBox) {
    let live = &*(*chunk_of(p)).live;
    live.set(live.get() - 1);
}

/// Return a box's slot to its class's free list. The value must already be dropped and both
/// counts zero.
///
/// # Safety
/// `p` must be a box allocated by [`ObjHeap::alloc`] whose chunk is still mapped.
#[inline]
pub(super) unsafe fn free(p: *mut GcBox) {
    let c = &mut *chunk_of(p);
    let head = &*c.free_head;
    (*p).weak.set(0);
    set_free_next(p, head.get());
    head.set(p);
    c.used -= 1;
    if c.used == 0 {
        let e = &*c.emptied;
        e.set(e.get().saturating_add(1));
    }
}
