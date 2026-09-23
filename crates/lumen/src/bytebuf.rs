//! Backing bytes of an `ArrayBuffer`: owned (`Vec`) or external — memory the embedder owns and
//! keeps alive through an `Rc` owner (e.g. a wasm linear memory in a reserved address range,
//! which must never be moved or freed by the global allocator).

use std::any::Any;
use std::ops::{Deref, DerefMut};
use std::rc::Rc;

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
            *self = ByteBuf(Repr::Heap(self.to_vec()));
        }
        if let Repr::Heap(v) = &mut self.0 {
            v.resize(n, fill);
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
        ByteBuf(Repr::Heap(self.to_vec()))
    }
}

impl From<Vec<u8>> for ByteBuf {
    fn from(v: Vec<u8>) -> ByteBuf {
        ByteBuf(Repr::Heap(v))
    }
}

impl Default for ByteBuf {
    fn default() -> ByteBuf {
        ByteBuf(Repr::Heap(Vec::new()))
    }
}
