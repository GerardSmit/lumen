//! Runtime helpers called from native region code. All are `extern "C"`, take the
//! [`JitFrame`] first, and return a status (`STATUS_OK` / `STATUS_THROW` with the exception in
//! `frame.exception`) unless documented otherwise. `*mut Value` operands point at owned values in
//! `frame.stack` (or `frame.slots`); "consumes" means the helper takes the value out and leaves
//! `Undefined` behind. Result destinations (`dst`) hold a trivially droppable value on entry and
//! receive an owned value.
//!
//! The IR refers to a helper by `ExtFunc::id == Helper as u32`; [`address`] resolves it and
//! [`signature`] gives its IR signature (pointers are I64, `u32` is I32, `f64` is F64).
//!
//! The panic strategy of release builds is `abort`, so a helper must not panic on any input.

#![allow(clippy::missing_safety_doc)]

use super::*;
use lumen_codegen::{Signature, Type};

/// Every helper, by IR id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Helper {
    /// `(frame, slot: u32, dst: *mut Value) -> status` — clone local `slot` into `dst`; throws the
    /// TDZ ReferenceError for `Empty`.
    LoadLocal,
    /// `(frame, slot: u32, src: *mut Value)` — move `*src` into local `slot` (dropping the old
    /// value; `src` left `Undefined`). No status (never throws).
    StoreLocal,
    /// `(frame, slot: u32, n: f64)` — store `Num(n)` into local `slot`, dropping the old value.
    StoreLocalNum,
    /// `(v: *mut Value)` — drop `*v` in place, leave `Undefined`. No frame, no status.
    Drop,
    /// `(dst: *mut Value, src: *const Value)` — `*dst = (*src).clone()` (`dst` trivial). No
    /// frame, no status.
    Clone,
    /// `(frame, op: u32, a: *mut Value, b: *mut Value, dst: *mut Value) -> status` — generic
    /// binary operator `op` (an [`crate::bytecode::Op`] discriminant-independent code: see
    /// [`binary_op_code`]) on consumed `a`, `b`; result into `dst`.
    Binary,
    /// `(frame, v: *mut Value) -> u32` — ToBoolean of `*v` (consumed), 0/1. Never throws.
    ToBoolean,
    /// `(frame, pc: u32, depth: u32) -> status` — the generic fallback: run the single
    /// interpreter op at `pc` with `frame.stack[0..depth]` (all Boxed) as its operand stack; the
    /// results are left at `frame.stack[depth - pops ..]`. Only for ops that do not touch
    /// locals, do not jump and do not push/pop handlers (see [`generic_ok`]).
    Generic,
}

pub(crate) const ALL: [Helper; 8] = [
    Helper::LoadLocal,
    Helper::StoreLocal,
    Helper::StoreLocalNum,
    Helper::Drop,
    Helper::Clone,
    Helper::Binary,
    Helper::ToBoolean,
    Helper::Generic,
];

/// The IR signature of `h`.
pub(crate) fn signature(h: Helper) -> Signature {
    use Type::*;
    let (p, r): (&[Type], &[Type]) = match h {
        Helper::LoadLocal => (&[I64, I32, I64], &[I32]),
        Helper::StoreLocal => (&[I64, I32, I64], &[]),
        Helper::StoreLocalNum => (&[I64, I32, F64], &[]),
        Helper::Drop => (&[I64], &[]),
        Helper::Clone => (&[I64, I64], &[]),
        Helper::Binary => (&[I64, I32, I64, I64, I64], &[I32]),
        Helper::ToBoolean => (&[I64, I64], &[I32]),
        Helper::Generic => (&[I64, I32, I32], &[I32]),
    };
    Signature::new(p.to_vec(), r.to_vec())
}

/// The address of helper `id`.
pub(crate) fn address(id: u32) -> Option<u64> {
    let h = *ALL.get(id as usize)?;
    Some(match h {
        Helper::LoadLocal => load_local as usize,
        Helper::StoreLocal => store_local as usize,
        Helper::StoreLocalNum => store_local_num as usize,
        Helper::Drop => drop_value as usize,
        Helper::Clone => clone_value as usize,
        Helper::Binary => binary as usize,
        Helper::ToBoolean => to_boolean as usize,
        Helper::Generic => generic as usize,
    } as u64)
}

/// The operator code [`Helper::Binary`] takes for a binary bytecode op (`Add`..`StrictNotEq`,
/// `InstanceOf` excluded), or `None`.
pub(crate) fn binary_op_code(op: &crate::bytecode::Op) -> Option<u32> {
    let _ = op;
    todo!("helpers agent")
}

/// Whether `op` may run through [`Helper::Generic`].
pub(crate) fn generic_ok(op: &crate::bytecode::Op) -> bool {
    let _ = op;
    todo!("helpers agent")
}

pub(crate) unsafe extern "C" fn load_local(f: *mut JitFrame, slot: u32, dst: *mut Value) -> u32 {
    let _ = (f, slot, dst);
    todo!("helpers agent")
}
pub(crate) unsafe extern "C" fn store_local(f: *mut JitFrame, slot: u32, src: *mut Value) {
    let _ = (f, slot, src);
    todo!("helpers agent")
}
pub(crate) unsafe extern "C" fn store_local_num(f: *mut JitFrame, slot: u32, n: f64) {
    let _ = (f, slot, n);
    todo!("helpers agent")
}
pub(crate) unsafe extern "C" fn drop_value(v: *mut Value) {
    let _ = v;
    todo!("helpers agent")
}
pub(crate) unsafe extern "C" fn clone_value(dst: *mut Value, src: *const Value) {
    let _ = (dst, src);
    todo!("helpers agent")
}
pub(crate) unsafe extern "C" fn binary(
    f: *mut JitFrame,
    op: u32,
    a: *mut Value,
    b: *mut Value,
    dst: *mut Value,
) -> u32 {
    let _ = (f, op, a, b, dst);
    todo!("helpers agent")
}
pub(crate) unsafe extern "C" fn to_boolean(f: *mut JitFrame, v: *mut Value) -> u32 {
    let _ = (f, v);
    todo!("helpers agent")
}
pub(crate) unsafe extern "C" fn generic(f: *mut JitFrame, pc: u32, depth: u32) -> u32 {
    let _ = (f, pc, depth);
    todo!("helpers agent")
}
