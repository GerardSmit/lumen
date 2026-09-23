//! The object slab: every [`GcBox`] lives in a 256 KiB, size-aligned chunk owned by one
//! [`GcState`](super::GcState). This replaces the old raw-pointer registry: the collector
//! enumerates objects by walking chunks, allocation bumps or pops a chunk-local free list, and
//! freeing a box needs no thread-local lookup because the chunk is found by masking the pointer.
//!
//! A free slot is marked by a zero weak count (a live box always holds at least the implicit
//! weak reference of its strong owners) and threads the chunk's free list through its strong
//! word. Chunks that become empty are returned to the system by [`ObjHeap::trim`], which the
//! collector calls after a sweep.
//!
//! Exclusivity follows `GcState`: a heap is touched by one thread at a time (the driver, or a
//! coroutine worker during the strict ping-pong handoff), so chunk headers are plain fields.

use super::GcBox;
use std::alloc::Layout;
use std::ptr::NonNull;

/// Chunk size and alignment: a box's chunk header is at `ptr & !(CHUNK_BYTES - 1)`.
const CHUNK_BYTES: usize = 256 << 10;
/// Slots start at a cache-line boundary after the header.
const SLOTS_OFF: usize = 64;
const SLOTS: usize = (CHUNK_BYTES - SLOTS_OFF) / std::mem::size_of::<GcBox>();
/// Switch to an existing chunk only when at least this fraction of it is free, so a heap of
/// nearly-full chunks never degrades into a scan per allocation.
const REUSE_FREE_FRACTION: usize = 8;

#[repr(C)]
struct ChunkHdr {
    /// Head of the free-slot list (threaded through each free box's strong word).
    free: *mut GcBox,
    /// Slots `[0, bump)` have been handed out at least once.
    bump: u32,
    /// Slots currently holding a box (live or weak-only).
    used: u32,
}

const _: () = assert!(std::mem::size_of::<ChunkHdr>() <= SLOTS_OFF);
const _: () = assert!(SLOTS_OFF % std::mem::align_of::<GcBox>() == 0);

fn chunk_layout() -> Layout {
    Layout::from_size_align(CHUNK_BYTES, CHUNK_BYTES).expect("chunk layout")
}

#[inline]
fn slot(chunk: NonNull<ChunkHdr>, i: usize) -> *mut GcBox {
    unsafe {
        chunk
            .as_ptr()
            .cast::<u8>()
            .add(SLOTS_OFF)
            .cast::<GcBox>()
            .add(i)
    }
}

#[inline]
fn is_free(b: *const GcBox) -> bool {
    unsafe { (*b).weak.get() == 0 }
}

pub(super) struct ObjHeap {
    chunks: Vec<NonNull<ChunkHdr>>,
    /// Index of the chunk allocation currently draws from.
    cur: usize,
}

impl ObjHeap {
    pub(super) fn new() -> ObjHeap {
        ObjHeap {
            chunks: Vec::new(),
            cur: 0,
        }
    }

    /// An uninitialized box slot. The caller writes a complete `GcBox` before anything can
    /// observe it (in particular a nonzero weak count, which is what marks the slot in use).
    #[inline]
    pub(super) fn alloc(&mut self) -> *mut GcBox {
        if let Some(&c) = self.chunks.get(self.cur) {
            if let Some(p) = take(c) {
                return p;
            }
        }
        self.alloc_slow()
    }

    #[cold]
    fn alloc_slow(&mut self) -> *mut GcBox {
        let threshold = SLOTS / REUSE_FREE_FRACTION;
        let reusable = self.chunks.iter().position(|c| {
            let h = unsafe { c.as_ref() };
            SLOTS - h.used as usize >= threshold
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
                    })
                };
                self.chunks.push(c);
                self.chunks.len() - 1
            }
        };
        self.cur = idx;
        take(self.chunks[idx]).expect("a chunk chosen for allocation has a free slot")
    }

    /// Visit every box whose value is alive (strong count above zero).
    pub(super) fn for_each_live(&self, mut f: impl FnMut(*mut GcBox)) {
        for &c in &self.chunks {
            let h = unsafe { c.as_ref() };
            if h.used == 0 {
                continue;
            }
            for i in 0..h.bump as usize {
                let b = slot(c, i);
                if !is_free(b) && unsafe { (*b).strong.get() } > 0 {
                    f(b);
                }
            }
        }
    }

    /// Return empty chunks to the system, keeping one spare so a steady allocate/free cycle
    /// does not map and unmap a chunk each time.
    pub(super) fn trim(&mut self) {
        let mut spare = false;
        let cur = self.chunks.get(self.cur).copied();
        self.chunks.retain(|&c| {
            if unsafe { c.as_ref() }.used != 0 {
                return true;
            }
            if !spare || Some(c) == cur {
                spare = true;
                return true;
            }
            unsafe { std::alloc::dealloc(c.as_ptr().cast(), chunk_layout()) };
            false
        });
        self.cur = cur
            .and_then(|c| self.chunks.iter().position(|&x| x == c))
            .unwrap_or(0);
    }

    #[cfg(test)]
    pub(super) fn chunk_count(&self) -> usize {
        self.chunks.len()
    }
}

impl Drop for ObjHeap {
    fn drop(&mut self) {
        // Boxes still in use belong to objects that outlive this heap's thread (process exit,
        // or handles leaked across threads); their chunks are leaked rather than freed under
        // them. Empty chunks go back to the system.
        for &c in &self.chunks {
            if unsafe { c.as_ref() }.used == 0 {
                unsafe { std::alloc::dealloc(c.as_ptr().cast(), chunk_layout()) };
            }
        }
    }
}

#[inline]
fn take(c: NonNull<ChunkHdr>) -> Option<*mut GcBox> {
    let h = unsafe { &mut *c.as_ptr() };
    if !h.free.is_null() {
        let p = h.free;
        h.free = unsafe { (*p).strong.get() } as *mut GcBox;
        h.used += 1;
        return Some(p);
    }
    if (h.bump as usize) < SLOTS {
        let p = slot(c, h.bump as usize);
        h.bump += 1;
        h.used += 1;
        return Some(p);
    }
    None
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
