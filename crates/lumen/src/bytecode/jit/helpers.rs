//! Runtime helpers called from native region code. All are `extern "C"`, take the
//! [`JitFrame`] first, and return a status (`STATUS_OK` / `STATUS_THROW` with the exception in
//! `frame.exception`) unless documented otherwise. `*mut Value` operands point at owned values in
//! `frame.stack` (or `frame.slots`); "consumes" means the helper takes the value out and leaves
//! `Undefined` behind. Result destinations (`dst`) hold a trivially droppable value on entry and
//! receive an owned value.
//!
//! The IR refers to a helper by `ExtFunc::id == Helper as u32`; [`address`] resolves it and
//! [`signature`] gives its IR signature (pointers are [`PTR`], `u32` is I32, `f64` is F64).
//!
//! The panic strategy of release builds is `abort`, so a helper must not panic on any input.
//!
//! # GC
//!
//! The collector (`Interp::gc_collect`) is a refcount-based cycle collector with no root
//! enumeration: an object is a root when its `Rc::strong_count` exceeds the references other heap
//! nodes hold to it. Every Boxed value native code keeps in `frame.slots` / `frame.stack` is an
//! owned `Value` (a counted reference), so it roots its object exactly as the interpreter's own
//! `slots` / `stack` `Vec`s do — nothing needs registering. The one rule native code must keep is
//! that a Boxed value is never duplicated bitwise: a second copy goes through [`clone_value`]
//! (and a discarded one through [`drop_value`]), or the count — and so the root test — is wrong.

#![allow(clippy::missing_safety_doc)]

use super::*;
use crate::bytecode::Op;
use crate::interpreter::Abrupt;
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
    /// `(frame) -> status` — the loop safepoint: resets `frame.budget` to [`SAFEPOINT_BUDGET`]
    /// and runs the collector / interrupt check the interpreter runs at backward jumps.
    Safepoint,
}

pub(crate) const ALL: [Helper; 9] = [
    Helper::LoadLocal,
    Helper::StoreLocal,
    Helper::StoreLocalNum,
    Helper::Drop,
    Helper::Clone,
    Helper::Binary,
    Helper::ToBoolean,
    Helper::Generic,
    Helper::Safepoint,
];

/// The IR signature of `h`.
pub(crate) fn signature(h: Helper) -> Signature {
    use Type::*;
    const P: Type = PTR;
    let (p, r): (&[Type], &[Type]) = match h {
        Helper::LoadLocal => (&[P, I32, P], &[I32]),
        Helper::StoreLocal => (&[P, I32, P], &[]),
        Helper::StoreLocalNum => (&[P, I32, F64], &[]),
        Helper::Drop => (&[P], &[]),
        Helper::Clone => (&[P, P], &[]),
        Helper::Binary => (&[P, I32, P, P, P], &[I32]),
        Helper::ToBoolean => (&[P, P], &[I32]),
        Helper::Generic => (&[P, I32, I32], &[I32]),
        Helper::Safepoint => (&[P], &[I32]),
    };
    Signature::new(p.to_vec(), r.to_vec())
}

/// The address of helper `id`.
pub(crate) fn address(id: u32) -> Option<u64> {
    let h = *ALL.get(id as usize)?;
    Some(match h {
        Helper::LoadLocal => load_local as *const () as usize,
        Helper::StoreLocal => store_local as *const () as usize,
        Helper::StoreLocalNum => store_local_num as *const () as usize,
        Helper::Drop => drop_value as *const () as usize,
        Helper::Clone => clone_value as *const () as usize,
        Helper::Binary => binary as *const () as usize,
        Helper::ToBoolean => to_boolean as *const () as usize,
        Helper::Generic => generic as *const () as usize,
        Helper::Safepoint => safepoint as *const () as usize,
    } as u64)
}

// Stable operator codes for `Helper::Binary`, independent of `Op`'s discriminants.
const BIN_ADD: u32 = 0;
const BIN_SUB: u32 = 1;
const BIN_MUL: u32 = 2;
const BIN_DIV: u32 = 3;
const BIN_MOD: u32 = 4;
const BIN_BITAND: u32 = 5;
const BIN_BITOR: u32 = 6;
const BIN_BITXOR: u32 = 7;
const BIN_SHL: u32 = 8;
const BIN_SHR: u32 = 9;
const BIN_USHR: u32 = 10;
const BIN_LT: u32 = 11;
const BIN_GT: u32 = 12;
const BIN_LE: u32 = 13;
const BIN_GE: u32 = 14;
const BIN_EQEQ: u32 = 15;
const BIN_NOTEQ: u32 = 16;
const BIN_STRICTEQ: u32 = 17;
const BIN_STRICTNOTEQ: u32 = 18;

/// The operator code [`Helper::Binary`] takes for a binary bytecode op (`Add`..`StrictNotEq`,
/// `InstanceOf` excluded), or `None`. `GenBin` is excluded too: its operator lives in the chunk's
/// name table, and it runs through [`Helper::Generic`] instead.
pub(crate) fn binary_op_code(op: &Op) -> Option<u32> {
    Some(match op {
        Op::Add => BIN_ADD,
        Op::Sub => BIN_SUB,
        Op::Mul => BIN_MUL,
        Op::Div => BIN_DIV,
        Op::Mod => BIN_MOD,
        Op::BitAnd => BIN_BITAND,
        Op::BitOr => BIN_BITOR,
        Op::BitXor => BIN_BITXOR,
        Op::Shl => BIN_SHL,
        Op::Shr => BIN_SHR,
        Op::UShr => BIN_USHR,
        Op::Lt => BIN_LT,
        Op::Gt => BIN_GT,
        Op::Le => BIN_LE,
        Op::Ge => BIN_GE,
        Op::EqEq => BIN_EQEQ,
        Op::NotEq => BIN_NOTEQ,
        Op::StrictEq => BIN_STRICTEQ,
        Op::StrictNotEq => BIN_STRICTNOTEQ,
        _ => return None,
    })
}

/// The `Interp::binary` operator string for a [`binary_op_code`] code.
fn binary_op_str(code: u32) -> Option<&'static str> {
    Some(match code {
        BIN_ADD => "+",
        BIN_SUB => "-",
        BIN_MUL => "*",
        BIN_DIV => "/",
        BIN_MOD => "%",
        BIN_BITAND => "&",
        BIN_BITOR => "|",
        BIN_BITXOR => "^",
        BIN_SHL => "<<",
        BIN_SHR => ">>",
        BIN_USHR => ">>>",
        BIN_LT => "<",
        BIN_GT => ">",
        BIN_LE => "<=",
        BIN_GE => ">=",
        BIN_EQEQ => "==",
        BIN_NOTEQ => "!=",
        BIN_STRICTEQ => "===",
        BIN_STRICTNOTEQ => "!==",
        _ => return None,
    })
}

/// The Number ⊕ Number result `run_vm` computes inline for `code` (`bin_num` / `bin_i32` /
/// `bin_cmp` / its `UShr` arm), bit for bit.
fn binary_num(code: u32, x: f64, y: f64) -> Option<Value> {
    use crate::eval::{js_mod, to_int32};
    let (ix, iy) = (to_int32(x), to_int32(y));
    Some(match code {
        BIN_ADD => Value::Num(x + y),
        BIN_SUB => Value::Num(x - y),
        BIN_MUL => Value::Num(x * y),
        BIN_DIV => Value::Num(x / y),
        BIN_MOD => Value::Num(js_mod(x, y)),
        BIN_BITAND => Value::Num((ix & iy) as f64),
        BIN_BITOR => Value::Num((ix | iy) as f64),
        BIN_BITXOR => Value::Num((ix ^ iy) as f64),
        BIN_SHL => Value::Num(ix.wrapping_shl(iy as u32 & 31) as f64),
        BIN_SHR => Value::Num((ix >> (iy as u32 & 31)) as f64),
        BIN_USHR => Value::Num(((ix as u32) >> (iy as u32 & 31)) as f64),
        BIN_LT => Value::Bool(x < y),
        BIN_GT => Value::Bool(x > y),
        BIN_LE => Value::Bool(x <= y),
        BIN_GE => Value::Bool(x >= y),
        BIN_EQEQ | BIN_STRICTEQ => Value::Bool(x == y),
        BIN_NOTEQ | BIN_STRICTNOTEQ => Value::Bool(x != y),
        _ => return None,
    })
}

/// Whether `op` may run through [`Helper::Generic`]: it neither reads nor writes a frame slot
/// (native code may hold locals in SSA, so `frame.slots` can be stale), never moves `pc` (no
/// jumps, returns, awaits or handler pushes/pops — `generic` runs it with an empty handler
/// stack), and depends on nothing but its operands, the chunk, the env and `this`. `Throw` is in:
/// its exception comes back as `STATUS_THROW` like any other throw. Captured variables
/// (`*Cap`) live in the activation env, not in slots, so they are fine.
pub(crate) fn generic_ok(op: &Op) -> bool {
    use Op::*;
    match *op {
        Const(_) | Undef | Dup | Dup2 | Pop | LoadCap(_) | StoreCap(_) | StoreCapInit(_)
        | UpdateCap(..) | UpdateName(..) | UpdateNameCached(..) | MakeClosure(..)
        | LoadName(..) | LoadNameForCall(..) | StoreName(_) | StoreNameCached(..) | LoadThis
        | LoadLexicalThis | GetProp(..) | GetPropThis(..) | SetProp(..) | SetPropDrop(..)
        | SetPropThisDrop(..) | ToStr | GetIter | ForInKeys | DestructureGuard
        | DestructureArr(_) | DeleteProp(..) | DeleteElem(_) | CallSpread(_)
        | CallSpreadThis(_) | AppendProp(..) | GetElem | SetElem | SetElemDrop | UpdateProp(..)
        | UpdateElem(_) | ToPropKey | GetMethod(..) | GetMethodElem | Add | Sub | Mul | Div
        | Mod | BitAnd | BitOr | BitXor | Shl | Shr | UShr | Lt | Gt | Le | Ge | EqEq | NotEq
        | StrictEq | StrictNotEq | InstanceOf(_) | GenBin(_) | Neg | Plus | Not | BitNot
        | Typeof | TypeofIs(..) | TypeofName(_) | Void | Call(_) | CallWithThis(_) | New(_)
        | MakeRegExp(..) | MakeArray(_) | MakeObject(..) | Throw => true,
        // Slot readers/writers, control flow, and handler bookkeeping.
        LoadLocal(_)
        | StoreLocal(_)
        | UpdateLocal(..)
        | Tdz(_)
        | GetPropLocal(..)
        | SetPropLocalDrop(..)
        | GetElemLocal(_)
        | SetElemLocal(_)
        | SetElemLocalDrop(_)
        | ToPropKeyLocal(_)
        | ForInStepL(..)
        | IterStepL(..)
        | IterCloseL(_)
        | IterAbortL(_)
        | ArithLL(..)
        | ArithLK(..)
        | Jump(_)
        | JumpIfFalse(_)
        | JumpIfNotCmp(..)
        | JumpIfNotCmpLL(..)
        | JumpIfNotCmpLK(..)
        | JumpIfFalsePeek(_)
        | JumpIfTruePeek(_)
        | JumpIfNotNullishPeek(_)
        | Return
        | ReturnUndef
        | Await
        | PushHandler(_)
        | PopHandler => false,
    }
}

/// Record a completion's abrupt outcome in the frame. A compiled body only ever sees `Throw`;
/// anything else would be an engine bug, surfaced as a thrown error rather than an abort.
unsafe fn fail(f: *mut JitFrame, a: Abrupt) -> u32 {
    let e = match a {
        Abrupt::Throw(e) => e,
        _ => match (*(*f).interp).throw("TypeError", "unexpected completion in compiled code") {
            Abrupt::Throw(e) => e,
            _ => Value::Undefined,
        },
    };
    (*f).exception = e;
    STATUS_THROW
}

unsafe fn tdz_error(f: *mut JitFrame, slot: usize) -> u32 {
    let chunk = &*(*f).chunk;
    let name = chunk.slot_names.get(slot).map(|n| &**n).unwrap_or("");
    let e = (*(*f).interp).throw(
        "ReferenceError",
        format!("cannot access '{name}' before initialization"),
    );
    fail(f, e)
}

pub(crate) unsafe extern "C" fn load_local(f: *mut JitFrame, slot: u32, dst: *mut Value) -> u32 {
    let slot = slot as usize;
    if slot >= (*(*f).chunk).n_slots {
        let e = (*(*f).interp).throw("TypeError", "compiled code read a missing local");
        return fail(f, e);
    }
    let v = &*(*f).slots.add(slot);
    if matches!(v, Value::Empty) {
        return tdz_error(f, slot);
    }
    std::ptr::write(dst, v.clone());
    STATUS_OK
}

pub(crate) unsafe extern "C" fn store_local(f: *mut JitFrame, slot: u32, src: *mut Value) {
    let v = std::ptr::replace(src, Value::Undefined);
    if (slot as usize) < (*(*f).chunk).n_slots {
        *(*f).slots.add(slot as usize) = v;
    }
}

pub(crate) unsafe extern "C" fn store_local_num(f: *mut JitFrame, slot: u32, n: f64) {
    if (slot as usize) < (*(*f).chunk).n_slots {
        *(*f).slots.add(slot as usize) = Value::Num(n);
    }
}

pub(crate) unsafe extern "C" fn drop_value(v: *mut Value) {
    *v = Value::Undefined;
}

pub(crate) unsafe extern "C" fn clone_value(dst: *mut Value, src: *const Value) {
    std::ptr::write(dst, (*src).clone());
}

pub(crate) unsafe extern "C" fn binary(
    f: *mut JitFrame,
    op: u32,
    a: *mut Value,
    b: *mut Value,
    dst: *mut Value,
) -> u32 {
    let a = std::ptr::replace(a, Value::Undefined);
    let b = std::ptr::replace(b, Value::Undefined);
    // The interpreter's inline Number fast paths first, so results match `run_vm` exactly.
    if let (Value::Num(x), Value::Num(y)) = (&a, &b) {
        if let Some(v) = binary_num(op, *x, *y) {
            std::ptr::write(dst, v);
            return STATUS_OK;
        }
    }
    let i = &mut *(*f).interp;
    let r = match binary_op_str(op) {
        Some(s) => i.binary(s, a, b),
        None => Err(i.throw("TypeError", "compiled code used an unknown operator")),
    };
    match r {
        Ok(v) => {
            std::ptr::write(dst, v);
            STATUS_OK
        }
        Err(e) => fail(f, e),
    }
}

pub(crate) unsafe extern "C" fn to_boolean(f: *mut JitFrame, v: *mut Value) -> u32 {
    let v = std::ptr::replace(v, Value::Undefined);
    (*(*f).interp).to_boolean(&v) as u32
}

pub(crate) unsafe extern "C" fn generic(f: *mut JitFrame, pc: u32, depth: u32) -> u32 {
    let chunk = &*(*f).chunk;
    let pc = pc as usize;
    let depth = depth as usize;
    if chunk.ops.get(pc).is_none_or(|op| !generic_ok(op)) {
        let e = (*(*f).interp).throw("TypeError", "compiled code ran an unsupported op");
        return fail(f, e);
    }
    let i = &mut *(*f).interp;
    let env = &*(*f).env;
    let this_val = &*(*f).this_val;
    let slots = std::slice::from_raw_parts_mut((*f).slots, chunk.n_slots);
    // A pooled buffer: this runs once per generic op on a hot loop's slow path.
    let (spare, mut stack) = i.vm_pool.pop().unwrap_or_default();
    stack.clear();
    for d in 0..depth {
        stack.push(std::ptr::replace((*f).stack.add(d), Value::Undefined));
    }
    let mut next_pc = pc;
    let mut handlers = Vec::new();
    let r = crate::bytecode::run_vm::<true>(
        i,
        chunk,
        env,
        slots,
        &mut stack,
        &mut next_pc,
        this_val,
        &mut handlers,
    );
    // Hand the stack back — on a throw too: the entries below the op's operands are untouched
    // (ops only pop from the top, and calls truncate only after returning), and native code
    // still owns them as Boxed entries. `frame.stack[0..depth]` is all `Undefined` here, so a
    // plain write neither leaks nor double-drops. On success the op's pushes fit in the region's
    // `max_stack` (the builder sized it from the same stack effects); on a throw anything
    // above the original depth is dropped rather than written past what the caller expects.
    let keep = if r.is_ok() {
        stack.len()
    } else {
        stack.len().min(depth)
    };
    for (d, v) in stack.drain(..).take(keep).enumerate() {
        std::ptr::write((*f).stack.add(d), v);
    }
    if i.vm_pool.len() < 64 {
        i.vm_pool.push((spare, stack));
    }
    match r {
        Ok(_) => STATUS_OK,
        Err(e) => fail(f, e),
    }
}

pub(crate) unsafe extern "C" fn safepoint(f: *mut JitFrame) -> u32 {
    (*f).budget = SAFEPOINT_BUDGET;
    // Native code already amortized (the budget), so this is the full check the interpreter's
    // `gc_check_amortized` falls through to.
    match (*(*f).interp).gc_check() {
        Ok(()) => STATUS_OK,
        Err(e) => fail(f, e),
    }
}
