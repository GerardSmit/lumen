//! Backing bytes of an `ArrayBuffer`: owned (`Vec`) or external — memory the embedder owns and
//! keeps alive through an `Rc` owner (e.g. a wasm linear memory in a reserved address range,
//! which must never be moved or freed by the global allocator).

use std::any::Any;
use std::cell::Cell;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

// ---- external-memory pressure -----------------------------------------------------------------
// The collector is triggered by the live object count, but an ArrayBuffer's bytes live outside
// the object heap: a loop allocating a large buffer per iteration creates few objects and would
// never collect, growing until the allocator aborts. Owned backing bytes are tallied per thread
// (a ByteBuf is !Send, so it is created and dropped on its realm's thread) and crossing the
// budget requests a collection at the next GC safe point (`Interp::gc_check_amortized`).

/// Owned backing bytes allowed to accumulate before a collection is requested.
const EXTERNAL_GC_MIN: usize = 256 << 20;

thread_local! {
    static TRACKED: Cell<usize> = const { Cell::new(0) };
    static NEXT_GC: Cell<usize> = const { Cell::new(EXTERNAL_GC_MIN) };
    static PRESSURE: Cell<bool> = const { Cell::new(false) };
}

fn track_add(n: usize) {
    let _ = TRACKED.try_with(|t| {
        let total = t.get().saturating_add(n);
        t.set(total);
        if total > NEXT_GC.with(Cell::get) {
            PRESSURE.with(|p| p.set(true));
        }
    });
}

fn track_sub(n: usize) {
    let _ = TRACKED.try_with(|t| t.set(t.get().saturating_sub(n)));
}

/// Whether owned ArrayBuffer bytes grew past the budget since the last collection.
#[inline]
pub(crate) fn gc_pressure() -> bool {
    PRESSURE.with(Cell::get)
}

/// After a collection: the next request comes once the surviving bytes double (at least the
/// minimum budget above them).
pub(crate) fn after_gc() {
    let live = TRACKED.with(Cell::get);
    NEXT_GC.with(|n| n.set(live.saturating_mul(2).max(live.saturating_add(EXTERNAL_GC_MIN))));
    PRESSURE.with(|p| p.set(false));
}

pub struct ByteBuf(Repr);

enum Repr {
    Heap(Vec<u8>),
    External {
        ptr: *mut u8,
        len: usize,
        _owner: Rc<dyn Any>,
    },
}

impl ByteBuf {
    fn heap(v: Vec<u8>) -> ByteBuf {
        track_add(v.capacity());
        ByteBuf(Repr::Heap(v))
    }

    /// A view of `len` bytes at `ptr`, valid for as long as `owner` lives.
    ///
    /// # Safety
    /// `ptr..ptr + len` must stay readable and writable, and not be aliased by live Rust
    /// references, for as long as `owner` is alive.
    pub unsafe fn external(ptr: *mut u8, len: usize, owner: Rc<dyn Any>) -> ByteBuf {
        ByteBuf(Repr::External {
            ptr,
            len,
            _owner: owner,
        })
    }

    pub fn is_external(&self) -> bool {
        matches!(self.0, Repr::External { .. })
    }

    /// Resize to `n` bytes, zero-filling. An external buffer is first copied to the heap (its
    /// size belongs to its owner).
    pub fn resize(&mut self, n: usize, fill: u8) {
        if self.is_external() {
            *self = ByteBuf::heap(self.to_vec());
        }
        if let Repr::Heap(v) = &mut self.0 {
            let before = v.capacity();
            v.resize(n, fill);
            let after = v.capacity();
            if after > before {
                track_add(after - before);
            } else {
                track_sub(before - after);
            }
        }
    }
}

impl Deref for ByteBuf {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        match &self.0 {
            Repr::Heap(v) => v,
            // SAFETY: guaranteed by `external`'s contract.
            Repr::External { ptr, len, .. } => unsafe { std::slice::from_raw_parts(*ptr, *len) },
        }
    }
}

impl DerefMut for ByteBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        match &mut self.0 {
            Repr::Heap(v) => v,
            // SAFETY: guaranteed by `external`'s contract.
            Repr::External { ptr, len, .. } => unsafe {
                std::slice::from_raw_parts_mut(*ptr, *len)
            },
        }
    }
}

/// Cloning always yields an owned copy (an `ArrayBuffer` copy never aliases).
impl Clone for ByteBuf {
    fn clone(&self) -> ByteBuf {
        ByteBuf::heap(self.to_vec())
    }
}

impl Drop for ByteBuf {
    fn drop(&mut self) {
        if let Repr::Heap(v) = &self.0 {
            track_sub(v.capacity());
        }
    }
}

impl From<Vec<u8>> for ByteBuf {
    fn from(v: Vec<u8>) -> ByteBuf {
        ByteBuf::heap(v)
    }
}

impl Default for ByteBuf {
    fn default() -> ByteBuf {
        ByteBuf(Repr::Heap(Vec::new()))
    }
}
