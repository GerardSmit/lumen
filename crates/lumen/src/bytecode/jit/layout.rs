//! Inline fast paths over the engine's object layouts, emitted as IR: element reads and writes
//! on dense arrays and typed arrays, `.length` of arrays / strings / typed arrays, and shape-
//! guarded property loads baked from the chunk's inline caches. Offsets are computed from the
//! Rust types (`offset_of!` / const asserts), never hand-written.
//!
//! Every emitter takes `v`, the I64 *address of a `Value`* (a slot or a `frame.stack` entry, any
//! tag), and a `miss` block. It emits guards that branch to `miss` (with no arguments) whenever
//! the fast path does not apply, and returns with the builder positioned in a fresh block where
//! the fast path succeeded. The caller owns `miss` (creates it, emits the slow path there, and
//! seals it after the emitter returned). Emitters never call helpers, never allocate, never run
//! JS, and never change refcounts; they read (or overwrite trivially-droppable) memory only.

use lumen_codegen::{Block, FunctionBuilder, Value as IrValue};

/// `v[index]` where the element is a Number: dense `Array` elements and every numeric typed
/// array kind (converted to F64). `index` is F64; non-integral, negative or out-of-bounds
/// indices, holes, accessors and non-Number elements miss. Returns the element as F64.
pub(crate) fn elem_get_num(fb: &mut FunctionBuilder, v: IrValue, index: IrValue, miss: Block) -> IrValue {
    let _ = (fb, v, index, miss);
    todo!("layout agent")
}

/// `v[index] = n` (n F64) overwriting an existing dense `Array` element that currently holds a
/// trivially-droppable value, or storing into a numeric typed array (with the typed array's
/// conversion). Misses on anything else (including appends, frozen arrays, holes).
pub(crate) fn elem_set_num(fb: &mut FunctionBuilder, v: IrValue, index: IrValue, n: IrValue, miss: Block) {
    let _ = (fb, v, index, n, miss);
    todo!("layout agent")
}

/// `v.length` for an `Array`, a string or a typed array, as F64. Misses otherwise.
pub(crate) fn length(fb: &mut FunctionBuilder, v: IrValue, miss: Block) -> IrValue {
    let _ = (fb, v, miss);
    todo!("layout agent")
}

/// Whether `length` needs no guard beyond the value's tag for `v`'s current kind — lets the
/// translator hoist bounds facts (`i < a.length` ⇒ `a[i]` in bounds). Returns the IR I64 of the
/// element count and the F64 length. Same miss contract as [`length`]. (Used for bounds-check
/// elimination; may simply call `length` for now.)
pub(crate) fn array_len_i64(fb: &mut FunctionBuilder, v: IrValue, miss: Block) -> (IrValue, IrValue) {
    let _ = (fb, v, miss);
    todo!("layout agent")
}

/// A snapshot of a property inline cache, taken at compile time from `chunk.caches[cache]`.
pub(crate) struct PropIc {
    // layout agent: baked shape / holder identity and the slot location.
}

/// Read the property IC `cache` of the chunk at compile time. `None` when the IC is not in a
/// state a native fast path can bake (uninitialized, megamorphic, accessor, prototype lookups
/// the layout cannot guard cheaply...).
pub(crate) fn prop_ic(chunk: &super::super::Chunk, cache: u32) -> Option<PropIc> {
    let _ = (chunk, cache);
    todo!("layout agent")
}

/// `v.<prop>` via the baked IC: guard `v` is an object with the IC's shape and read the data
/// property. Returns `(tag: I32, payload: I64)` of the property's value; misses when the value is
/// not trivially copyable (tag > 4) so the caller never needs a refcount.
pub(crate) fn prop_get(fb: &mut FunctionBuilder, v: IrValue, ic: &PropIc, miss: Block) -> (IrValue, IrValue) {
    let _ = (fb, v, ic, miss);
    todo!("layout agent")
}
