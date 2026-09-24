//! The object slab: every [`GcBox`] lives in a 256 KiB, size-aligned chunk owned by one
//! [`GcState`](super::GcState). This replaces the old raw-pointer registry: the collector
//! enumerates objects by walking chunks, allocation bumps or pops a chunk-local free list, and
//! freeing a box needs no thread-local lookup because the chunk is found by masking the pointer.
//!
//! Two slot classes share the design ([`SlotClass`]): plain boxes, and boxes followed by
//! [`INLINE_PROPS`] property slots that a small ordinary object uses as its entry storage (see
//! `EntryVec`'s inline mode), so `new C()` / `{x, y}` / `{}` allocate one block, not two. Each
//! chunk holds one class; its header records the slot size.
//!
//! A free slot is marked by a zero weak count (a live box always holds at least the implicit
//! weak reference of its strong owners) and threads the chunk's free list through its strong
//! word. Chunks that become empty are returned to the system by [`ObjHeap::trim`], which the
//! collector calls after a sweep.
//!
//! Exclusivity follows `GcState`: a heap is touched by one thread at a time (the driver, or a
//! coroutine worker during the strict ping-pong handoff), so chunk headers are plain fields.

use super::{GcBox, Property};
use std::alloc::Layout;
use std::cell::Cell;
use std::ptr::NonNull;

/// Chunk size and alignment: a box's chunk header is at `ptr & !(CHUNK_BYTES - 1)`.
const CHUNK_BYTES: usize = 256 << 10;
/// Slots start at a cache-line boundary after the header.
const SLOTS_OFF: usize = 64;
/// Switch to an existing chunk only when at least this fraction of it is free, so a heap of
/// nearly-full chunks never degrades into a scan per allocation.
const REUSE_FREE_FRACTION: usize = 8;

/// Property slots trailing a [`SlotClass::Inline`] box.
pub(crate) const INLINE_PROPS: usize = 4;

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

/// The two box sizes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SlotClass {
    /// A bare [`GcBox`]: functions, arrays, and every object built around an existing map.
    Plain = 0,
    /// A [`GcBox`] followed by [`INLINE_PROPS`] uninitialized [`Property`] slots.
    Inline = 1,
}

impl SlotClass {
    const fn size(self) -> usize {
        match self {
            SlotClass::Plain => std::mem::size_of::<GcBox>(),
            SlotClass::Inline => round_up(
                INLINE_OFF + INLINE_PROPS * std::mem::size_of::<Property>(),
                SLOT_ALIGN,
            ),
        }
    }
    const fn slots(self) -> usize {
        (CHUNK_BYTES - SLOTS_OFF) / self.size()
    }
}

/// The inline property area of a [`SlotClass::Inline`] box.
#[inline(always)]
pub(super) fn inline_props(b: *mut GcBox) -> *mut Property {
    unsafe { b.cast::<u8>().add(INLINE_OFF).cast::<Property>() }
}

#[repr(C)]
struct ChunkHdr {
    /// Head of the free-slot list (threaded through each free box's strong word).
    free: *mut GcBox,
    /// Slots `[0, bump)` have been handed out at least once.
    bump: u32,
    /// Slots currently holding a box (live or weak-only).
    used: u32,
    /// The owning state's live-object count (`GcState::live`): a dying object decrements it
    /// through its chunk, with no thread-local lookup. Repointed at a leaked sink when the heap
    /// is dropped with this chunk still in use.
    live: *const Cell<i64>,
    /// Bytes per slot ([`SlotClass::size`]) and the slot count that fits the chunk.
    slot_size: u32,
    slots: u32,
}

const _: () = assert!(std::mem::size_of::<ChunkHdr>() <= SLOTS_OFF);
const _: () = assert!(SLOTS_OFF % SLOT_ALIGN == 0);
const _: () = assert!(SlotClass::Inline.size() % SLOT_ALIGN == 0);
// Inline storage must stay within `EntryVec`'s reach check (see `props::entries`).
const _: () = assert!(INLINE_OFF <= 224);

fn chunk_layout() -> Layout {
    Layout::from_size_align(CHUNK_BYTES, CHUNK_BYTES).expect("chunk layout")
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

#[inline]
fn is_free(b: *const GcBox) -> bool {
    unsafe { (*b).weak.get() == 0 }
}

/// The chunks of one slot class.
struct ClassHeap {
    chunks: Vec<NonNull<ChunkHdr>>,
    /// Index of the chunk allocation currently draws from.
    cur: usize,
}

pub(super) struct ObjHeap {
    classes: [ClassHeap; 2],
}

impl ObjHeap {
    pub(super) fn new() -> ObjHeap {
        ObjHeap {
            classes: [
                ClassHeap {
                    chunks: Vec::new(),
                    cur: 0,
                },
                ClassHeap {
                    chunks: Vec::new(),
                    cur: 0,
                },
            ],
        }
    }

    /// An uninitialized box slot of `class`. The caller writes a complete `GcBox` before
    /// anything can observe it (in particular a nonzero weak count, which is what marks the
    /// slot in use).
    ///
    /// `live` is the owning state's live count; it is bumped here (and decremented through the
    /// chunk by [`note_dead`]).
    #[inline(always)]
    pub(super) fn alloc(&mut self, live: &Cell<i64>, class: SlotClass) -> *mut GcBox {
        live.set(live.get() + 1);
        let h = &mut self.classes[class as usize];
        if let Some(&c) = h.chunks.get(h.cur) {
            if let Some(p) = take(c) {
                return p;
            }
        }
        self.alloc_slow(live, class)
    }

    #[cold]
    #[inline(never)]
    fn alloc_slow(&mut self, live: &Cell<i64>, class: SlotClass) -> *mut GcBox {
        let slots = class.slots();
        let threshold = slots / REUSE_FREE_FRACTION;
        let h = &mut self.classes[class as usize];
        let reusable = h.chunks.iter().position(|c| {
            let hdr = unsafe { c.as_ref() };
            slots - hdr.used as usize >= threshold
        });
        let idx = match reusable {
            Some(i) => i,
            None => {
                let raw = unsafe { std::alloc::alloc(chunk_layout()) };
                let Some(c) = NonNull::new(raw.cast::<ChunkHdr>()) else {
                    std::alloc::handle_alloc_error(chunk_layout());
                };
                unsafe {
                    c.as_ptr().write(ChunkHdr {
                        free: std::ptr::null_mut(),
                        bump: 0,
                        used: 0,
                        live,
                        slot_size: class.size() as u32,
                        slots: slots as u32,
                    })
                };
                h.chunks.push(c);
                h.chunks.len() - 1
            }
        };
        h.cur = idx;
        take(h.chunks[idx]).expect("a chunk chosen for allocation has a free slot")
    }

    /// Visit every box whose value is alive (strong count above zero).
    pub(super) fn for_each_live(&self, mut f: impl FnMut(*mut GcBox)) {
        for h in &self.classes {
            for &c in &h.chunks {
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
    /// steady allocate/free cycle does not map and unmap chunks each time.
    pub(super) fn trim(&mut self, keep: usize) {
        let keep = keep.max(1);
        for h in &mut self.classes {
            let mut spare = 0;
            let cur = h.chunks.get(h.cur).copied();
            h.chunks.retain(|&c| {
                if unsafe { c.as_ref() }.used != 0 {
                    return true;
                }
                if spare < keep || Some(c) == cur {
                    spare += 1;
                    return true;
                }
                unsafe { std::alloc::dealloc(c.as_ptr().cast(), chunk_layout()) };
                false
            });
            h.cur = cur
                .and_then(|c| h.chunks.iter().position(|&x| x == c))
                .unwrap_or(0);
        }
    }

    #[cfg(test)]
    pub(super) fn chunk_count(&self) -> usize {
        self.classes.iter().map(|h| h.chunks.len()).sum()
    }
}

impl Drop for ObjHeap {
    fn drop(&mut self) {
        // Boxes still in use belong to objects that outlive this heap's thread (process exit,
        // or handles leaked across threads); their chunks are leaked rather than freed under
        // them. Empty chunks go back to the system. A leaked chunk's live-count pointer would
        // dangle once the owning state is gone, so it is repointed at a leaked sink.
        let mut sink: Option<&'static Cell<i64>> = None;
        for h in &self.classes {
            for &c in &h.chunks {
                let hdr = unsafe { &mut *c.as_ptr() };
                if hdr.used == 0 {
                    unsafe { std::alloc::dealloc(c.as_ptr().cast(), chunk_layout()) };
                } else {
                    hdr.live = *sink.get_or_insert_with(|| Box::leak(Box::new(Cell::new(0))));
                }
            }
        }
    }
}

#[inline(always)]
fn take(c: NonNull<ChunkHdr>) -> Option<*mut GcBox> {
    let h = unsafe { &mut *c.as_ptr() };
    if !h.free.is_null() {
        let p = h.free;
        h.free = unsafe { (*p).strong.get() } as *mut GcBox;
        h.used += 1;
        return Some(p);
    }
    if h.bump < h.slots {
        let p = slot(c, h.bump as usize);
        h.bump += 1;
        h.used += 1;
        return Some(p);
    }
    None
}

/// Count a box's object as dead (its value is about to be dropped).
///
/// # Safety
/// `p` must be a box allocated by [`ObjHeap::alloc`] whose chunk is still mapped.
#[inline(always)]
pub(super) unsafe fn note_dead(p: *mut GcBox) {
    let c = ((p as usize) & !(CHUNK_BYTES - 1)) as *const ChunkHdr;
    let live = &*(*c).live;
    live.set(live.get() - 1);
}

/// Return a box's slot to its chunk. The value must already be dropped and both counts zero.
///
/// # Safety
/// `p` must be a box allocated by [`ObjHeap::alloc`] whose chunk is still mapped.
#[inline]
pub(super) unsafe fn free(p: *mut GcBox) {
    let c = ((p as usize) & !(CHUNK_BYTES - 1)) as *mut ChunkHdr;
    let h = &mut *c;
    (*p).weak.set(0);
    (*p).strong.set(h.free as usize);
    h.free = p;
    h.used -= 1;
}
