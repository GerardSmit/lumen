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
    /// `(frame, pc: u32, base: u32, depth: u32) -> status` — the generic fallback: run the
    /// single interpreter op at `pc` with `frame.stack[base..depth]` (all Boxed; `base` at or
    /// below the op's operands) as its operand stack; the results are left at
    /// `frame.stack[base..]`. Only for ops that do not write locals, do not jump and do not
    /// push/pop handlers (see [`generic_ok`]), or that read a Boxed local (see
    /// [`generic_slot_ok`]: the translator only sends those when the slot's memory is
    /// authoritative).
    Generic,
    /// `(frame) -> status` — the loop safepoint: resets `frame.budget` to [`SAFEPOINT_BUDGET`]
    /// and runs the collector / interrupt check the interpreter runs at backward jumps.
    Safepoint,
    /// `(frame, n: u32, c: u32) -> *const Value` — the address of the value of the binding the
    /// free name `names[n]` (name cache `c`) resolves to, when the name cache proves it is a
    /// plain initialized scope binding (filling the cache first on a miss); null otherwise.
    /// Pure: never runs JS, never throws. The address stays valid until JS runs (only JS
    /// restructures a scope or reassigns a binding).
    NamePtr,
    /// `(frame, n: u32) -> *mut Value` — the address of captured binding `names[n]`'s value in
    /// the activation env, or null while it is in its TDZ. Pure, like [`Helper::NamePtr`].
    CapPtr,
    /// `(frame, n: u32, c: u32, dst) -> u32` — `LoadName(n, c)` into `dst`: 0 = done through the
    /// name cache (no JS ran), 2 = done through the full lookup (JS may have run), 1 = threw.
    LoadName,
    /// `(frame, n: u32, c: u32, dst) -> u32` — `LoadNameForCall(n, c)`: the receiver into
    /// `dst[0]` and the callee into `dst[1]`; statuses as [`Helper::LoadName`].
    LoadNameForCall,
    /// `(frame, n: u32, c: u32, src) -> status` — `StoreNameCached(n, c)` of the consumed `src`.
    StoreName,
    /// `(frame, n: u32, src, init: u32) -> status` — `StoreCap(n)` (`StoreCapInit` when `init`)
    /// of the consumed `src`.
    StoreCap,
    /// `(frame, base: u32, argc: u32, with_this: u32) -> status` — `Call(argc)` (receiver
    /// `undefined`, callee at `stack[base]`) or `CallWithThis(argc)` (receiver at `stack[base]`,
    /// callee at `stack[base + 1]`), arguments following. Consumes all of them and leaves the
    /// result at `stack[base]` (`Undefined` there on a throw).
    Call,
    /// `(frame, n: u32, c: u32, obj, consume: u32, dst) -> status` — `obj.<names[n]>` through
    /// property cache `c` into `dst`; `obj` is dropped afterwards when `consume`.
    GetProp,
    /// `(frame, n: u32, c: u32, obj, dst) -> status` — `GetMethod`: like `GetProp`, `obj` kept.
    GetMethod,
    /// `(frame, n: u32, c: u32, obj, v, consume: u32) -> status` — `obj.<names[n]> = v` through
    /// property cache `c`, consuming `v` (and `obj` when `consume`).
    SetProp,
    /// `(frame, pc: u32) -> u32` — 1 when the `LoadName(Math) GetMethod(f)` pair at `pc` still
    /// yields the realm's intrinsic `Math.f` (see [`math_intrinsic`]). Pure; a failure is
    /// remembered for the site ([`math_site_failed`]) so a recompile stops speculating.
    MathGuard,
}

pub(crate) const ALL: [Helper; 20] = [
    Helper::LoadLocal,
    Helper::StoreLocal,
    Helper::StoreLocalNum,
    Helper::Drop,
    Helper::Clone,
    Helper::Binary,
    Helper::ToBoolean,
    Helper::Generic,
    Helper::Safepoint,
    Helper::NamePtr,
    Helper::CapPtr,
    Helper::LoadName,
    Helper::LoadNameForCall,
    Helper::StoreName,
    Helper::StoreCap,
    Helper::Call,
    Helper::GetProp,
    Helper::GetMethod,
    Helper::SetProp,
    Helper::MathGuard,
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
        Helper::Generic => (&[P, I32, I32, I32], &[I32]),
        Helper::Safepoint => (&[P], &[I32]),
        Helper::NamePtr => (&[P, I32, I32], &[P]),
        Helper::CapPtr => (&[P, I32], &[P]),
        Helper::LoadName | Helper::LoadNameForCall => (&[P, I32, I32, P], &[I32]),
        Helper::StoreName => (&[P, I32, I32, P], &[I32]),
        Helper::StoreCap => (&[P, I32, P, I32], &[I32]),
        Helper::Call => (&[P, I32, I32, I32], &[I32]),
        Helper::GetProp => (&[P, I32, I32, P, I32, P], &[I32]),
        Helper::GetMethod => (&[P, I32, I32, P, P], &[I32]),
        Helper::SetProp => (&[P, I32, I32, P, P, I32], &[I32]),
        Helper::MathGuard => (&[P, I32], &[I32]),
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
        Helper::NamePtr => name_ptr as *const () as usize,
        Helper::CapPtr => cap_ptr as *const () as usize,
        Helper::LoadName => load_name as *const () as usize,
        Helper::LoadNameForCall => load_name_for_call as *const () as usize,
        Helper::StoreName => store_name as *const () as usize,
        Helper::StoreCap => store_cap as *const () as usize,
        Helper::Call => call as *const () as usize,
        Helper::GetProp => get_prop as *const () as usize,
        Helper::GetMethod => get_method as *const () as usize,
        Helper::SetProp => set_prop as *const () as usize,
        Helper::MathGuard => math_guard as *const () as usize,
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

/// Slot-reading ops [`Helper::Generic`] may run when the translator knows the slot is Boxed —
/// its memory is then authoritative. None of them writes a slot.
pub(crate) fn generic_slot_ok(op: &Op) -> bool {
    use Op::*;
    matches!(
        *op,
        GetPropLocal(..)
            | SetPropLocalDrop(..)
            | GetElemLocal(_)
            | SetElemLocal(_)
            | SetElemLocalDrop(_)
            | ToPropKeyLocal(_)
    )
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

pub(crate) unsafe extern "C" fn generic(f: *mut JitFrame, pc: u32, base: u32, depth: u32) -> u32 {
    let chunk = &*(*f).chunk;
    let pc = pc as usize;
    let (base, depth) = (base as usize, depth as usize);
    if base > depth
        || chunk
            .ops
            .get(pc)
            .is_none_or(|op| !generic_ok(op) && !generic_slot_ok(op))
    {
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
    for d in base..depth {
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
    // still owns them as Boxed entries. `frame.stack[base..depth]` is all `Undefined` here, so
    // a plain write neither leaks nor double-drops. On success the op's pushes fit in the
    // region's `max_stack` (the builder sized it from the same stack effects); on a throw
    // anything above the original depth is dropped rather than written past what the caller
    // expects.
    let keep = if r.is_ok() {
        stack.len()
    } else {
        stack.len().min(depth - base)
    };
    for (d, v) in stack.drain(..).take(keep).enumerate() {
        std::ptr::write((*f).stack.add(base + d), v);
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

// ---- names, captures, calls and properties ---------------------------------------------------

/// The binding a name cache resolves to through a scope (the depth-0 and depth-1 modes of
/// [`crate::bytecode::NameIc`]), revalidated exactly like `Chunk::name_ic_hit` does.
unsafe fn name_binding(
    chunk: &Chunk,
    env: &Env,
    c: usize,
) -> Option<*mut crate::interpreter::Binding> {
    let ic = chunk.name_caches.get(c)?.get();
    if ic.env == 0 || ic.env & 1 != 0 {
        return None;
    }
    let raw = std::rc::Rc::as_ptr(env) as usize;
    if ic.env == raw {
        let b = env.try_borrow().ok()?;
        if b.vars.generation() != ic.gen {
            return None;
        }
        return Some(ic.binding as usize as *mut crate::interpreter::Binding);
    }
    if ic.env & 2 != 0 {
        let b = env.try_borrow().ok()?;
        if b.vars.generation() != ic.act_gen {
            return None;
        }
        let p = b.parent.as_ref()?;
        if std::rc::Rc::as_ptr(p) as usize | 2 != ic.env {
            return None;
        }
        let pb = p.try_borrow().ok()?;
        if pb.vars.generation() != ic.gen {
            return None;
        }
        return Some(ic.binding as usize as *mut crate::interpreter::Binding);
    }
    None
}

pub(crate) unsafe extern "C" fn name_ptr(f: *mut JitFrame, n: u32, c: u32) -> *const Value {
    let chunk = &*(*f).chunk;
    let env = &*(*f).env;
    if c as usize >= chunk.name_caches.len() || n as usize >= chunk.names.len() {
        return std::ptr::null();
    }
    let mut b = name_binding(chunk, env, c as usize);
    if b.is_none() {
        // Side-effect free: seeds the cache from plain data bindings / properties only.
        let i = &*(*f).interp;
        if chunk.name_ic_fill(i, env, n, c).is_some() {
            b = name_binding(chunk, env, c as usize);
        }
    }
    match b {
        Some(bd) if (*bd).initialized && (*bd).import_ref.is_none() => &(*bd).value,
        _ => std::ptr::null(),
    }
}

pub(crate) unsafe extern "C" fn cap_ptr(f: *mut JitFrame, n: u32) -> *mut Value {
    let chunk = &*(*f).chunk;
    if n as usize >= chunk.names.len() {
        return std::ptr::null_mut();
    }
    let env = &*(*f).env;
    let bd = chunk.cap_binding_ptr(env, n);
    if (*bd).initialized {
        &mut (*bd).value
    } else {
        std::ptr::null_mut()
    }
}

/// Status of a name read that went through the full (possibly JS-running) lookup.
pub(crate) const STATUS_OK_SLOW: u32 = 2;

pub(crate) unsafe extern "C" fn load_name(
    f: *mut JitFrame,
    n: u32,
    c: u32,
    dst: *mut Value,
) -> u32 {
    let chunk = &*(*f).chunk;
    let env = &*(*f).env;
    let i = &mut *(*f).interp;
    if let Some(v) = chunk
        .name_ic_hit(i, env, c)
        .or_else(|| chunk.name_ic_fill(i, env, n, c))
    {
        std::ptr::write(dst, v);
        return STATUS_OK;
    }
    match i.get_var(&chunk.names[n as usize], env) {
        Ok(v) => {
            std::ptr::write(dst, v);
            STATUS_OK_SLOW
        }
        Err(e) => fail(f, e),
    }
}

pub(crate) unsafe extern "C" fn load_name_for_call(
    f: *mut JitFrame,
    n: u32,
    c: u32,
    dst: *mut Value,
) -> u32 {
    let chunk = &*(*f).chunk;
    let env = &*(*f).env;
    let i = &mut *(*f).interp;
    // A depth-0 cache hit/fill can't have come through a `with` object: `this` is undefined.
    if let Some(v) = chunk
        .name_ic_hit(i, env, c)
        .or_else(|| chunk.name_ic_fill(i, env, n, c))
    {
        std::ptr::write(dst, Value::Undefined);
        std::ptr::write(dst.add(1), v);
        return STATUS_OK;
    }
    match i.get_var_with(&chunk.names[n as usize], env) {
        Ok((callee, with_this)) => {
            std::ptr::write(dst, with_this.unwrap_or(Value::Undefined));
            std::ptr::write(dst.add(1), callee);
            STATUS_OK_SLOW
        }
        Err(e) => fail(f, e),
    }
}

pub(crate) unsafe extern "C" fn store_name(
    f: *mut JitFrame,
    n: u32,
    c: u32,
    src: *mut Value,
) -> u32 {
    let chunk = &*(*f).chunk;
    let env = &*(*f).env;
    let v = std::ptr::replace(src, Value::Undefined);
    match chunk.store_name_ic(&mut *(*f).interp, env, n, c, v) {
        Ok(()) => STATUS_OK,
        Err(e) => fail(f, e),
    }
}

pub(crate) unsafe extern "C" fn store_cap(
    f: *mut JitFrame,
    n: u32,
    src: *mut Value,
    init: u32,
) -> u32 {
    let chunk = &*(*f).chunk;
    let env = &*(*f).env;
    let v = std::ptr::replace(src, Value::Undefined);
    match chunk.store_cap_ic(&mut *(*f).interp, env, n, v, init != 0) {
        Ok(()) => STATUS_OK,
        Err(e) => fail(f, e),
    }
}

pub(crate) unsafe extern "C" fn call(
    f: *mut JitFrame,
    base: u32,
    argc: u32,
    with_this: u32,
) -> u32 {
    let s = (*f).stack;
    let (base, argc) = (base as usize, argc as usize);
    let (this, callee, at) = if with_this != 0 {
        (
            std::ptr::replace(s.add(base), Value::Undefined),
            std::ptr::replace(s.add(base + 1), Value::Undefined),
            base + 2,
        )
    } else {
        (
            Value::Undefined,
            std::ptr::replace(s.add(base), Value::Undefined),
            base + 1,
        )
    };
    // The arguments stay owned by the frame's stack for the call (they root their objects
    // there, like the interpreter's operand stack does), then are dropped.
    let args = std::slice::from_raw_parts(s.add(at), argc);
    let r = (*(*f).interp).call(callee, this, args);
    for k in 0..argc {
        *s.add(at + k) = Value::Undefined;
    }
    match r {
        Ok(v) => {
            std::ptr::write(s.add(base), v);
            STATUS_OK
        }
        Err(e) => fail(f, e),
    }
}

pub(crate) unsafe extern "C" fn get_prop(
    f: *mut JitFrame,
    n: u32,
    c: u32,
    obj: *mut Value,
    consume: u32,
    dst: *mut Value,
) -> u32 {
    let chunk = &*(*f).chunk;
    let (Some(name), Some(cache)) = (chunk.names.get(n as usize), chunk.caches.get(c as usize))
    else {
        let e = (*(*f).interp).throw("TypeError", "compiled code used a bad property cache");
        return fail(f, e);
    };
    let r = (*(*f).interp).get_prop_ic(&*obj, name, cache);
    if consume != 0 {
        *obj = Value::Undefined;
    }
    match r {
        Ok(v) => {
            std::ptr::write(dst, v);
            STATUS_OK
        }
        Err(e) => fail(f, e),
    }
}

pub(crate) unsafe extern "C" fn get_method(
    f: *mut JitFrame,
    n: u32,
    c: u32,
    obj: *mut Value,
    dst: *mut Value,
) -> u32 {
    get_prop(f, n, c, obj, 0, dst)
}

pub(crate) unsafe extern "C" fn set_prop(
    f: *mut JitFrame,
    n: u32,
    c: u32,
    obj: *mut Value,
    v: *mut Value,
    consume: u32,
) -> u32 {
    let chunk = &*(*f).chunk;
    let v = std::ptr::replace(v, Value::Undefined);
    let (Some(name), Some(cache)) = (chunk.names.get(n as usize), chunk.caches.get(c as usize))
    else {
        let e = (*(*f).interp).throw("TypeError", "compiled code used a bad property cache");
        return fail(f, e);
    };
    let r = (*(*f).interp).set_prop_ic(&*obj, name, v, cache);
    if consume != 0 {
        *obj = Value::Undefined;
    }
    match r {
        Ok(()) => STATUS_OK,
        Err(e) => fail(f, e),
    }
}

// ---- Math intrinsics ---------------------------------------------------------------------------

/// A `Math` function the translator emits inline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MathFn {
    Sqrt,
    Abs,
    Floor,
    Ceil,
    Round,
    Trunc,
    Max,
    Min,
    Imul,
}

impl MathFn {
    pub(crate) fn arity(self) -> usize {
        match self {
            MathFn::Max | MathFn::Min | MathFn::Imul => 2,
            _ => 1,
        }
    }
}

/// The inlinable `Math` function called `name`.
pub(crate) fn math_intrinsic(name: &str) -> Option<MathFn> {
    Some(match name {
        "sqrt" => MathFn::Sqrt,
        "abs" => MathFn::Abs,
        "floor" => MathFn::Floor,
        "ceil" => MathFn::Ceil,
        "round" => MathFn::Round,
        "trunc" => MathFn::Trunc,
        "max" => MathFn::Max,
        "min" => MathFn::Min,
        "imul" => MathFn::Imul,
        _ => return None,
    })
}

thread_local! {
    /// `(chunk address, pc)` of `Math` call sites whose guard failed: compiled again, they take
    /// the call path. An address reused by a later chunk only costs that chunk the inline path.
    static MATH_FAILED: std::cell::RefCell<std::collections::HashSet<(usize, u32)>> =
        std::cell::RefCell::new(std::collections::HashSet::new());
}

pub(crate) fn math_site_failed(chunk: &Chunk, pc: usize) -> bool {
    MATH_FAILED.with(|s| {
        s.borrow()
            .contains(&(chunk as *const Chunk as usize, pc as u32))
    })
}

/// Whether `LoadName(n, c) GetMethod(m, _)` at `pc` reads the intrinsic `Math.<m>`: `Math`
/// resolves through its name cache (plain data bindings / properties only — no getter can
/// run), `Math` is an ordinary object, and its own data property `m` holds a native function
/// whose code is the builtin's.
unsafe fn math_check(f: *mut JitFrame, pc: usize) -> Option<()> {
    let chunk = &*(*f).chunk;
    let env = &*(*f).env;
    let i = &*(*f).interp;
    let (Op::LoadName(n, c), Op::GetMethod(m, _)) = (*chunk.ops.get(pc)?, *chunk.ops.get(pc + 1)?)
    else {
        return None;
    };
    if c as usize >= chunk.name_caches.len() {
        return None;
    }
    let name = chunk.names.get(m as usize)?;
    let want = crate::builtins::jit_math_fns()
        .into_iter()
        .find(|(k, _)| *k == &**name)?
        .1;
    let math = chunk
        .name_ic_hit(i, env, c)
        .or_else(|| chunk.name_ic_fill(i, env, n, c))?;
    let Value::Obj(mo) = math else { return None };
    if !i.ordinary_get_ptr(crate::value::Gc::as_ptr(&mo) as usize) {
        return None;
    }
    let fv = {
        let mb = mo.try_borrow().ok()?;
        if !matches!(mb.exotic, crate::value::Exotic::None) {
            return None;
        }
        let slot = mb.props.slot_of(name)?;
        let p = mb.props.entry_at(slot)?;
        if p.accessor() {
            return None;
        }
        p.value()
    };
    let Value::Obj(fo) = fv else { return None };
    let fb = fo.try_borrow().ok()?;
    match fb.call {
        crate::value::Callable::Native(fp) if fp as usize == want as usize => Some(()),
        _ => None,
    }
}

pub(crate) unsafe extern "C" fn math_guard(f: *mut JitFrame, pc: u32) -> u32 {
    if math_check(f, pc as usize).is_some() {
        return 1;
    }
    let key = ((*f).chunk as usize, pc);
    MATH_FAILED.with(|s| {
        s.borrow_mut().insert(key);
    });
    0
}
