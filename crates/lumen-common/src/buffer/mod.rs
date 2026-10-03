//! Byte buffers shared by every language Lumen hosts: JS `ArrayBuffer` / typed arrays /
//! `DataView` and Python `bytes` / `bytearray` / `memoryview` / `_struct` are facades over the
//! types here. A buffer created by one language can be handed to the other without copying: both
//! hold the same `Rc<ByteStore>`.
//!
//! # Model
//!
//! - **Storage — [`ByteStore`]** (held as `Rc<ByteStore>`): owned bytes (`Vec<u8>`) or external
//!   bytes (a raw range kept alive by an `Rc<dyn Any>` owner, e.g. wasm linear memory), plus
//!   flags: readonly, resizable (with a `max_len`), detached, and an export (pin) count. Interior
//!   mutability is handled by the store: [`ByteStore::bytes`] / [`ByteStore::bytes_mut`] return
//!   guards with a RefCell-style borrow flag; resize / edit / detach only happen with no guard and
//!   no pin live. [`ByteStore::as_ptr`] is the escape hatch for compiled code and FFI that manage
//!   aliasing themselves. Owned bytes are tallied per thread; a collector polls
//!   [`gc_pressure`] and calls [`after_gc`] (the JS engine does both at its GC safe points).
//! - **Views — [`ViewDesc`]**: offset, itemsize, element kind + byte order, shape, strides and
//!   readonly. N-dimensional and strided (Python `memoryview`); a JS typed array / DataView is
//!   the 1-D contiguous case and keeps its compact per-object record, using [`span_len`] for the
//!   length (including length-tracking views over resizable buffers).
//! - **Element formats — [`ElemKind`] and the pack/unpack functions in [`format`]**: one table for
//!   JS typed-array kinds and Python struct codes ([`struct_code`] resolves `b B h H i I l L q Q n N
//!   P e f d ? c x s p` for native `@` vs standard `= < > !` sizes). Reads/writes take an explicit
//!   [`ByteOrder`]: [`load_f64`] / [`store_f64`] (Number semantics, C-cast wrapping and
//!   `Uint8Clamped`), [`load_int`] / [`store_int_wrapping`], and the checked [`store_int_checked`] /
//!   [`store_float_checked`] / [`load`] / [`store`] (Python semantics, exact values via [`Scalar`]).
//!   Half floats come from [`crate::float16`].
//! - **Pinning**: an export (a Python `memoryview`, a buffer-protocol consumer, a native borrow)
//!   calls [`ByteStore::pin`] / [`ByteStore::unpin`] or holds an [`Export`] guard. While pinned,
//!   [`ByteStore::resize`], [`ByteStore::edit`] and [`ByteStore::detach`] fail with
//!   [`BufferError::Pinned`]; in-place writes stay allowed.
//!
//! # Error mapping
//!
//! [`BufferError`] is language-neutral; each facade maps it:
//!
//! | `BufferError`  | JS                                   | Python                                                        |
//! |----------------|--------------------------------------|---------------------------------------------------------------|
//! | `Pinned`       | `TypeError` (like a non-detachable / non-resizable buffer) | `BufferError("Existing exports of data: object cannot be re-sized")` |
//! | `Detached`     | `TypeError` (detached ArrayBuffer)   | `ValueError("operation forbidden on released memoryview object")` |
//! | `ReadOnly`     | `TypeError` (immutable ArrayBuffer)  | `TypeError("cannot modify read-only memory")`                 |
//! | `NotResizable` | `TypeError`                          | (bytes are never resized)                                     |
//! | `TooLarge`     | `RangeError`                         | `MemoryError` / `OverflowError`                               |
//! | `OutOfBounds`  | `RangeError` / `TypeError` per spec  | `IndexError` / `ValueError` per call                          |
//! | `Borrowed`     | engine bug (a guard held across JS)  | `BufferError` (re-entrant mutation)                           |
//!
//! # JS facade (crates/lumen)
//!
//! `Interp::array_buffers` maps an ArrayBuffer object to its [`StoreSlot`] (inline until
//! `Interp::array_buffer_store` shares it, so unshared buffers skip the `Rc` allocation); a detached buffer
//! has no entry (the JS object keeps its brand/max/resizable slots). Immutable buffers are
//! readonly stores; resizable buffers are stores with a `max_len`. Typed arrays, DataView, Atomics
//! and the embedding API read and write elements only through [`format`]; transfer moves the
//! `Vec` out with [`ByteStore::detach`].
//!
//! # Python facade (crates/lumen-py) — what the port must do
//!
//! - `bytearray`: hold an `Rc<ByteStore>` built with [`ByteStore::growable`]. Length-changing
//!   operations (`append`, `extend`, `insert`, `pop`, `remove`, `clear`, slice assignment with a
//!   different length, `+=`, `*=`, `del`) go through [`ByteStore::edit`] (or `resize`); map
//!   `Pinned` to `BufferError("Existing exports of data: object cannot be re-sized")`. Same-length
//!   writes use [`ByteStore::bytes_mut`] and stay allowed while exported.
//! - `bytes`: either keep `Vec<u8>` (immutable, never pinned: a memoryview of `bytes` only needs
//!   to keep the object alive) or a `ByteStore::new(v).readonly()` when it must be shareable with
//!   JS zero-copy.
//! - `memoryview`: an exporter `Rc<ByteStore>` (bytearray, readonly bytes store, `BytesIO`'s
//!   buffer, or a JS ArrayBuffer through the bridge) + an [`Export`] guard (dropped by
//!   `release()` / dealloc, replacing the hand-rolled `exports` counters) + a [`ViewDesc`]. Parse
//!   the format with [`StructMode::from_prefix`] / [`struct_code`] to get `elem`/`itemsize`;
//!   indexing uses [`ViewDesc::index`] / [`ViewDesc::item_offset`] + [`load`] / [`store`];
//!   slicing [`adjust_slice`] + [`ViewDesc::slice`]; `cast` [`ViewDesc::cast`] (the "one side
//!   must be a byte format" rule stays in the facade); `tobytes` / `tolist` / slice assignment
//!   [`ViewDesc::gather`] / [`ViewDesc::for_each_offset`] / [`ViewDesc::scatter`];
//!   `c_contiguous` / `f_contiguous` / `contiguous` the `is_*_contiguous` helpers; validate with
//!   [`ViewDesc::check`] against the store's current length on every access.
//! - `_struct`: compile formats with [`StructMode`] + [`struct_code`] (alignment from
//!   [`StructCode::align`] in native mode); pack with [`store_int_checked`] (map
//!   `OutOfRange{lo,hi}` to `struct.error("'%c' format requires lo <= number <= hi")`; `P` keeps
//!   its own wider range check), [`store_float_checked`] (`FloatOverflow` to `OverflowError("float
//!   too large to pack with f format")`), `?` / `c` via [`store`]; unpack with [`load`]. This
//!   replaces the private `item_size` / `pack` / `unpack` / `read_uint` / `char_info` tables in
//!   `memview.rs` and `structm.rs`.

pub mod format;
mod store;
pub mod view;

pub use format::{
    load, load_bits, load_f64, load_int, store, store_bits, store_f64, store_float_checked, store_int_checked,
    store_int_wrapping, struct_code, ByteOrder, ElemKind, PackError, Scalar, StructCode, StructMode,
};
pub use store::{after_gc, gc_pressure, tracked_bytes, ByteStore, Bytes, BytesMut, Export, Lend, StoreSlot};
pub use view::{adjust_slice, adjust_slice_bounds, c_strides, f_strides, shape_product, span_len, CastError, ViewDesc};

/// Why a buffer operation was refused; each language facade maps it to its own exception (see
/// the table in the module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BufferError {
    /// The store was detached (its bytes transferred or released).
    Detached,
    /// Live exports forbid changing the store's size or identity.
    Pinned,
    /// The store's contents are immutable.
    ReadOnly,
    /// A fixed-length store cannot change length.
    NotResizable,
    /// The requested length exceeds the store's maximum.
    TooLarge,
    /// A range lies outside the store.
    OutOfBounds,
    /// The bytes are borrowed by a live guard (a re-entrant access).
    Borrowed,
}

impl BufferError {
    /// A neutral description, for facades without a language-specific message.
    pub fn message(self) -> &'static str {
        match self {
            BufferError::Detached => "buffer is detached",
            BufferError::Pinned => "buffer has live exports and cannot be resized or detached",
            BufferError::ReadOnly => "buffer is read-only",
            BufferError::NotResizable => "buffer is not resizable",
            BufferError::TooLarge => "buffer length exceeds its maximum",
            BufferError::OutOfBounds => "range is outside the buffer",
            BufferError::Borrowed => "buffer is already borrowed",
        }
    }
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for BufferError {}
