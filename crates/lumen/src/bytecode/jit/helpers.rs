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
use crate::interpreter::{Abrupt, Env, Interp};
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
    /// `(frame, pc: u32, expect: *const ()) -> u32` — 1 when the call site starting at `pc`
    /// (`LoadNameForCall`, or `LoadName GetMethod`) still resolves, without running JS, to the
    /// function object at `expect` (see [`site_callee`]). Pure; failures are remembered like
    /// [`Helper::MathGuard`]'s.
    FastGuard,
    /// `(frame, dst: *mut Value)` — move `frame.exception` into `dst` (a region `catch`).
    TakeException,
    /// `(frame, src: *mut Value)` — move `*src` into `frame.exception` (a `throw` caught by a
    /// region handler).
    SetException,
    /// `(frame, iter: u32, next: u32, dst) -> u32` — `IterStepL(iter, next)`: the yielded value
    /// (or `undefined`) into `dst`; the result has bit 0 set when a value was yielded, bit 1
    /// when the step went through the full protocol (JS may have run), and is
    /// [`ITER_THREW`] on a throw.
    IterStep,
    /// `(frame, v: *const Value, write: u32) -> u32` — when `*v` is a typed array whose
    /// elements native code may access directly (a numeric, non-Float16 kind over an unshared
    /// buffer, in bounds; for `write` also not immutable): its [`TaCode`] with the address of
    /// element 0 in `frame.ta_data` and the length in `frame.ta_len`; else 0. Pure. The view
    /// stays valid until JS runs (only JS can detach, resize or transfer a buffer, or create a
    /// typed array that could reuse a freed one's address).
    TaView,
    /// `(frame, v: *const Value) -> f64` — `v.length` when `*v` is a typed array without an own
    /// `length` (computed like `Interp::get`, which never consults the prototype's getter);
    /// else -1. Pure.
    TaLength,
    /// `(frame, pc: u32, slot: u32, chunk: *const Chunk) -> u32` — 1 when local `slot` holds a
    /// function the interpreter would call inline whose body is `chunk` (an inlined call site
    /// starting at `pc`, `LoadLocal(slot) <args> Call`). Pure; failures are remembered like
    /// [`Helper::MathGuard`]'s.
    SlotGuard,
    /// `(frame, callee: *mut JitFrame, word: u64, entry) -> status` — finish a direct call
    /// whose function code (at address `entry`) left through anything but a return exit (see
    /// `build::call`): the interpreter completes the call on the callee frame's slots. The
    /// result goes to the callee frame's `stack[0]`, a throw to `frame.exception`; the callee
    /// slots are left `Undefined`.
    CallFinish,
    /// `(frame, n: u32, c: u32) -> PTR` — the payload word of the value free name `names[n]`
    /// resolves to through name cache `c` when that is an object and needs no JS; else 0. Pure.
    NameWord,
    /// `(frame)` — pop the top `Interp::fn_frames` entry (one with `extra` state to drop).
    PopFnFrame,
    /// `(frame, pc: u32)` — remember that the speculation of the call site at `pc` failed (a
    /// recompile leaves it out, see [`math_site_failed`]).
    SiteFailed,
    /// `(p: *mut Value, n: u32)` — drop `n` values in place, leaving `Undefined`.
    DropN,
}

/// [`Helper::IterStep`]'s throw result.
pub(crate) const ITER_THREW: u32 = 4;

pub(crate) const ALL: [Helper; 32] = [
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
    Helper::FastGuard,
    Helper::TakeException,
    Helper::SetException,
    Helper::IterStep,
    Helper::TaView,
    Helper::TaLength,
    Helper::SlotGuard,
    Helper::CallFinish,
    Helper::NameWord,
    Helper::PopFnFrame,
    Helper::SiteFailed,
    Helper::DropN,
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
        Helper::FastGuard => (&[P, I32, P], &[I32]),
        Helper::TakeException | Helper::SetException => (&[P, P], &[]),
        Helper::IterStep => (&[P, I32, I32, P], &[I32]),
        Helper::TaView => (&[P, P, I32], &[I32]),
        Helper::TaLength => (&[P, P], &[F64]),
        Helper::SlotGuard => (&[P, I32, I32, P], &[I32]),
        Helper::CallFinish => (&[P, P, I64, P], &[I32]),
        Helper::NameWord => (&[P, I32, I32], &[P]),
        Helper::PopFnFrame => (&[P], &[]),
        Helper::SiteFailed => (&[P, I32], &[]),
        Helper::DropN => (&[P, I32], &[]),
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
        Helper::FastGuard => fast_guard as *const () as usize,
        Helper::TakeException => take_exception as *const () as usize,
        Helper::SetException => set_exception as *const () as usize,
        Helper::IterStep => iter_step as *const () as usize,
        Helper::TaView => ta_view as *const () as usize,
        Helper::TaLength => ta_length as *const () as usize,
        Helper::SlotGuard => slot_guard as *const () as usize,
        Helper::CallFinish => call_finish as *const () as usize,
        Helper::NameWord => name_word as *const () as usize,
        Helper::PopFnFrame => pop_fn_frame as *const () as usize,
        Helper::SiteFailed => site_failed as *const () as usize,
        Helper::DropN => drop_n as *const () as usize,
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
        | PopHandler
        | SwitchLK(..)
        | SuperCtor
        | SuperCall(_)
        | SuperCallSpread(_)
        | DerivedReturn
        | InitialYield
        | Yield
        | YieldDelegate(_) => false,
        // The widened subset (bytecode/ext_ops.rs): operand/env/this-only helpers.
        GetPrivate(_) | SetPrivate(_) | GetPrivateMethod(_) | PrivateIn(_)
        | UpdatePrivate(..) | NewObject | InitProp(..) | InitPropComputed(_)
        | InitMethod(..) | CopyDataProps | SetProtoLit | MakeClass(..) | Nip | SuperGet(_)
        | SuperGetElem | SuperBase | SuperMethod(..) | SuperMethodElem(_) | ObjRest(..)
        | NewArrayLit | ArrayAppend | ArrayAppendSpread | ArrayHole => true,
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
            | IterCloseL(_)
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
    let i = &mut *(*f).interp;
    let r = match crate::bytecode::inline_callee(i, &callee) {
        Some(c) => match super::native_code(&c.chunk) {
            Some(native) => call_native(i, callee, this, s.add(at), argc, c, native),
            None => call_frame(i, callee, this, s.add(at), argc, c),
        },
        None => {
            // The arguments stay owned by the frame's stack for the call (they root their
            // objects there, like the interpreter's operand stack does), then are dropped.
            let args = std::slice::from_raw_parts(s.add(at), argc);
            let r = i.call(callee, this, args);
            for k in 0..argc {
                *s.add(at + k) = Value::Undefined;
            }
            r
        }
    };
    match r {
        Ok(v) => {
            std::ptr::write(s.add(base), v);
            STATUS_OK
        }
        Err(e) => fail(f, e),
    }
}

/// `Interp::call` for a callee [`crate::bytecode::inline_callee`] accepted (the same steps as
/// its shortcut, `bytecode::call_compiled`), except that the arguments are *moved* from the
/// region's stack into the callee's parameter slots — no clone per argument, no drop after.
/// The argument window is left all `Undefined`.
#[inline(always)]
unsafe fn call_frame(
    i: &mut Interp,
    callee: Value,
    this: Value,
    args: *mut Value,
    argc: usize,
    c: crate::bytecode::InlineCallee,
) -> Result<Value, Abrupt> {
    use crate::bytecode::{drive_vm, enter_frame, leave_frame, VmStep};
    let clear = |from: usize| {
        for k in from..argc {
            *args.add(k) = Value::Undefined;
        }
    };
    // `inline_callee` already checked the recursion limit.
    i.depth += 1;
    if let Err(e) = i.gc_check_amortized() {
        i.depth -= 1;
        clear(0);
        return Err(e);
    }
    let mut rec = i.vm_frame_one.pop().unwrap_or_default();
    let seed = enter_frame(i, &mut rec, callee, this, None, c, args, argc, true);
    clear(seed);
    let r = {
        let crate::bytecode::InlineFrame {
            chunk,
            env,
            this_val,
            pc,
            slots,
            stack,
            handlers,
            ..
        } = &mut rec;
        // SAFETY: `enter_frame` filled both.
        let (chunk, env) = (
            chunk.as_deref().unwrap_unchecked(),
            env.as_ref().unwrap_unchecked(),
        );
        match super::run_entered(i, chunk, env, slots, stack, pc, this_val) {
            Some(r) => r,
            None => drive_vm(i, chunk, env, slots, stack, pc, this_val, handlers, None),
        }
    };
    leave_frame(i, &mut rec);
    if i.vm_frame_one.len() < 64 {
        i.vm_frame_one.push(rec);
    }
    let mut r = match r {
        Ok(VmStep::Done(v)) => Ok(v),
        Ok(VmStep::Await(_)) => Err(i.throw("TypeError", "compiled code awaited in a call")),
        Err(e) => Err(e),
    };
    // A proper tail call unwound out of the callee (`Interp::call`'s trampoline).
    while r.is_ok() {
        match i.pending_tail.take() {
            Some(bx) => {
                let (g, t, a) = *bx;
                r = i.call(g, t, &a);
            }
            None => break,
        }
    }
    i.depth -= 1;
    r
}

/// Slots a callee of [`call_native`] may have (they live on the Rust stack).
pub(super) const NATIVE_SLOTS: usize = 16;

/// [`call_frame`] for a callee whose chunk has whole-function code (see
/// [`super::native_code`]): the same entry and exit work as `enter_frame` / `leave_frame` (kept
/// in step with them), but with the frame's slots on the Rust stack and the code entered
/// directly — no frame record, no interpreter driver. When the code exits part-way the
/// interpreter finishes the call on the same slots. The arguments are moved as in
/// `call_frame`.
#[inline(never)]
#[allow(clippy::too_many_arguments)]
unsafe fn call_native(
    i: &mut Interp,
    callee: Value,
    this: Value,
    args: *mut Value,
    argc: usize,
    c: crate::bytecode::InlineCallee,
    native: std::rc::Rc<super::Native>,
) -> Result<Value, Abrupt> {
    use crate::bytecode::{drive_vm, drop_value_fast, VmStep};
    let clear = |from: usize| {
        for k in from..argc {
            *args.add(k) = Value::Undefined;
        }
    };
    // `inline_callee` already checked the recursion limit.
    i.depth += 1;
    if let Err(e) = i.gc_check_amortized() {
        i.depth -= 1;
        clear(0);
        return Err(e);
    }
    let chunk: &Chunk = &c.chunk;
    // --- enter_frame (an ordinary call) ---
    let saved_ctor = std::mem::replace(&mut i.constructing, false);
    let saved_nt = if !c.arrow && !matches!(i.new_target, Value::Undefined) {
        Some(std::mem::replace(&mut i.new_target, Value::Undefined))
    } else {
        None
    };
    let fn_ptr = match &callee {
        Value::Obj(o) => crate::value::Gc::as_ptr(o) as usize,
        _ => unreachable!("inline callee is a function object"),
    };
    i.fn_frames.push(crate::interpreter::FnFrame {
        fn_ptr,
        coro: i.cur_coro,
        strict: c.strict,
        extra: None,
    });
    let this_val = if chunk.uses_this() {
        i.bind_compiled_this_flags(c.strict, chunk, this, false)
    } else {
        drop_value_fast(this);
        Value::Undefined
    };
    let saved_strict = std::mem::replace(&mut i.strict, c.strict);
    let saved_tco = std::mem::replace(&mut i.tco_ok, c.strict);
    let saved_field_init = i.in_field_init_code;
    let saved_agb = i.in_async_gen_body;
    if !c.arrow {
        i.in_field_init_code = false;
        i.in_async_gen_body = false;
    }
    let arg_slice = std::slice::from_raw_parts_mut(args, argc);
    let env = match &chunk.activation_layout {
        Some(layout) => layout.make_env(chunk, i, &c.env, &this_val, arg_slice),
        None => c.env.clone(),
    };
    let args_obj = match chunk.arguments_slot {
        Some(_) => Some(i.make_compiled_arguments_object(arg_slice, &env)),
        None => None,
    };
    let n = chunk.n_slots;
    let mut store = [const { std::mem::MaybeUninit::<Value>::uninit() }; NATIVE_SLOTS];
    let seed = chunk.n_params.min(argc);
    for (k, v) in arg_slice[..seed].iter_mut().enumerate() {
        store[k].write(std::mem::take(v));
    }
    for v in &mut store[seed..n] {
        v.write(Value::Undefined);
    }
    let slots: &mut [Value] = std::slice::from_raw_parts_mut(store.as_mut_ptr().cast::<Value>(), n);
    if let (Some(s), Some(ao)) = (chunk.arguments_slot, args_obj) {
        slots[s as usize] = Value::Obj(ao);
    }
    chunk.seed_rest(i, slots, arg_slice);
    for &s in &chunk.var_force_resets {
        slots[s as usize] = Value::Undefined;
    }
    clear(seed);
    // --- run ---
    let mut stack: Vec<Value> = Vec::new();
    let mut pc = 0usize;
    let r = match super::enter(i, chunk, &env, slots, &mut stack, &mut pc, &this_val, None, &native)
    {
        Ok(Some(step)) => Ok(step),
        Ok(None) => {
            let mut handlers = Vec::new();
            drive_vm(i, chunk, &env, slots, &mut stack, &mut pc, &this_val, &mut handlers, None)
        }
        Err(e) => Err(e),
    };
    drop(native);
    // --- leave_frame ---
    for v in slots.iter_mut() {
        drop_value_fast(std::mem::take(v));
    }
    drop(stack);
    drop_value_fast(this_val);
    drop(env);
    i.strict = saved_strict;
    i.tco_ok = saved_tco;
    i.in_field_init_code = saved_field_init;
    i.in_async_gen_body = saved_agb;
    let top = i.fn_frames.len() - 1;
    if i.fn_frames[top].extra.is_some() {
        i.fn_frames.truncate(top);
    } else {
        // SAFETY: the remaining fields of `FnFrame` are plain data.
        i.fn_frames.set_len(top);
    }
    i.constructing = saved_ctor;
    if let Some(nt) = saved_nt {
        drop_value_fast(std::mem::replace(&mut i.new_target, nt));
    }
    drop_value_fast(callee);
    let mut r = match r {
        Ok(VmStep::Done(v)) => Ok(v),
        Ok(VmStep::Await(_)) => Err(i.throw("TypeError", "compiled code awaited in a call")),
        Err(e) => Err(e),
    };
    // A proper tail call unwound out of the callee (`Interp::call`'s trampoline).
    while r.is_ok() {
        match i.pending_tail.take() {
            Some(bx) => {
                let (g, t, a) = *bx;
                r = i.call(g, t, &a);
            }
            None => break,
        }
    }
    i.depth -= 1;
    r
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

// ---- fast native ops, handlers and iteration ------------------------------------------------

/// An unboxed argument / result kind of a `#[op(fast)]` entry (`embed::FastKind`, which only
/// exists with the `embed` feature).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(not(feature = "embed"), allow(dead_code))]
pub(crate) enum FastArg {
    F64,
    I32,
    U32,
    Bool,
    Void,
}

/// The unboxed signature and entry address of `callee` when it is a `#[op(fast)]` op.
#[cfg(feature = "embed")]
pub(crate) fn fast_sig(i: &Interp, callee: &Value) -> Option<(Vec<FastArg>, FastArg, u64)> {
    use crate::embed_convert::FastKind;
    let sig = i.fast_op_of(callee)?;
    let k = |k: &FastKind| match k {
        FastKind::F64 => FastArg::F64,
        FastKind::I32 => FastArg::I32,
        FastKind::U32 => FastArg::U32,
        FastKind::Bool => FastArg::Bool,
        FastKind::Void => FastArg::Void,
    };
    Some((
        sig.args.iter().map(k).collect(),
        k(&sig.ret),
        sig.entry.0 as usize as u64,
    ))
}

#[cfg(not(feature = "embed"))]
pub(crate) fn fast_sig(_i: &Interp, _callee: &Value) -> Option<(Vec<FastArg>, FastArg, u64)> {
    None
}

/// The value of free name `names[n]` through name cache `c`, when the cache can produce it
/// without running JS (plain data bindings / properties only).
pub(crate) fn name_value(i: &Interp, chunk: &Chunk, env: &Env, n: u32, c: u32) -> Option<Value> {
    if c as usize >= chunk.name_caches.len() || n as usize >= chunk.names.len() {
        return None;
    }
    chunk
        .name_ic_hit(i, env, c)
        .or_else(|| chunk.name_ic_fill(i, env, n, c))
}

/// The own data property `name` of `obj` when `obj` is an ordinary object (no proxy, typed
/// array or namespace behavior, no exotic kind) — reading it runs no JS.
fn own_data(i: &Interp, obj: &Value, name: &str) -> Option<Value> {
    let Value::Obj(o) = obj else { return None };
    if !i.ordinary_get_ptr(crate::value::Gc::as_ptr(o) as usize) {
        return None;
    }
    let b = o.try_borrow().ok()?;
    if !matches!(b.exotic, crate::value::Exotic::None) {
        return None;
    }
    let slot = b.props.slot_of(name)?;
    let p = b.props.entry_at(slot)?;
    if p.accessor() {
        return None;
    }
    Some(p.value())
}

/// The callee of the call site whose callee part starts at `pc` — `LoadNameForCall(n, c)`, or
/// `LoadName(n, c) GetMethod(m, _)` — as far as it can be resolved without running JS.
pub(crate) fn site_callee(i: &Interp, chunk: &Chunk, env: &Env, pc: usize) -> Option<Value> {
    match *chunk.ops.get(pc)? {
        Op::LoadNameForCall(n, c) => name_value(i, chunk, env, n, c),
        Op::LoadName(n, c) => {
            let Op::GetMethod(m, _) = *chunk.ops.get(pc + 1)? else {
                return None;
            };
            let obj = name_value(i, chunk, env, n, c)?;
            own_data(i, &obj, chunk.names.get(m as usize)?)
        }
        _ => None,
    }
}

pub(crate) unsafe extern "C" fn fast_guard(f: *mut JitFrame, pc: u32, expect: *const ()) -> u32 {
    let chunk = &*(*f).chunk;
    let env = &*(*f).env;
    let i = &*(*f).interp;
    if let Some(Value::Obj(o)) = site_callee(i, chunk, env, pc as usize) {
        if crate::value::Gc::as_ptr(&o) as usize == expect as usize {
            return 1;
        }
    }
    let key = ((*f).chunk as usize, pc);
    MATH_FAILED.with(|s| {
        s.borrow_mut().insert(key);
    });
    0
}

pub(crate) unsafe extern "C" fn slot_guard(
    f: *mut JitFrame,
    pc: u32,
    slot: u32,
    expect: *const Chunk,
) -> u32 {
    let chunk = &*(*f).chunk;
    let i = &*(*f).interp;
    if (slot as usize) < chunk.n_slots {
        let v = &*(*f).slots.add(slot as usize);
        if let Some(c) = crate::bytecode::inline_callee(i, v) {
            if std::rc::Rc::as_ptr(&c.chunk) == expect {
                return 1;
            }
        }
    }
    let key = ((*f).chunk as usize, pc);
    MATH_FAILED.with(|s| {
        s.borrow_mut().insert(key);
    });
    0
}

pub(crate) unsafe extern "C" fn take_exception(f: *mut JitFrame, dst: *mut Value) {
    std::ptr::write(dst, std::mem::take(&mut (*f).exception));
}

pub(crate) unsafe extern "C" fn set_exception(f: *mut JitFrame, src: *mut Value) {
    (*f).exception = std::ptr::replace(src, Value::Undefined);
}

pub(crate) unsafe extern "C" fn iter_step(f: *mut JitFrame, is: u32, ns: u32, dst: *mut Value) -> u32 {
    let chunk = &*(*f).chunk;
    let (is, ns) = (is as usize, ns as usize);
    if is >= chunk.n_slots || ns >= chunk.n_slots {
        let e = (*(*f).interp).throw("TypeError", "compiled code read a missing local");
        fail(f, e);
        return ITER_THREW;
    }
    let i = &mut *(*f).interp;
    let (it, nx) = (&*(*f).slots.add(is), &*(*f).slots.add(ns));
    // The pure fast path borrows the slots; the full protocol runs on owned clones (the
    // callbacks may reassign anything).
    if let Some(v) = crate::bytecode::array_iterator_step::try_yield(i, it, nx) {
        std::ptr::write(dst, v);
        return 1;
    }
    let (it, nx) = (it.clone(), nx.clone());
    match i.iterator_step(&it, &nx) {
        Ok(Some(v)) => {
            std::ptr::write(dst, v);
            3
        }
        Ok(None) => {
            std::ptr::write(dst, Value::Undefined);
            2
        }
        Err(e) => {
            std::ptr::write(dst, Value::Undefined);
            fail(f, e);
            ITER_THREW
        }
    }
}

/// The element kinds [`Helper::TaView`] reports (0 = not accessible natively).
pub(crate) mod ta_code {
    pub const I8: u32 = 1;
    pub const U8: u32 = 2;
    pub const U8C: u32 = 3;
    pub const I16: u32 = 4;
    pub const U16: u32 = 5;
    pub const I32: u32 = 6;
    pub const U32: u32 = 7;
    pub const F32: u32 = 8;
    pub const F64: u32 = 9;
    /// One past the largest code.
    pub const END: u32 = 10;
}

pub(crate) unsafe extern "C" fn ta_view(f: *mut JitFrame, v: *const Value, write: u32) -> u32 {
    use crate::value::TaKind;
    let Value::Obj(o) = &*v else { return 0 };
    let i = &mut *(*f).interp;
    let Some(info) = i
        .typed_arrays
        .get(&(crate::value::Gc::as_ptr(o) as usize))
        .copied()
    else {
        return 0;
    };
    let code = match info.kind {
        TaKind::I8 => ta_code::I8,
        TaKind::U8 => ta_code::U8,
        TaKind::U8Clamped => ta_code::U8C,
        TaKind::I16 => ta_code::I16,
        TaKind::U16 => ta_code::U16,
        TaKind::I32 => ta_code::I32,
        TaKind::U32 => ta_code::U32,
        TaKind::F32 => ta_code::F32,
        TaKind::F64 => ta_code::F64,
        _ => return 0,
    };
    if !i.shared_buffers.is_empty() && i.shared_buffers.contains_key(&info.buffer) {
        return 0;
    }
    if write != 0 && !i.immutable_buffers.is_empty() && i.immutable_buffers.contains(&info.buffer) {
        return 0;
    }
    let Some(len) = i.ta_len(&info) else { return 0 };
    let Some(buf) = i.array_buffers.get_mut(&info.buffer) else {
        return 0;
    };
    let bytes: &mut [u8] = buf;
    let es = info.kind.elsize();
    match len.checked_mul(es).and_then(|n| n.checked_add(info.offset)) {
        Some(end) if end <= bytes.len() => {}
        _ => return 0,
    }
    (*f).ta_data = bytes.as_mut_ptr().add(info.offset);
    (*f).ta_len = len;
    code
}

pub(crate) unsafe extern "C" fn ta_length(f: *mut JitFrame, v: *const Value) -> f64 {
    let i = &*(*f).interp;
    let Value::Obj(o) = &*v else { return -1.0 };
    let Some(info) = i.typed_arrays.get(&(crate::value::Gc::as_ptr(o) as usize)) else {
        return -1.0;
    };
    // `Interp::get`'s typed-array case: an own `length` shadows the computed one.
    match o.try_borrow() {
        Ok(b) if !b.props.contains("length") => i.ta_len(info).unwrap_or(0) as f64,
        _ => -1.0,
    }
}

// ---- direct calls (see `build::call`) ---------------------------------------------------------

pub(crate) unsafe extern "C" fn call_finish(
    f: *mut JitFrame,
    nf: *mut JitFrame,
    word: u64,
    entry: *const (),
) -> u32 {
    use crate::bytecode::{drive_vm, VmStep};
    let i = &mut *(*f).interp;
    let chunk = &*(*nf).chunk;
    let env = &*(*nf).env;
    let this_val = &*(*nf).this_val;
    let slots = std::slice::from_raw_parts_mut((*nf).slots, chunk.n_slots);
    let area = (*nf).stack;
    let native = super::find_native(chunk, entry as usize);
    // Exits of replaced code are not counted against the current code.
    let current = chunk.jit.dentry.get() == entry as usize;
    let kind = word & 0xff;
    let exit_pc = (word >> 8) as usize;
    let depth = (*nf).exit_depth as usize;
    let mut stack: Vec<Value> = Vec::new();
    let mut pc = 0usize;
    let mut handlers = Vec::new();
    let r: Result<VmStep, Abrupt> = match kind {
        EXIT_RETURN => Ok(VmStep::Done(std::ptr::replace(area, Value::Undefined))),
        EXIT_ENTRY_FAIL => {
            if let (Some(n), true) = (&native, current) {
                super::fn_entry_failed(chunk, slots, n);
            }
            drive_vm(i, chunk, env, slots, &mut stack, &mut pc, this_val, &mut handlers, None)
        }
        EXIT_THROW => {
            // As `on_entry`: the interpreter unwinds from the exit state (no handler of the
            // frame is live outside native code, so this propagates).
            stack.extend((0..depth).map(|k| std::ptr::replace(area.add(k), Value::Undefined)));
            pc = exit_pc;
            let e = std::mem::take(&mut (*nf).exception);
            drive_vm(i, chunk, env, slots, &mut stack, &mut pc, this_val, &mut handlers, Some(e))
        }
        _ => {
            stack.extend((0..depth).map(|k| std::ptr::replace(area.add(k), Value::Undefined)));
            pc = exit_pc;
            if let (Some(n), true) = (&native, current) {
                super::fn_deopt(chunk, exit_pc, n);
            }
            let set = ((*nf).exit_hset as usize)
                .checked_sub(1)
                .and_then(|k| native.as_ref()?.hsets.get(k).cloned());
            let first = match set {
                Some(set) => {
                    super::finish_handlers(i, chunk, env, slots, &mut stack, &mut pc, this_val, &set)
                }
                None => Ok(None),
            };
            match first {
                Ok(Some(step)) => Ok(step),
                Ok(None) => drive_vm(
                    i,
                    chunk,
                    env,
                    slots,
                    &mut stack,
                    &mut pc,
                    this_val,
                    &mut handlers,
                    None,
                ),
                Err(e) => Err(e),
            }
        }
    };
    drop(native);
    drop(stack);
    for v in slots.iter_mut() {
        drop(std::mem::take(v));
    }
    let mut r = match r {
        Ok(VmStep::Done(v)) => Ok(v),
        Ok(VmStep::Await(_)) => Err(i.throw("TypeError", "compiled code awaited in a call")),
        Err(e) => Err(e),
    };
    // A proper tail call unwound out of the callee (`Interp::call`'s trampoline).
    while r.is_ok() {
        match i.pending_tail.take() {
            Some(bx) => {
                let (g, t, a) = *bx;
                r = i.call(g, t, &a);
            }
            None => break,
        }
    }
    match r {
        Ok(v) => {
            std::ptr::write(area, v);
            STATUS_OK
        }
        Err(e) => fail(f, e),
    }
}

pub(crate) unsafe extern "C" fn name_word(f: *mut JitFrame, n: u32, c: u32) -> usize {
    let chunk = &*(*f).chunk;
    let env = &*(*f).env;
    let i = &*(*f).interp;
    match name_value(i, chunk, env, n, c) {
        // The payload word as a `Value` stores it (what call sites compare).
        Some(v @ Value::Obj(_)) => *(&v as *const Value as *const u8)
            .add(VALUE_PAYLOAD as usize)
            .cast::<usize>(),
        _ => 0,
    }
}

pub(crate) unsafe extern "C" fn pop_fn_frame(f: *mut JitFrame) {
    (*(*f).interp).fn_frames.pop();
}

pub(crate) unsafe extern "C" fn site_failed(f: *mut JitFrame, pc: u32) {
    let key = ((*f).chunk as usize, pc);
    MATH_FAILED.with(|s| {
        s.borrow_mut().insert(key);
    });
}

pub(crate) unsafe extern "C" fn drop_n(p: *mut Value, n: u32) {
    for k in 0..n as usize {
        drop(std::ptr::replace(p.add(k), Value::Undefined));
    }
}
