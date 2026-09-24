//! Inline fast paths over the engine's object layouts, emitted as IR: element reads and writes
//! on dense arrays and typed arrays, `.length` of arrays / strings / typed arrays, and shape-
//! guarded property loads baked from the chunk's inline caches. Offsets are computed from the
//! Rust types (`offset_of!` / const asserts), never hand-written.
//!
//! Every emitter takes `v`, the [`PTR`]-typed *address of a `Value`* (a slot or a `frame.stack` entry, any
//! tag), and a `miss` block. It emits guards that branch to `miss` (with no arguments) whenever
//! the fast path does not apply, and returns with the builder positioned in a fresh block where
//! the fast path succeeded. The caller owns `miss` (creates it, emits the slow path there, and
//! seals it after the emitter returned). Emitters never call helpers, never allocate, never run
//! JS, and never change refcounts; they read (or overwrite trivially-droppable) memory only.
//!
//! # Layouts relied on
//!
//! Every address is [`PTR`]-typed and loaded with [`PTR_MEM`] (pointers are 32-bit on wasm32);
//! every offset and size comes from `offset_of!` / `size_of` or a probe on this target.
//!
//! - `Value::Obj(Gc)`: the payload word is the `GcBox` address; the `Object` sits inside the
//!   box's `RefCell` at a measured offset ([`crate::value::jit_gc_offsets`]), next to the
//!   cell's `isize` borrow counter. Reads require no live mutable borrow (counter >= 0), writes
//!   require no borrow at all (counter == 0) — a borrow held across a JIT call anywhere up the
//!   stack only costs a miss, never an aliasing violation.
//! - `Object::exotic` is `repr(u8)` with declared discriminants; `Object::ic_plain` is a
//!   `Cell<bool>` byte, cleared on every object whose behavior lives in an interpreter side
//!   table (proxies, typed arrays, module namespaces), which is what lets the element paths skip
//!   `Interp::plain_for_elems`' namespace hash lookups.
//! - `Props` (`value::props::PropsLayout`): the `EntryVec` (pointer, `u32` len, `u32` cap),
//!   the `u32` shape id, `Option<Rc<Shape>>` (one nullable word; `len_slot` at a measured
//!   distance), and the nullable sidecar `Box<DenseBuffers>` with the boxed / inline packed
//!   element storage, the `Vec<u32>` slot map, the `Vec<f64>` numeric mirror and its flags.
//!   `Vec`'s field order is private to std and measured per element type (`vec_layout`).
//! - `Property`: NaN-boxed value word (`PACK_*` tags in the top 16 bits, numbers as raw f64
//!   bits with NaN canonicalized) plus a meta word whose low bits are the descriptor flags.
//! - `LStr`: pointer to a header with a `u32` UTF-8 byte length and a `u32` capacity whose top
//!   bit is the all-ASCII hint (byte length == UTF-16 length only when it is set).
//!
//! Typed arrays are not handled by these emitters: their element kind, offset and length live
//! in `Interp::typed_arrays` and their bytes in `Interp::array_buffers`, both hash maps keyed by
//! object / buffer pointer, so nothing is reachable from the object itself. Typed arrays have
//! `ic_plain` clear, so every emitter here misses on them. The translator reaches their bytes
//! through a per-site view cache instead (`Helper::TaView`; see [`side_table_object`]).

use super::{PTR, PTR_MEM, TAG_OBJ, TAG_STR, VALUE_PAYLOAD};
use crate::bytecode::{Chunk, PROP_IC_WAYS};
use crate::value::{
    Exotic, Object, Property, MIRROR_ALL_I32, MIRROR_HOLE, MIRROR_OK, PACK_BIGINT, PACK_BOOL,
    PACK_CANON_NAN, PACK_EMPTY, PACK_NULL, PACK_OBJ, PACK_STR, PACK_SYM, PACK_UNDEFINED,
    PROPERTY_META_OFFSET, PROPERTY_PACKED_OFFSET, PROP_ACCESSOR, PROP_WRITABLE,
};
use lumen_codegen::{
    BinaryOp, Block, ConvOp, FloatCC, FunctionBuilder, IntCC, MemKind, Type, Value as IrValue,
};
use std::sync::OnceLock;

// Zero-extending loads of a `u8` / `u32` field into a pointer-typed (`PTR`) IR value.
#[cfg(target_pointer_width = "64")]
const PTR_U8: MemKind = MemKind::I64U8;
#[cfg(target_pointer_width = "32")]
const PTR_U8: MemKind = MemKind::I32U8;
#[cfg(target_pointer_width = "64")]
const PTR_U32: MemKind = MemKind::I64U32;
#[cfg(target_pointer_width = "32")]
const PTR_U32: MemKind = MemKind::I32;

// The NaN-box tags, as the top 16 bits of a property's value word. Numbers are every word whose
// top 16 bits, sign masked off, lie below the first tag (`PACK_UNDEFINED`); the one negative
// tag (`PACK_OBJ`) masks onto that first tag, so `(top & 0x7fff) < TOP_UNDEFINED` is "Num".
const TOP_UNDEFINED: i64 = (PACK_UNDEFINED >> 48) as i64;
const TOP_EMPTY: i64 = (PACK_EMPTY >> 48) as i64;
const TOP_NULL: i64 = (PACK_NULL >> 48) as i64;
const TOP_BOOL: i64 = (PACK_BOOL >> 48) as i64;
const TOP_BIGINT: i64 = (PACK_BIGINT >> 48) as i64;
const TOP_SYM: i64 = (PACK_SYM >> 48) as i64;
const TOP_OBJ: i64 = (PACK_OBJ >> 48) as i64;

const _: () = {
    // Every tag sits at or above the number cut-off once the sign is masked off, and the
    // refcounted tags are exactly BIGINT..=SYM plus OBJ.
    assert!(TOP_UNDEFINED == 0x7ff9);
    assert!((TOP_OBJ & 0x7fff) == TOP_UNDEFINED);
    assert!(TOP_EMPTY > TOP_UNDEFINED && TOP_NULL > TOP_UNDEFINED && TOP_BOOL > TOP_UNDEFINED);
    assert!(TOP_BIGINT + 1 == (PACK_STR >> 48) as i64 && (PACK_STR >> 48) as i64 + 1 == TOP_SYM);
    assert!(TOP_SYM == 0x7fff);
    // The canonical NaN is itself a number word.
    assert!(((PACK_CANON_NAN >> 48) as i64 & 0x7fff) < TOP_UNDEFINED);
    assert!((MIRROR_HOLE >> 48) as i64 & 0x7fff < TOP_UNDEFINED);
};

/// Every offset the emitters use, measured once. Object / props offsets are relative to the
/// `Gc` handle word, `dense_*` to the sidecar `DenseBuffers`, `shape_len_slot` to the
/// `Rc<Shape>` handle word.
struct Layout {
    borrow: i32,
    exotic: i32,
    ic_plain: i32,
    entries_ptr: i32,
    entries_len: i32,
    entries_cap: i32,
    shape: i32,
    shape_rc: i32,
    shape_len_slot: i32,
    elems: i32,
    dense_packed: i32,
    dense_inline_len: i32,
    dense_inline_slots: i32,
    /// `dense_elems` + the `Vec<u32>` pointer / length word.
    elems_ptr: i32,
    elems_len: i32,
    mirror_ptr: i32,
    mirror_len: i32,
    mirror_flags: i32,
    /// `Vec<Property>` pointer / length words, relative to the boxed vector.
    packed_ptr: i32,
    packed_len: i32,
    prop_size: i64,
    prop_packed: i32,
    prop_meta: i32,
    str_len: i32,
    str_cap: i32,
}

/// `(pointer word, length word)` byte offsets inside a `Vec<T>`, measured on a vector whose
/// pointer, length (1) and capacity (>= 4) are pairwise distinct. `None` if ambiguous.
pub(crate) fn vec_layout<T>(one: T) -> Option<(usize, usize)> {
    if std::mem::size_of::<Vec<T>>() != 3 * std::mem::size_of::<usize>() {
        return None;
    }
    let mut v = Vec::with_capacity(4);
    v.push(one);
    if v.capacity() < 2 {
        return None;
    }
    let words: [usize; 3] = unsafe { std::mem::transmute_copy(&v) };
    let find = |x: usize| {
        let mut hits = (0..3).filter(|&w| words[w] == x);
        match (hits.next(), hits.next()) {
            (Some(w), None) => Some(w * std::mem::size_of::<usize>()),
            _ => None,
        }
    };
    Some((find(v.as_ptr() as usize)?, find(1)?))
}

fn layout() -> Option<&'static Layout> {
    static LAYOUT: OnceLock<Option<Layout>> = OnceLock::new();
    LAYOUT
        .get_or_init(|| {
            let (obj, borrow) = crate::value::jit_gc_offsets()?;
            let p = crate::value::jit_props_layout()?;
            let (u32_ptr, u32_len) = vec_layout::<u32>(7)?;
            let (f64_ptr, f64_len) = vec_layout::<f64>(7.0)?;
            let (prop_ptr, prop_len) = vec_layout(Property::plain(crate::value::Value::Undefined))?;
            let props = obj + std::mem::offset_of!(Object, props);
            let i = |x: usize| i32::try_from(x).ok();
            Some(Layout {
                borrow: i(borrow)?,
                exotic: i(obj + std::mem::offset_of!(Object, exotic))?,
                ic_plain: i(obj + std::mem::offset_of!(Object, ic_plain))?,
                entries_ptr: i(props + p.entries_ptr)?,
                entries_len: i(props + p.entries_len)?,
                entries_cap: i(props + p.entries_cap)?,
                shape: i(props + p.shape)?,
                shape_rc: i(props + p.shape_rc)?,
                shape_len_slot: i(p.shape_len_slot)?,
                elems: i(props + p.elems)?,
                dense_packed: i(p.dense_packed)?,
                dense_inline_len: i(p.dense_inline_len)?,
                dense_inline_slots: i(p.dense_inline_slots)?,
                elems_ptr: i(p.dense_elems + u32_ptr)?,
                elems_len: i(p.dense_elems + u32_len)?,
                mirror_ptr: i(p.dense_mirror + f64_ptr)?,
                mirror_len: i(p.dense_mirror + f64_len)?,
                mirror_flags: i(p.dense_mirror_flags)?,
                packed_ptr: i(prop_ptr)?,
                packed_len: i(prop_len)?,
                prop_size: std::mem::size_of::<Property>() as i64,
                prop_packed: i(PROPERTY_PACKED_OFFSET)?,
                prop_meta: i(PROPERTY_META_OFFSET)?,
                str_len: i(crate::lstr::LSTR_LEN_OFFSET)?,
                str_cap: i(crate::lstr::LSTR_CAP_OFFSET)?,
            })
        })
        .as_ref()
}

// ----- IR helpers ------------------------------------------------------------------------------

/// Continue in a fresh block when `ok` (I32) is non-zero, else branch to `miss`.
fn guard(fb: &mut FunctionBuilder, ok: IrValue, miss: Block) {
    let next = fb.create_block();
    fb.brif(ok, next, &[], miss, &[]);
    fb.seal_block(next);
    fb.switch_to_block(next);
}

fn cmp_imm(fb: &mut FunctionBuilder, cc: IntCC, ty: Type, a: IrValue, imm: i64) -> IrValue {
    let b = fb.iconst(ty, imm);
    fb.icmp(cc, a, b)
}

fn bin_imm(fb: &mut FunctionBuilder, op: BinaryOp, ty: Type, a: IrValue, imm: i64) -> IrValue {
    let b = fb.iconst(ty, imm);
    fb.binary(op, a, b)
}

/// `base + i * size` (all `PTR`).
fn scaled(fb: &mut FunctionBuilder, base: IrValue, i: IrValue, size: i64) -> IrValue {
    let off = bin_imm(fb, BinaryOp::Imul, PTR, i, size);
    fb.binary(BinaryOp::Iadd, base, off)
}

/// No layout: jump to `miss` and continue in an unreachable block (the caller keeps emitting
/// its hit path there; nothing reaches it).
fn always_miss(fb: &mut FunctionBuilder, miss: Block) {
    fb.jump(miss, &[]);
    let dead = fb.create_block();
    fb.seal_block(dead);
    fb.switch_to_block(dead);
}

/// Guard `v` holds an object whose cell is not mutably borrowed (`write`: not borrowed at all)
/// and return its `Gc` handle word.
fn object(fb: &mut FunctionBuilder, l: &Layout, v: IrValue, miss: Block, write: bool) -> IrValue {
    let tag = fb.load(MemKind::I32U8, v, 0);
    let is_obj = cmp_imm(fb, IntCC::Eq, Type::I32, tag, TAG_OBJ as i64);
    guard(fb, is_obj, miss);
    let gc = fb.load(PTR_MEM, v, VALUE_PAYLOAD);
    let flag = fb.load(PTR_MEM, gc, l.borrow);
    let cc = if write { IntCC::Eq } else { IntCC::Sge };
    let free = cmp_imm(fb, cc, PTR, flag, 0);
    guard(fb, free, miss);
    gc
}

/// Guard the object's `exotic` byte is `kind` and its `ic_plain` byte is set.
fn exotic_is(fb: &mut FunctionBuilder, l: &Layout, gc: IrValue, kinds: &[Exotic], miss: Block) {
    let ex = fb.load(MemKind::I32U8, gc, l.exotic);
    let mut ok = None;
    for &k in kinds {
        let c = cmp_imm(fb, IntCC::Eq, Type::I32, ex, k as u8 as i64);
        ok = Some(match ok {
            None => c,
            Some(prev) => fb.binary(BinaryOp::Bor, prev, c),
        });
    }
    // `ic_plain` is a bool byte: exactly 0 or 1.
    let plain = fb.load(MemKind::I32U8, gc, l.ic_plain);
    let ok = fb.binary(BinaryOp::Band, ok.expect("at least one kind"), plain);
    guard(fb, ok, miss);
}

/// The F64 `index` as a `PTR` element number: guard it is integral (a round trip through the
/// integer reproduces it; NaN and saturated values fail). Negative values stay negative and fail
/// every unsigned bounds check later, which also caps them below 2^32 like `fast_get_elem`'s
/// range test.
fn index_ptr(fb: &mut FunctionBuilder, index: IrValue, miss: Block) -> IrValue {
    let i = fb.convert(ConvOp::ToSintSat, PTR, index);
    let back = fb.convert(ConvOp::FromSint, Type::F64, i);
    let exact = fb.fcmp(FloatCC::Eq, back, index);
    guard(fb, exact, miss);
    i
}

/// I32 1 when the NaN-boxed word `bits` is a number.
fn is_num_bits(fb: &mut FunctionBuilder, bits: IrValue) -> IrValue {
    let top = bin_imm(fb, BinaryOp::Ushr, Type::I64, bits, 48);
    let top = bin_imm(fb, BinaryOp::Band, Type::I64, top, 0x7fff);
    cmp_imm(fb, IntCC::Ult, Type::I64, top, TOP_UNDEFINED)
}

/// The sidecar `DenseBuffers` pointer, guarded non-null (no sidecar = no elements).
fn dense(fb: &mut FunctionBuilder, l: &Layout, gc: IrValue, miss: Block) -> IrValue {
    let d = fb.load(PTR_MEM, gc, l.elems);
    let present = cmp_imm(fb, IntCC::Ne, PTR, d, 0);
    guard(fb, present, miss);
    d
}

/// Address of own dense element `i`'s `Property` — `Props::get_index`'s lookup: boxed packed
/// storage, else inline packed storage, else the `elems` slot map into `entries`. Misses when
/// the element is not in the dense storage (a hole, out of range, a sparse "far" key). With
/// `owned`, the `entries` route also requires an owned (mutable, not copy-on-write shared)
/// entry block. Returns `(property address, I32 1 if it lives in entries / 0 if packed)`.
fn element_prop(
    fb: &mut FunctionBuilder,
    l: &Layout,
    gc: IrValue,
    d: IrValue,
    i: IrValue,
    miss: Block,
    owned: bool,
) -> (IrValue, IrValue) {
    let join = fb.create_block();
    let prop = fb.append_block_param(join, PTR);
    let in_entries = fb.append_block_param(join, Type::I32);

    let boxed = fb.create_block();
    let not_boxed = fb.create_block();
    let packed = fb.load(PTR_MEM, d, l.dense_packed);
    let is_boxed = cmp_imm(fb, IntCC::Ne, PTR, packed, 0);
    fb.brif(is_boxed, boxed, &[], not_boxed, &[]);
    fb.seal_block(boxed);
    fb.seal_block(not_boxed);

    // Boxed packed: `Box<Vec<Property>>`, indexed directly.
    fb.switch_to_block(boxed);
    let len = fb.load(PTR_MEM, packed, l.packed_len);
    let inb = fb.icmp(IntCC::Ult, i, len);
    guard(fb, inb, miss);
    let base = fb.load(PTR_MEM, packed, l.packed_ptr);
    let p = scaled(fb, base, i, l.prop_size);
    let zero = fb.iconst(Type::I32, 0);
    fb.jump(join, &[p, zero]);

    // Inline packed: a non-zero `u8` length selects it.
    fb.switch_to_block(not_boxed);
    let inline = fb.create_block();
    let classic = fb.create_block();
    let ilen = fb.load(PTR_U8, d, l.dense_inline_len);
    let is_inline = cmp_imm(fb, IntCC::Ne, PTR, ilen, 0);
    fb.brif(is_inline, inline, &[], classic, &[]);
    fb.seal_block(inline);
    fb.seal_block(classic);

    fb.switch_to_block(inline);
    let inb = fb.icmp(IntCC::Ult, i, ilen);
    guard(fb, inb, miss);
    let slots = bin_imm(fb, BinaryOp::Iadd, PTR, d, l.dense_inline_slots as i64);
    let p = scaled(fb, slots, i, l.prop_size);
    let zero = fb.iconst(Type::I32, 0);
    fb.jump(join, &[p, zero]);

    // Classic: `elems[i]` is the entries slot, or `NO_SLOT` for a hole.
    fb.switch_to_block(classic);
    if owned {
        let cap = fb.load(PTR_U32, gc, l.entries_cap);
        let is_owned = cmp_imm(fb, IntCC::Ne, PTR, cap, 0);
        guard(fb, is_owned, miss);
    }
    let elen = fb.load(PTR_MEM, d, l.elems_len);
    let inb = fb.icmp(IntCC::Ult, i, elen);
    guard(fb, inb, miss);
    let eptr = fb.load(PTR_MEM, d, l.elems_ptr);
    let at = scaled(fb, eptr, i, 4);
    let slot = fb.load(PTR_U32, at, 0);
    // Also rejects `NO_SLOT` (u32::MAX), which is never below a u32 entry count.
    let nent = fb.load(PTR_U32, gc, l.entries_len);
    let inb = fb.icmp(IntCC::Ult, slot, nent);
    guard(fb, inb, miss);
    let base = fb.load(PTR_MEM, gc, l.entries_ptr);
    let p = scaled(fb, base, slot, l.prop_size);
    let one = fb.iconst(Type::I32, 1);
    fb.jump(join, &[p, one]);

    fb.seal_block(join);
    fb.switch_to_block(join);
    (prop, in_entries)
}

/// Guard `prop` is a data property (no accessor) holding a number; return it as F64.
fn data_num(fb: &mut FunctionBuilder, l: &Layout, prop: IrValue, miss: Block) -> IrValue {
    let meta = fb.load(PTR_MEM, prop, l.prop_meta);
    let acc = bin_imm(fb, BinaryOp::Band, PTR, meta, PROP_ACCESSOR as i64);
    let data = cmp_imm(fb, IntCC::Eq, PTR, acc, 0);
    guard(fb, data, miss);
    let bits = fb.load(MemKind::I64, prop, l.prop_packed);
    let num = is_num_bits(fb, bits);
    guard(fb, num, miss);
    fb.convert(ConvOp::Bitcast, Type::F64, bits)
}

// ----- emitters --------------------------------------------------------------------------------

/// Guard `v` holds an object with `ic_plain` clear (a typed array, proxy or module namespace)
/// and return its `Gc` handle word. No borrow is needed: nothing inside the cell is read.
pub(crate) fn side_table_object(fb: &mut FunctionBuilder, v: IrValue, miss: Block) -> IrValue {
    let Some(l) = layout() else {
        always_miss(fb, miss);
        return fb.iconst(PTR, 0);
    };
    let tag = fb.load(MemKind::I32U8, v, 0);
    let is_obj = cmp_imm(fb, IntCC::Eq, Type::I32, tag, TAG_OBJ as i64);
    guard(fb, is_obj, miss);
    let gc = fb.load(PTR_MEM, v, VALUE_PAYLOAD);
    let plain = fb.load(MemKind::I32U8, gc, l.ic_plain);
    let side = cmp_imm(fb, IntCC::Eq, Type::I32, plain, 0);
    guard(fb, side, miss);
    gc
}

/// `v[index]` where the element is a Number: dense `Array` elements and every numeric typed
/// array kind (converted to F64). `index` is F64; non-integral, negative or out-of-bounds
/// indices, holes, accessors and non-Number elements miss. Returns the element as F64.
///
/// Mirrors `Interp::fast_get_elem` for plain arrays and ordinary objects: the numeric mirror
/// first (one load), then `Props::get_index`. Typed arrays always miss (see the module docs).
pub(crate) fn elem_get_num(
    fb: &mut FunctionBuilder,
    v: IrValue,
    index: IrValue,
    miss: Block,
) -> IrValue {
    let Some(l) = layout() else {
        always_miss(fb, miss);
        return fb.f64const(0.0);
    };
    let gc = object(fb, l, v, miss, false);
    exotic_is(fb, l, gc, &[Exotic::None, Exotic::Array], miss);
    let i = index_ptr(fb, index, miss);
    let d = dense(fb, l, gc, miss);

    let done = fb.create_block();
    let result = fb.append_block_param(done, Type::F64);
    let mirror = fb.create_block();
    let mirror_load = fb.create_block();
    let classic = fb.create_block();

    // While `MIRROR_OK`, a non-hole `mirror[i]` IS element i's Num value (see `Props::mirror`).
    let flags = fb.load(MemKind::I32U8, d, l.mirror_flags);
    let ok = bin_imm(fb, BinaryOp::Band, Type::I32, flags, MIRROR_OK as i64);
    fb.brif(ok, mirror, &[], classic, &[]);
    fb.seal_block(mirror);

    fb.switch_to_block(mirror);
    let mlen = fb.load(PTR_MEM, d, l.mirror_len);
    let inb = fb.icmp(IntCC::Ult, i, mlen);
    fb.brif(inb, mirror_load, &[], classic, &[]);
    fb.seal_block(mirror_load);

    fb.switch_to_block(mirror_load);
    let mptr = fb.load(PTR_MEM, d, l.mirror_ptr);
    let at = scaled(fb, mptr, i, 8);
    let bits = fb.load(MemKind::I64, at, 0);
    let hole = fb.iconst(Type::I64, MIRROR_HOLE as i64);
    let present = fb.icmp(IntCC::Ne, bits, hole);
    let f = fb.convert(ConvOp::Bitcast, Type::F64, bits);
    fb.brif(present, done, &[f], classic, &[]);
    fb.seal_block(classic);

    // The mirror can't answer (off, short, or a hole): read the property itself.
    fb.switch_to_block(classic);
    let (prop, _) = element_prop(fb, l, gc, d, i, miss, false);
    let f = data_num(fb, l, prop, miss);
    fb.jump(done, &[f]);

    fb.seal_block(done);
    fb.switch_to_block(done);
    result
}

/// `v[index] = n` (n F64) overwriting an existing dense `Array` element that currently holds a
/// trivially-droppable value, or storing into a numeric typed array (with the typed array's
/// conversion). Misses on anything else (including appends, frozen arrays, holes).
///
/// `Props::set_index_value`'s overwrite, inline: the element must exist as a writable data
/// property holding a non-refcounted, non-`Empty` value; an `entries`-backed element also
/// updates the numeric mirror (and clears `MIRROR_ALL_I32` for a non-int32 value) while the
/// mirror is on. Every guard precedes the first store. Typed arrays always miss.
pub(crate) fn elem_set_num(
    fb: &mut FunctionBuilder,
    v: IrValue,
    index: IrValue,
    n: IrValue,
    miss: Block,
) {
    let Some(l) = layout() else {
        always_miss(fb, miss);
        return;
    };
    let gc = object(fb, l, v, miss, true);
    exotic_is(fb, l, gc, &[Exotic::None, Exotic::Array], miss);
    let i = index_ptr(fb, index, miss);
    let d = dense(fb, l, gc, miss);
    let (prop, in_entries) = element_prop(fb, l, gc, d, i, miss, true);

    // Writable data property.
    let meta = fb.load(PTR_MEM, prop, l.prop_meta);
    let attrs = bin_imm(
        fb,
        BinaryOp::Band,
        PTR,
        meta,
        (PROP_ACCESSOR | PROP_WRITABLE) as i64,
    );
    let writable = cmp_imm(fb, IntCC::Eq, PTR, attrs, PROP_WRITABLE as i64);
    guard(fb, writable, miss);

    // The old value needs no drop and is not a packed hole (`Empty`).
    let old = fb.load(MemKind::I64, prop, l.prop_packed);
    let top = bin_imm(fb, BinaryOp::Ushr, Type::I64, old, 48);
    let empty = cmp_imm(fb, IntCC::Eq, Type::I64, top, TOP_EMPTY);
    let ge = cmp_imm(fb, IntCC::Uge, Type::I64, top, TOP_BIGINT);
    let le = cmp_imm(fb, IntCC::Ule, Type::I64, top, TOP_SYM);
    let rc_prim = fb.binary(BinaryOp::Band, ge, le);
    let obj = cmp_imm(fb, IntCC::Eq, Type::I64, top, TOP_OBJ);
    let bad = fb.binary(BinaryOp::Bor, empty, rc_prim);
    let bad = fb.binary(BinaryOp::Bor, bad, obj);
    let droppable = cmp_imm(fb, IntCC::Eq, Type::I32, bad, 0);
    guard(fb, droppable, miss);

    // Mirror maintenance applies to `entries`-backed elements while the mirror is on (packed
    // storage has no mirror; `set_index_value` doesn't touch it there either).
    let flags = fb.load(MemKind::I32U8, d, l.mirror_flags);
    let on = bin_imm(fb, BinaryOp::Band, Type::I32, flags, MIRROR_OK as i64);
    let track = fb.binary(BinaryOp::Band, on, in_entries);
    let mirror = fb.create_block();
    let store = fb.create_block();
    fb.brif(track, mirror, &[], store, &[]);
    fb.seal_block(mirror);

    fb.switch_to_block(mirror);
    // The hole sentinel is never mirrored as data (the interpreter invalidates instead).
    let nbits = fb.convert(ConvOp::Bitcast, Type::I64, n);
    let hole = fb.iconst(Type::I64, MIRROR_HOLE as i64);
    let not_hole = fb.icmp(IntCC::Ne, nbits, hole);
    guard(fb, not_hole, miss);
    // Lockstep (`mirror.len() == elems.len()`) holds whenever the flag does; guard anyway.
    let mlen = fb.load(PTR_MEM, d, l.mirror_len);
    let inb = fb.icmp(IntCC::Ult, i, mlen);
    guard(fb, inb, miss);
    let mptr = fb.load(PTR_MEM, d, l.mirror_ptr);
    let at = scaled(fb, mptr, i, 8);
    fb.store(MemKind::F64, at, n, 0);
    // `f64_exact_i32`: bit-identical through an i32 round trip (excludes -0.0 and NaN).
    let as_i32 = fb.convert(ConvOp::ToSintSat, Type::I32, n);
    let back = fb.convert(ConvOp::FromSint, Type::F64, as_i32);
    let back_bits = fb.convert(ConvOp::Bitcast, Type::I64, back);
    let exact = fb.icmp(IntCC::Eq, back_bits, nbits);
    let cleared = bin_imm(
        fb,
        BinaryOp::Band,
        Type::I32,
        flags,
        !(MIRROR_ALL_I32 as i64),
    );
    let new_flags = fb.select(exact, flags, cleared);
    fb.store(MemKind::I32U8, d, new_flags, l.mirror_flags);
    fb.jump(store, &[]);
    fb.seal_block(store);

    // The entry: `PackedValue::pack(Num(n))` — raw bits, NaN canonicalized.
    fb.switch_to_block(store);
    let nbits = fb.convert(ConvOp::Bitcast, Type::I64, n);
    let is_nan = fb.fcmp(FloatCC::Ne, n, n);
    let canon = fb.iconst(Type::I64, PACK_CANON_NAN as i64);
    let packed = fb.select(is_nan, canon, nbits);
    fb.store(MemKind::I64, prop, packed, l.prop_packed);
}

/// `v.length` for an `Array`, a string or a typed array, as F64. Misses otherwise.
pub(crate) fn length(fb: &mut FunctionBuilder, v: IrValue, miss: Block) -> IrValue {
    array_len_i64(fb, v, miss).1
}

/// Whether `length` needs no guard beyond the value's tag for `v`'s current kind — lets the
/// translator hoist bounds facts (`i < a.length` ⇒ `a[i]` in bounds). Returns the IR I64 of the
/// element count and the F64 length. Same miss contract as [`length`]. (Used for bounds-check
/// elimination; may simply call `length` for now.)
///
/// Strings: the byte length when the all-ASCII hint is set (otherwise UTF-16 length differs
/// from the UTF-8 byte count: miss). Arrays (`Exotic::Array`, ic-plain): the own `length` data
/// property, found through the shape's `len_slot` memo like `Props::length_property`. Typed
/// arrays miss.
pub(crate) fn array_len_i64(
    fb: &mut FunctionBuilder,
    v: IrValue,
    miss: Block,
) -> (IrValue, IrValue) {
    let Some(l) = layout() else {
        always_miss(fb, miss);
        let n = fb.iconst(Type::I64, 0);
        let f = fb.f64const(0.0);
        return (n, f);
    };
    let done = fb.create_block();
    let count = fb.append_block_param(done, Type::I64);
    let len_f = fb.append_block_param(done, Type::F64);
    let string = fb.create_block();
    let other = fb.create_block();
    let tag = fb.load(MemKind::I32U8, v, 0);
    let is_str = cmp_imm(fb, IntCC::Eq, Type::I32, tag, TAG_STR as i64);
    fb.brif(is_str, string, &[], other, &[]);
    fb.seal_block(string);
    fb.seal_block(other);

    fb.switch_to_block(string);
    let hdr = fb.load(PTR_MEM, v, VALUE_PAYLOAD);
    let cap = fb.load(MemKind::I32, hdr, l.str_cap);
    let ascii = bin_imm(
        fb,
        BinaryOp::Band,
        Type::I32,
        cap,
        crate::lstr::ASCII_HINT as i64,
    );
    let ascii = cmp_imm(fb, IntCC::Ne, Type::I32, ascii, 0);
    guard(fb, ascii, miss);
    let n = fb.load(MemKind::I64U32, hdr, l.str_len);
    let f = fb.convert(ConvOp::FromSint, Type::F64, n);
    fb.jump(done, &[n, f]);

    fb.switch_to_block(other);
    let gc = object(fb, l, v, miss, false);
    exotic_is(fb, l, gc, &[Exotic::Array], miss);
    let shape = fb.load(PTR_MEM, gc, l.shape_rc);
    let has_shape = cmp_imm(fb, IntCC::Ne, PTR, shape, 0);
    guard(fb, has_shape, miss);
    // `NO_SLOT` (u32::MAX) is never below a u32 entry count.
    let slot = fb.load(PTR_U32, shape, l.shape_len_slot);
    let nent = fb.load(PTR_U32, gc, l.entries_len);
    let inb = fb.icmp(IntCC::Ult, slot, nent);
    guard(fb, inb, miss);
    let base = fb.load(PTR_MEM, gc, l.entries_ptr);
    let prop = scaled(fb, base, slot, l.prop_size);
    let f = data_num(fb, l, prop, miss);
    // An array length is a uint32; guard it anyway so the I64 is exact.
    let n = fb.convert(ConvOp::ToSintSat, Type::I64, f);
    let back = fb.convert(ConvOp::FromSint, Type::F64, n);
    let exact = fb.fcmp(FloatCC::Eq, back, f);
    guard(fb, exact, miss);
    fb.jump(done, &[n, f]);

    fb.seal_block(done);
    fb.switch_to_block(done);
    (count, len_f)
}

/// A snapshot of a property inline cache, taken at compile time from `chunk.caches[cache]`.
///
/// Only own-property (`depth == 0`) states on ordinary receivers are baked: one `(receiver
/// shape id, entries slot)` pair per IC way. A shape id pins the ordered named-key list of an
/// `Exotic::None` map, so the slot is valid on every object of that shape (the interpreter's
/// `ic_shape_probe` relies on the same fact). Prototype hits (methods) are left out: their
/// values are objects, which [`prop_get`] could not return anyway.
pub(crate) struct PropIc {
    ways: Vec<(u32, u32)>,
}

/// Read the property IC `cache` of the chunk at compile time. `None` when the IC is not in a
/// state a native fast path can bake (uninitialized, megamorphic, accessor, prototype lookups
/// the layout cannot guard cheaply...).
///
/// `cache` must be a property-access site's cache (`GetProp`), which owns `PROP_IC_WAYS`
/// consecutive cells: a neighbouring site's cell would name a different property.
pub(crate) fn prop_ic(chunk: &Chunk, cache: u32) -> Option<PropIc> {
    let mut ways: Vec<(u32, u32)> = Vec::new();
    for k in 0..PROP_IC_WAYS {
        let st = chunk.caches.get(cache as usize + k)?.get();
        // Exactly 0: no `IC_ARR_KEYCHK` (array holder), and not EMPTY / ABSENT / CREATE.
        if st.depth == 0 && !ways.iter().any(|&(s, _)| s == st.recv_shape) {
            ways.push((st.recv_shape, st.slot));
        }
    }
    (!ways.is_empty()).then_some(PropIc { ways })
}

/// `v.<prop>` via the baked IC: guard `v` is an object with the IC's shape and read the data
/// property. Returns `(tag: I32, payload: I64)` of the property's value; misses when the value is
/// not trivially copyable (tag > 4) so the caller never needs a refcount.
///
/// Payload encoding: the f64 bits for `Num` (tag 4), 0 / 1 for `Bool` (tag 3) — the caller
/// stores that low byte at `VALUE_BOOL` — and 0 for `Undefined` / `Null`. `Empty` misses.
pub(crate) fn prop_get(
    fb: &mut FunctionBuilder,
    v: IrValue,
    ic: &PropIc,
    miss: Block,
) -> (IrValue, IrValue) {
    let Some(l) = layout() else {
        always_miss(fb, miss);
        let t = fb.iconst(Type::I32, 0);
        let p = fb.iconst(Type::I64, 0);
        return (t, p);
    };
    let gc = object(fb, l, v, miss, false);
    exotic_is(fb, l, gc, &[Exotic::None], miss);
    let shape = fb.load(MemKind::I32, gc, l.shape);
    let nent = fb.load(PTR_U32, gc, l.entries_len);

    let got = fb.create_block();
    let bits = fb.append_block_param(got, Type::I64);
    for &(recv_shape, slot) in &ic.ways {
        let Some(off) = (slot as i64)
            .checked_mul(l.prop_size)
            .and_then(|o| i32::try_from(o).ok())
        else {
            continue;
        };
        let hit = fb.create_block();
        let next = fb.create_block();
        let same = cmp_imm(fb, IntCC::Eq, Type::I32, shape, recv_shape as i32 as i64);
        fb.brif(same, hit, &[], next, &[]);
        fb.seal_block(hit);
        fb.seal_block(next);

        fb.switch_to_block(hit);
        let inb = cmp_imm(fb, IntCC::Ugt, PTR, nent, slot as i64);
        guard(fb, inb, miss);
        let base = fb.load(PTR_MEM, gc, l.entries_ptr);
        let meta = fb.load(PTR_MEM, base, off + l.prop_meta);
        let acc = bin_imm(fb, BinaryOp::Band, PTR, meta, PROP_ACCESSOR as i64);
        let data = cmp_imm(fb, IntCC::Eq, PTR, acc, 0);
        guard(fb, data, miss);
        let b = fb.load(MemKind::I64, base, off + l.prop_packed);
        fb.jump(got, &[b]);

        fb.switch_to_block(next);
    }
    fb.jump(miss, &[]);
    fb.seal_block(got);

    // Unpack the NaN-boxed word into the `Value` tag / payload of the four copyable kinds.
    fb.switch_to_block(got);
    let is_num = is_num_bits(fb, bits);
    let top = bin_imm(fb, BinaryOp::Ushr, Type::I64, bits, 48);
    let is_undef = cmp_imm(fb, IntCC::Eq, Type::I64, top, TOP_UNDEFINED);
    let is_null = cmp_imm(fb, IntCC::Eq, Type::I64, top, TOP_NULL);
    let is_bool = cmp_imm(fb, IntCC::Eq, Type::I64, top, TOP_BOOL);
    let ok = fb.binary(BinaryOp::Bor, is_num, is_undef);
    let ok = fb.binary(BinaryOp::Bor, ok, is_null);
    let ok = fb.binary(BinaryOp::Bor, ok, is_bool);
    guard(fb, ok, miss);
    let t_num = fb.iconst(Type::I32, super::TAG_NUM as i64);
    let t_bool = fb.iconst(Type::I32, super::TAG_BOOL as i64);
    let t_null = fb.iconst(Type::I32, super::TAG_NULL as i64);
    let t_undef = fb.iconst(Type::I32, super::TAG_UNDEFINED as i64);
    let t = fb.select(is_null, t_null, t_undef);
    let t = fb.select(is_bool, t_bool, t);
    let tag = fb.select(is_num, t_num, t);
    let bit = bin_imm(fb, BinaryOp::Band, Type::I64, bits, 1);
    let zero = fb.iconst(Type::I64, 0);
    let p = fb.select(is_bool, bit, zero);
    let payload = fb.select(is_num, bits, p);
    (tag, payload)
}

/// The element storage `v`'s own elements can be read from directly, for bounds-check
/// elimination: `(kind: I32, base: PTR, count: PTR)`. Never misses — `kind` 0 means "no view".
///
/// - kind 2: the numeric mirror while `MIRROR_OK` (`base` = the `f64` array, `count` = its
///   length; a `MIRROR_HOLE` word is a hole, see [`view_elem_num`]);
/// - kind 1: packed storage, boxed or inline (`base` = the first `Property`, `count` = the
///   packed length).
///
/// Only `Exotic::None` / `Exotic::Array` ic-plain objects not mutably borrowed have a view.
/// The view is a snapshot: it stays valid while no JS and no helper runs (only the interpreter
/// reallocates or restructures element storage; the inline element stores only overwrite).
pub(crate) fn array_view(fb: &mut FunctionBuilder, v: IrValue) -> (IrValue, IrValue, IrValue) {
    let join = fb.create_block();
    let kind = fb.append_block_param(join, Type::I32);
    let base = fb.append_block_param(join, PTR);
    let count = fb.append_block_param(join, PTR);
    let none = fb.create_block();
    if let Some(l) = layout() {
        let gc = object(fb, l, v, none, false);
        exotic_is(fb, l, gc, &[Exotic::None, Exotic::Array], none);
        let d = dense(fb, l, gc, none);

        let mirror = fb.create_block();
        let mirror_ok = fb.create_block();
        let packed_b = fb.create_block();
        let flags = fb.load(MemKind::I32U8, d, l.mirror_flags);
        let ok = bin_imm(fb, BinaryOp::Band, Type::I32, flags, MIRROR_OK as i64);
        fb.brif(ok, mirror, &[], packed_b, &[]);
        fb.seal_block(mirror);

        fb.switch_to_block(mirror);
        let mlen = fb.load(PTR_MEM, d, l.mirror_len);
        let some = cmp_imm(fb, IntCC::Ne, PTR, mlen, 0);
        fb.brif(some, mirror_ok, &[], packed_b, &[]);
        fb.seal_block(mirror_ok);
        fb.seal_block(packed_b);

        fb.switch_to_block(mirror_ok);
        let mptr = fb.load(PTR_MEM, d, l.mirror_ptr);
        let two = fb.iconst(Type::I32, 2);
        fb.jump(join, &[two, mptr, mlen]);

        fb.switch_to_block(packed_b);
        let boxed = fb.create_block();
        let not_boxed = fb.create_block();
        let packed = fb.load(PTR_MEM, d, l.dense_packed);
        let is_boxed = cmp_imm(fb, IntCC::Ne, PTR, packed, 0);
        fb.brif(is_boxed, boxed, &[], not_boxed, &[]);
        fb.seal_block(boxed);
        fb.seal_block(not_boxed);

        fb.switch_to_block(boxed);
        let len = fb.load(PTR_MEM, packed, l.packed_len);
        let ptr = fb.load(PTR_MEM, packed, l.packed_ptr);
        let one = fb.iconst(Type::I32, 1);
        fb.jump(join, &[one, ptr, len]);

        // Inline packed storage: a non-zero `u8` length selects it (zero: no packed view).
        fb.switch_to_block(not_boxed);
        let inline = fb.create_block();
        let ilen = fb.load(PTR_U8, d, l.dense_inline_len);
        let is_inline = cmp_imm(fb, IntCC::Ne, PTR, ilen, 0);
        fb.brif(is_inline, inline, &[], none, &[]);
        fb.seal_block(inline);
        fb.switch_to_block(inline);
        let slots = bin_imm(fb, BinaryOp::Iadd, PTR, d, l.dense_inline_slots as i64);
        let one = fb.iconst(Type::I32, 1);
        fb.jump(join, &[one, slots, ilen]);
    } else {
        fb.jump(none, &[]);
    }

    fb.seal_block(none);
    fb.switch_to_block(none);
    let z = fb.iconst(Type::I32, 0);
    let zp = fb.iconst(PTR, 0);
    fb.jump(join, &[z, zp, zp]);
    fb.seal_block(join);
    fb.switch_to_block(join);
    (kind, base, count)
}

/// Element `ii` (`PTR`, known `< count`) of a non-empty [`array_view`] `(kind, base)`, as F64.
/// Misses on a hole, an accessor or a non-Number element.
pub(crate) fn view_elem_num(
    fb: &mut FunctionBuilder,
    kind: IrValue,
    base: IrValue,
    ii: IrValue,
    miss: Block,
) -> IrValue {
    let Some(l) = layout() else {
        always_miss(fb, miss);
        return fb.f64const(0.0);
    };
    let done = fb.create_block();
    let result = fb.append_block_param(done, Type::F64);
    let mirror = fb.create_block();
    let packed = fb.create_block();
    let is_mirror = cmp_imm(fb, IntCC::Eq, Type::I32, kind, 2);
    fb.brif(is_mirror, mirror, &[], packed, &[]);
    fb.seal_block(mirror);
    fb.seal_block(packed);

    fb.switch_to_block(mirror);
    let at = scaled(fb, base, ii, 8);
    let bits = fb.load(MemKind::I64, at, 0);
    let hole = fb.iconst(Type::I64, MIRROR_HOLE as i64);
    let present = fb.icmp(IntCC::Ne, bits, hole);
    guard(fb, present, miss);
    let f = fb.convert(ConvOp::Bitcast, Type::F64, bits);
    fb.jump(done, &[f]);

    fb.switch_to_block(packed);
    let prop = scaled(fb, base, ii, l.prop_size);
    let f = data_num(fb, l, prop, miss);
    fb.jump(done, &[f]);

    fb.seal_block(done);
    fb.switch_to_block(done);
    result
}

/// `v.<prop> = n` (n F64) through the baked IC: overwrite an existing own writable data
/// property of an ordinary object holding a trivially-droppable value. Misses otherwise
/// (setters, frozen / non-writable, shared copy-on-write entries, new properties...). Every
/// guard precedes the store.
pub(crate) fn prop_set_num(
    fb: &mut FunctionBuilder,
    v: IrValue,
    ic: &PropIc,
    n: IrValue,
    miss: Block,
) {
    let Some(l) = layout() else {
        always_miss(fb, miss);
        return;
    };
    let gc = object(fb, l, v, miss, true);
    exotic_is(fb, l, gc, &[Exotic::None], miss);
    let cap = fb.load(PTR_U32, gc, l.entries_cap);
    let owned = cmp_imm(fb, IntCC::Ne, PTR, cap, 0);
    guard(fb, owned, miss);
    let shape = fb.load(MemKind::I32, gc, l.shape);
    let nent = fb.load(PTR_U32, gc, l.entries_len);

    let got = fb.create_block();
    let prop = fb.append_block_param(got, PTR);
    for &(recv_shape, slot) in &ic.ways {
        let hit = fb.create_block();
        let next = fb.create_block();
        let same = cmp_imm(fb, IntCC::Eq, Type::I32, shape, recv_shape as i32 as i64);
        fb.brif(same, hit, &[], next, &[]);
        fb.seal_block(hit);
        fb.seal_block(next);

        fb.switch_to_block(hit);
        let inb = cmp_imm(fb, IntCC::Ugt, PTR, nent, slot as i64);
        guard(fb, inb, miss);
        let base = fb.load(PTR_MEM, gc, l.entries_ptr);
        let sl = fb.iconst(PTR, slot as i64);
        let p = scaled(fb, base, sl, l.prop_size);
        fb.jump(got, &[p]);

        fb.switch_to_block(next);
    }
    fb.jump(miss, &[]);
    fb.seal_block(got);

    fb.switch_to_block(got);
    let meta = fb.load(PTR_MEM, prop, l.prop_meta);
    let attrs = bin_imm(
        fb,
        BinaryOp::Band,
        PTR,
        meta,
        (PROP_ACCESSOR | PROP_WRITABLE) as i64,
    );
    let writable = cmp_imm(fb, IntCC::Eq, PTR, attrs, PROP_WRITABLE as i64);
    guard(fb, writable, miss);
    let old = fb.load(MemKind::I64, prop, l.prop_packed);
    let top = bin_imm(fb, BinaryOp::Ushr, Type::I64, old, 48);
    let empty = cmp_imm(fb, IntCC::Eq, Type::I64, top, TOP_EMPTY);
    let ge = cmp_imm(fb, IntCC::Uge, Type::I64, top, TOP_BIGINT);
    let le = cmp_imm(fb, IntCC::Ule, Type::I64, top, TOP_SYM);
    let rc_prim = fb.binary(BinaryOp::Band, ge, le);
    let obj = cmp_imm(fb, IntCC::Eq, Type::I64, top, TOP_OBJ);
    let bad = fb.binary(BinaryOp::Bor, empty, rc_prim);
    let bad = fb.binary(BinaryOp::Bor, bad, obj);
    let droppable = cmp_imm(fb, IntCC::Eq, Type::I32, bad, 0);
    guard(fb, droppable, miss);
    let nbits = fb.convert(ConvOp::Bitcast, Type::I64, n);
    let is_nan = fb.fcmp(FloatCC::Ne, n, n);
    let canon = fb.iconst(Type::I64, PACK_CANON_NAN as i64);
    let packed = fb.select(is_nan, canon, nbits);
    fb.store(MemKind::I64, prop, packed, l.prop_packed);
}
