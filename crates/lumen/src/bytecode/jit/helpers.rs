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
    /// `(frame, n: u32, c: u32) -> *mut Value` — [`Helper::NamePtr`] for a write: also null
    /// for an immutable binding.
    NamePtrW,
    /// `(frame, n: u32, c: u32) -> *mut Property` — the global object's data property the free
    /// name `names[n]` (name cache `c`) resolves to (filling the cache first on a miss); null
    /// otherwise. Pure; the address stays valid until JS runs (only JS reshapes the global).
    GlobPtr,
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
    /// `(frame, base: u32, argc: u32, flags: u32) -> status` (flags: 1 = with `this`, plus
    /// [`CALL_AWAIT`]) — `Call(argc)` (receiver
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
    /// `(frame, cell: *mut SiteCell, callee: *const Value) -> 0/1` — rebind a direct call
    /// site's cache to `callee` when it is another function object with the site's code. Pure.
    SiteRebind,
    /// `(frame, cell: *mut SiteCell, callee: *const Value, need_obj: u32) -> *const Value` — the
    /// bound `this` of `callee` when it is a bound function without bound arguments whose
    /// target is the site's function (rebinding the cell as [`Helper::SiteRebind`] does), and
    /// with `need_obj`, an object `this`; null otherwise. Pure.
    BoundThis,
    /// `(frame, v: *const Value) -> 0/1` — `v` is an `apply` argument list whose elements
    /// [`Helper::ApplySeed`] may read without running code: `undefined`, `null`, or an ordinary
    /// Array (not a mapped `arguments`) whose indices below its length are all data
    /// properties. Pure.
    ApplyOk,
    /// `(v: *const Value, slots: *mut Value, n: u32)` — clone the first `n` elements of an
    /// [`Helper::ApplyOk`] list into `slots` (which hold `undefined`). Pure.
    ApplySeed,
    /// `(frame, cell: *const SiteCell, out: *mut Value)` — `[[Construct]]` of a direct `new`
    /// site's constructor (`cell.pin`): the new instance (OrdinaryCreateFromConstructor, the
    /// learned capacity) into `out`, and `new.target` set to the constructor. Pure.
    NewThis,
    /// `(frame, cell, inst: *mut Value, res: *mut Value, status: u32)` — finish a direct `new`:
    /// `new.target` back to `undefined`; on success the size hint is recorded and a non-object
    /// `*res` replaced by the instance (consumed). Pure.
    NewDone,
    /// `(frame) -> status` — count a call for the amortized collector poll (and poll when due),
    /// as `Interp::call` does: the `[[Construct]]` path does not count its calls, and a direct
    /// `new` site whose poll is due waits on the count moving. Runs JS (finalizers, interrupts).
    GcPoll,
    /// `(frame, d: u32, y: *const Value) -> 0/1` — `slots[d] += y` for a string slot, appending
    /// in place (the interpreter's `append_to_slot`); 0 when not applicable (nothing touched).
    /// Pure.
    AppendLocal,
    /// `(frame, obj: *mut Value, key: f64, dst: *mut Value, consume: u32) -> 0/1` — `obj[key]`
    /// through the interpreter's element fast paths (a one-unit string of an ASCII string, an
    /// own data element of a plain object or array, a typed-array element): 1 with the value
    /// in `dst` (and `obj` consumed when `consume`), 0 when not applicable (nothing touched).
    /// Pure.
    ElemGet,
    /// `(frame, a: *mut Value, b: *mut Value) -> 0/1` — `a === b`, consuming both. Pure.
    StrictEq,
    /// `(frame, a: *const Value, b: *const Value) -> 0/1/2` — `a === b` of borrowed values; 2
    /// when either is a TDZ marker (the ordinary path throws). Pure.
    StrictEqRef,
    /// `(dst: *mut Value, bits: u64, consume: u32)` — the `Value` of a refcounted NaN-boxed
    /// property word (a new reference) into `dst`; with `consume`, `dst` holds the receiver it
    /// was read from, dropped first (after the value is taken). Pure.
    UnpackClone,
    /// `(frame, pc: u32, base: *mut Value, n: u32)` — the `MakeArray(n)` / `MakeObject` literal
    /// at `pc` from the `n` values at `base` (consumed), the result into `base`. Pure.
    MakeLit,
    /// `(frame, dst: *mut Value, src: *mut Value, n: u32)` — a rest parameter's array from the
    /// `n` surplus arguments at `src` (consumed) into slot `dst` (trivially droppable). Pure.
    MakeRest,
    /// `(frame, cell: *const PlanCell, out: *mut Value, consume: u32) -> *mut Property` — see
    /// `build::new_plan::new_plan_helper`. Pure.
    NewPlan,
    /// `(frame, pc: u32, recv: *const Value) -> 0/1` — whether the `GetMethod` at `pc` on
    /// `recv` reads the intrinsic `String.prototype` method (see [`str_check`]); a failure is
    /// remembered for the site. Pure.
    StrGuard,
    /// `(frame, s: *const Value, pos: f64) -> f64` — `s.charCodeAt(pos)` of a String `s`. Pure.
    StrCodeAt,
    /// `(frame, fidx: u32, name: u32, dst: *mut Value)` — `MakeClosure(fidx, name)` in the
    /// frame's env into `dst` (a trivially droppable operand-stack entry). Pure.
    MakeClosure,
    /// `(frame, template: *const Props, base: *mut Value, n: u32)` — a `MakeObject` literal
    /// whose template instantiates in one box ([`Props::fast_template`]): the `n` values at
    /// `base` (consumed) as its properties, the object into `base`. Pure.
    AllocObj,
    /// `(frame, base: *mut Value, n: u32)` — a `MakeArray(n)` literal of the `n` values at
    /// `base` (consumed), the array into `base`. Pure.
    AllocArr,
    /// `(frame, dst: u32, parent: u32)` — `BlkNew(dst, parent)`. Pure.
    BlkNew,
    /// `(frame, slot: u32)` — `BlkCopy(slot)`. Pure.
    BlkCopy,
    /// `(frame, slot: u32, name: u32, is_const: u32)` — `BlkDecl`. Pure.
    BlkDecl,
    /// `(frame, slot: u32, name: u32, dst: *mut Value) -> status` — `BlkLoad` into `dst`
    /// (throws in the TDZ). Pure.
    BlkLoad,
    /// `(frame, slot: u32, name: u32, src: *mut Value, init: u32) -> status` — `BlkInit` /
    /// `BlkStore` of the value at `src` (moved out; `undefined` left there). Pure.
    BlkStore,
    /// `(frame, slot: u32, fidx: u32, name: u32, dst: *mut Value)` — `InEnv(slot)` +
    /// `MakeClosure(fidx, name)`: the closure over the block env in `slot`, into `dst`. Pure.
    MakeClosureIn,
    /// `(frame, pc: u32, dst: *mut Value) -> status` — the `BlkUpdate` at `pc`, its result (if
    /// it pushes one) into `dst`. Pure.
    BlkUpdate,
    /// `(arr: *const Value, v: *mut Value)` — `ArrayAppend`: move `*v` (left `Undefined`) onto
    /// the end of the array literal `*arr`. No frame, no status (never throws, runs no JS).
    ArrayAppend,
    /// `(f, pc: u32, recv: *const Value) -> u32`: 1 when `GetMethod(push)` at `pc` on `recv`
    /// reads the intrinsic `Array.prototype.push` of a plain Array (else the site is marked
    /// failed and 0 is returned).
    ArrPushGuard,
    /// `(f, recv: *mut Value, arg: *mut Value) -> status`: `recv.push(arg)` for a guarded site;
    /// the new length replaces `recv`.
    ArrPush,
}

/// [`Helper::IterStep`]'s throw result.
pub(crate) const ITER_THREW: u32 = 4;

pub(crate) const ALL: [Helper; 64] = [
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
    Helper::NamePtrW,
    Helper::GlobPtr,
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
    Helper::SiteRebind,
    Helper::BoundThis,
    Helper::ApplyOk,
    Helper::ApplySeed,
    Helper::NewThis,
    Helper::NewDone,
    Helper::GcPoll,
    Helper::AppendLocal,
    Helper::ElemGet,
    Helper::StrictEq,
    Helper::StrictEqRef,
    Helper::UnpackClone,
    Helper::MakeLit,
    Helper::MakeRest,
    Helper::NewPlan,
    Helper::StrGuard,
    Helper::StrCodeAt,
    Helper::MakeClosure,
    Helper::AllocObj,
    Helper::AllocArr,
    Helper::BlkNew,
    Helper::BlkCopy,
    Helper::BlkDecl,
    Helper::BlkLoad,
    Helper::BlkStore,
    Helper::MakeClosureIn,
    Helper::BlkUpdate,
    Helper::ArrayAppend,
    Helper::ArrPushGuard,
    Helper::ArrPush,
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
        Helper::NamePtr | Helper::NamePtrW | Helper::GlobPtr => (&[P, I32, I32], &[P]),
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
        Helper::SiteRebind => (&[P, P, P], &[I32]),
        Helper::BoundThis => (&[P, P, P, I32], &[P]),
        Helper::ApplyOk => (&[P, P], &[I32]),
        Helper::ApplySeed => (&[P, P, I32], &[]),
        Helper::NewThis => (&[P, P, P], &[]),
        Helper::NewPlan => (&[P, P, P, I32], &[P]),
        Helper::NewDone => (&[P, P, P, P, I32], &[]),
        Helper::GcPoll => (&[P], &[I32]),
        Helper::AppendLocal => (&[P, I32, P], &[I32]),
        Helper::ElemGet => (&[P, P, F64, P, I32], &[I32]),
        Helper::StrictEq => (&[P, P, P], &[I32]),
        Helper::StrictEqRef => (&[P, P, P], &[I32]),
        Helper::UnpackClone => (&[P, I64, I32], &[]),
        Helper::MakeLit => (&[P, I32, P, I32], &[]),
        Helper::MakeRest => (&[P, P, P, I32], &[]),
        Helper::StrGuard => (&[P, I32, P], &[I32]),
        Helper::StrCodeAt => (&[P, P, F64], &[F64]),
        Helper::MakeClosure => (&[P, I32, I32, P], &[]),
        Helper::AllocObj => (&[P, P, P, I32], &[]),
        Helper::AllocArr => (&[P, P, I32], &[]),
        Helper::BlkNew => (&[P, I32, I32], &[]),
        Helper::BlkCopy => (&[P, I32], &[]),
        Helper::BlkDecl => (&[P, I32, I32, I32], &[]),
        Helper::BlkLoad => (&[P, I32, I32, P], &[I32]),
        Helper::BlkStore => (&[P, I32, I32, P, I32], &[I32]),
        Helper::MakeClosureIn => (&[P, I32, I32, I32, P], &[]),
        Helper::BlkUpdate => (&[P, I32, P], &[I32]),
        Helper::ArrayAppend => (&[P, P], &[]),
        Helper::ArrPushGuard => (&[P, I32, P], &[I32]),
        Helper::ArrPush => (&[P, P, P], &[I32]),
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
        Helper::NamePtrW => name_ptr_w as *const () as usize,
        Helper::GlobPtr => glob_ptr as *const () as usize,
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
        Helper::SiteRebind => site_rebind as *const () as usize,
        Helper::BoundThis => bound_this as *const () as usize,
        Helper::ApplyOk => apply_ok as *const () as usize,
        Helper::ApplySeed => apply_seed as *const () as usize,
        Helper::NewThis => new_this as *const () as usize,
        Helper::NewPlan => super::build::new_plan::new_plan_helper as *const () as usize,
        Helper::StrGuard => str_guard as *const () as usize,
        Helper::StrCodeAt => str_code_at as *const () as usize,
        Helper::MakeClosure => make_closure as *const () as usize,
        Helper::AllocObj => alloc_obj as *const () as usize,
        Helper::AllocArr => alloc_arr as *const () as usize,
        Helper::BlkNew => blk_new as *const () as usize,
        Helper::BlkCopy => blk_copy as *const () as usize,
        Helper::BlkDecl => blk_decl as *const () as usize,
        Helper::BlkLoad => blk_load as *const () as usize,
        Helper::BlkStore => blk_store as *const () as usize,
        Helper::MakeClosureIn => make_closure_in as *const () as usize,
        Helper::BlkUpdate => blk_update as *const () as usize,
        Helper::NewDone => new_done as *const () as usize,
        Helper::GcPoll => gc_poll as *const () as usize,
        Helper::AppendLocal => append_local as *const () as usize,
        Helper::ElemGet => elem_get as *const () as usize,
        Helper::StrictEq => strict_eq as *const () as usize,
        Helper::StrictEqRef => strict_eq_ref as *const () as usize,
        Helper::UnpackClone => unpack_clone as *const () as usize,
        Helper::MakeLit => make_lit as *const () as usize,
        Helper::MakeRest => make_rest as *const () as usize,
        Helper::ArrayAppend => array_append as *const () as usize,
        Helper::ArrPushGuard => arr_push_guard as *const () as usize,
        Helper::ArrPush => arr_push as *const () as usize,
    } as u64)
}

/// [`Helper::ArrayAppend`].
unsafe extern "C" fn array_append(arr: *const Value, v: *mut Value) {
    let v = crate::value::take_value_words(v);
    match &*arr {
        Value::Obj(ao) => crate::bytecode::ext_ops::append_one(ao, v),
        _ => unreachable!("array literal under construction"),
    }
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
        | ArgsLen(..)
        | ArgsGet(..)
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
        | DerivedReturn
        | InitialYield
        | Yield
        | YieldDelegate(_)
        | IterRestL(..)
        | GetAsyncIter
        | AsyncIterNext(..)
        | AsyncIterResult(_)
        | AsyncCloseCall(..)
        | AsyncCloseCheck(_)
        | AsyncDelegateInit
        | AsyncDelegateCall(..)
        | AsyncDelegateResult(..)
        | AsyncDelegateCloseReject(..)
        | AsyncDelegateSpecial(..) => false,
        // The callee from the reflection frame (`LoadCallee` syncs the pending direct-call
        // records first, see `super::sync_frames`).
        LoadCallee => true,
        // Block envs: a carrier slot is touched by these ops only (never modeled in SSA, see
        // `build::op_slots`), so its memory is authoritative. `InEnv` runs together with the
        // closure-creating op after it (see `build`).
        BlkNew(..) | BlkDecl(..) | BlkCopy(_) | BlkLoad(..) | BlkStore(..) | BlkInit(..)
        | BlkUpdate(..) | InEnv(_) => true,
        // A proper tail call run as a generic call-out: an ordinary call, or (deep in tail
        // recursion) `Interp::pending_tail` for the caller's trampoline, with the `Return`
        // after it returning the pushed `undefined` (see `call`'s `DSite::tails`).
        TailCall(..) | TailCallSpread(..) => true,
        NewSpread(_) => true,
        ImportCall(..) => true,
        // Inlined array callbacks (see `bytecode::inline_callback`): stack-only.
        ArrayCbGuard(_) | ArrayCbHas | ArrayCbDone(_) => true,
        // `super(…)`: the parent construct runs as the interpreter's op.
        SuperCtor | SuperCall(_) | SuperCallSpread(_) => true,
        // The widened subset (bytecode/ext_ops.rs): operand/env/this-only helpers.
        GetPrivate(_) | SetPrivate(_) | GetPrivateMethod(_) | PrivateIn(_)
        | UpdatePrivate(..) | NewObject | InitProp(..) | InitPropComputed(_)
        | InitMethod(..) | CopyDataProps | SetProtoLit | MakeClass(..) | Nip | SuperGet(_)
        | SuperGetElem | SuperBase | SuperMethod(..) | SuperMethodElem(_) | ObjRest(..)
        | NewArrayLit | ArrayAppend | ArrayAppendSpread | ArrayHole => true,
        // Class field DefineField (bytecode/class_fields.rs): operands only.
        DefineField(..) => true,
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
            | IterRestL(..)
            | ArgsLen(..)
            | ArgsGet(..)
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
    // Other primitives whose ToPrimitive / ToNumber is plain (`undefined`, `null`, booleans —
    // `x + undefined` for a missing argument, say): the Number operation on their values.
    // (Not the equality operators, which do not convert these.)
    let prim = |v: &Value| match v {
        Value::Num(x) => Some(*x),
        Value::Undefined => Some(f64::NAN),
        Value::Null => Some(0.0),
        Value::Bool(b) => Some(*b as u8 as f64),
        _ => None,
    };
    if !matches!(op, BIN_EQEQ | BIN_NOTEQ | BIN_STRICTEQ | BIN_STRICTNOTEQ) {
        if let (Some(x), Some(y)) = (prim(&a), prim(&b)) {
            if let Some(v) = binary_num(op, x, y) {
                std::ptr::write(dst, v);
                return STATUS_OK;
            }
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
    let b = b.or_else(|| chunk.name_path_binding(env, c, false));
    match b {
        Some(bd) if (*bd).initialized && (*bd).import_ref.is_none() => &(*bd).value,
        _ => std::ptr::null(),
    }
}

pub(crate) unsafe extern "C" fn name_ptr_w(f: *mut JitFrame, n: u32, c: u32) -> *mut Value {
    let chunk = &*(*f).chunk;
    let env = &*(*f).env;
    if c as usize >= chunk.name_caches.len() || n as usize >= chunk.names.len() {
        return std::ptr::null_mut();
    }
    let b = name_binding(chunk, env, c as usize).or_else(|| chunk.name_path_binding(env, c, true));
    match b {
        Some(bd) if (*bd).initialized && (*bd).mutable && (*bd).import_ref.is_none() => {
            &mut (*bd).value
        }
        _ => std::ptr::null_mut(),
    }
}

pub(crate) unsafe extern "C" fn glob_ptr(
    f: *mut JitFrame,
    n: u32,
    c: u32,
) -> *mut crate::value::Property {
    let chunk = &*(*f).chunk;
    let env = &*(*f).env;
    let i = &*(*f).interp;
    if c as usize >= chunk.name_caches.len() || n as usize >= chunk.names.len() {
        return std::ptr::null_mut();
    }
    let mut slot = chunk.name_global_slot(i, env, c);
    if slot.is_none() && chunk.name_ic_fill(i, env, n, c).is_some() {
        slot = chunk.name_global_slot(i, env, c);
    }
    let Some(slot) = slot else {
        return std::ptr::null_mut();
    };
    if !i.ordinary_get_ptr(crate::value::Gc::as_ptr(&i.global) as usize) {
        return std::ptr::null_mut();
    }
    let Ok(mut g) = i.global.try_borrow_mut() else {
        return std::ptr::null_mut();
    };
    match g.props.entry_at_mut(slot) {
        Some(p) if !p.accessor() => p,
        _ => std::ptr::null_mut(),
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

/// Arguments an unwrapped call adaptor (see [`unwrap_adaptor`]) may pass without allocating.
const ARGBUF: usize = 8;

/// The argument list of an unwrapped call adaptor: `len` initialized values.
struct ArgBuf {
    vals: [std::mem::MaybeUninit<Value>; ARGBUF],
    len: usize,
}

impl ArgBuf {
    #[inline(always)]
    fn new() -> ArgBuf {
        ArgBuf { vals: [const { std::mem::MaybeUninit::uninit() }; ARGBUF], len: 0 }
    }
    #[inline]
    fn push(&mut self, v: Value) {
        self.vals[self.len].write(v);
        self.len += 1;
    }
    fn as_mut_ptr(&mut self) -> *mut Value {
        self.vals.as_mut_ptr().cast()
    }
}

impl Drop for ArgBuf {
    fn drop(&mut self) {
        for v in &mut self.vals[..self.len] {
            // SAFETY: the first `len` are initialized (callees leave them `Undefined`).
            unsafe { v.assume_init_drop() };
        }
    }
}

/// The call that `f.call(t, ...a)`, `f.apply(t, list)` or a bound function makes, with its
/// arguments moved (or, for `apply`, copied) into `buf`: the adaptor's own native frame and
/// `Interp::call` round trip are skipped (none of them has a stack-trace frame, see
/// `reflect::native_transparent`). Only exact cases unwrap: a callable target, a small
/// argument list, and for `apply` an `undefined`/`null` list or an ordinary Array whose
/// elements are all own data properties (reading them runs no code). On `Some` the argument
/// window is consumed (left `Undefined`); on `None` it is untouched.
unsafe fn unwrap_adaptor(
    i: &Interp,
    callee: &Value,
    this: &Value,
    args: *mut Value,
    argc: usize,
    buf: &mut ArgBuf,
) -> Option<(Value, Value)> {
    use crate::value::Callable;
    let Value::Obj(o) = callee else { return None };
    let b = o.try_borrow().ok()?;
    if !b.ic_plain.get() {
        return None;
    }
    let take = |k: usize| std::ptr::replace(args.add(k), Value::Undefined);
    match &b.call {
        Callable::Native(fp) if *fp as usize == crate::builtins::nf_function_call as usize => {
            if argc > ARGBUF + 1 || !this.is_callable() {
                return None;
            }
            let t = if argc > 0 { take(0) } else { Value::Undefined };
            for k in 1..argc {
                buf.push(take(k));
            }
            Some((this.clone(), t))
        }
        Callable::Native(fp) if *fp as usize == crate::builtins::nf_function_apply as usize => {
            if argc > 2 || !this.is_callable() {
                return None;
            }
            if argc == 2 {
                match &*args.add(1) {
                    Value::Undefined | Value::Null => {}
                    Value::Obj(a) => {
                        if !i.ordinary_get_ptr(crate::value::Gc::as_ptr(a) as usize)
                            || i.mapped_arguments.contains_key(&(crate::value::Gc::as_ptr(a) as usize))
                        {
                            return None;
                        }
                        let ab = a.try_borrow().ok()?;
                        if !matches!(ab.exotic, crate::value::Exotic::Array) {
                            return None;
                        }
                        let len = ab.props.length_property()?.num_value()?;
                        if !(0.0..=ARGBUF as f64).contains(&len) || len.fract() != 0.0 {
                            return None;
                        }
                        let len = len as u32;
                        if (0..len).any(|k| ab.props.get_index(k).is_none_or(|p| p.accessor())) {
                            return None;
                        }
                        for k in 0..len {
                            buf.push(ab.props.get_index(k).expect("checked").value());
                        }
                    }
                    _ => return None,
                }
                drop(take(1));
            }
            let t = if argc > 0 { take(0) } else { Value::Undefined };
            Some((this.clone(), t))
        }
        Callable::Bound(bc) => {
            if bc.args.len() + argc > ARGBUF {
                return None;
            }
            for a in &bc.args {
                buf.push(a.clone());
            }
            for k in 0..argc {
                buf.push(take(k));
            }
            Some((Value::Obj(bc.target.clone()), bc.this.clone()))
        }
        _ => None,
    }
}

/// [`Helper::Call`]'s flag bit: the result feeds straight into an `await`.
pub(crate) const CALL_AWAIT: u32 = 2;
/// [`Helper::Call`]'s flag bit: the op is a proper tail call (`Op::TailCall`). Past
/// `TAIL_NEST` in a frame that may tail-call, the call is parked in `Interp::pending_tail` for
/// the caller's trampoline and `undefined` stands in for its result (the `Return` after the op
/// returns it), as the interpreter's generic call-out of the op does.
pub(crate) const CALL_TAIL: u32 = 4;

pub(crate) unsafe extern "C" fn call(
    f: *mut JitFrame,
    base: u32,
    argc: u32,
    flags: u32,
) -> u32 {
    let s = (*f).stack;
    let (base, argc) = (base as usize, argc as usize);
    let (this, callee, at) = if flags & 1 != 0 {
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
    if flags & CALL_TAIL != 0
        && i.tco_ok
        && i.depth > crate::bytecode::TAIL_NEST
        && callee.is_callable()
        && i.leaf_native(&callee).is_none()
    {
        let args = (0..argc).map(|k| std::ptr::replace(s.add(at + k), Value::Undefined)).collect();
        i.pending_tail = Some(Box::new((callee, this, args)));
        std::ptr::write(s.add(base), Value::Undefined);
        return STATUS_OK;
    }
    let mut buf = ArgBuf::new();
    let (callee, this, argp, argc) = match unwrap_adaptor(i, &callee, &this, s.add(at), argc, &mut buf) {
        Some((target, t)) => (target, t, buf.as_mut_ptr(), buf.len),
        None => (callee, this, s.add(at), argc),
    };
    // A plain native: straight to it (`Interp::call`'s bookkeeping without its dispatch).
    if let Some(nat) = i.leaf_native(&callee) {
        let args = std::slice::from_raw_parts(argp, argc);
        let r = i.call_native_fast(nat, this, args);
        drop(callee);
        for k in 0..argc {
            *argp.add(k) = Value::Undefined;
        }
        return match r {
            Ok(v) => {
                std::ptr::write(s.add(base), v);
                STATUS_OK
            }
            Err(e) => fail(f, e),
        };
    }
    let r = match crate::bytecode::inline_callee(i, &callee) {
        Some(c) => match super::native_code(&c.chunk) {
            // A sloppy callee whose `f.arguments` may be read runs as a frame, which records
            // its activation (see `bytecode::reflect`).
            Some(native) if !(c.chunk.reflect_args && crate::bytecode::reflect::enabled()) => {
                call_native(i, callee, this, argp, argc, c, native)
            }
            _ => call_frame(i, callee, this, argp, argc, c),
        },
        None => {
            // The arguments stay owned by the frame's stack for the call (they root their
            // objects there, like the interpreter's operand stack does), then are dropped.
            let args = std::slice::from_raw_parts(argp, argc);
            let r = if callee.is_callable() {
                // `await f()`: an async callee may skip its promise (the interpreter's fusion).
                let fused = flags & CALL_AWAIT != 0;
                if fused {
                    i.note_await_call(&callee);
                }
                let r = i.call(callee, this, args);
                if fused {
                    i.clear_await_call();
                }
                r
            } else {
                // The TypeError names the callee as written (`bytecode::call_error`).
                i.call(callee.clone(), this, args)
                    .map_err(|e| crate::bytecode::site_call_error(i, &*(*f).chunk, &callee, e))
            };
            for k in 0..argc {
                *argp.add(k) = Value::Undefined;
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
                // The strict tail caller is gone: hidden from `fn.caller` (like the
                // interpreter's trampolines).
                let site = i.cur_site;
                i.cur_site = site | crate::interpreter::frames::SITE_NATIVE;
                r = i.call(g, t, &a);
                i.cur_site = site;
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
    super::sync_frames(i);
    i.fn_frames.push(crate::interpreter::FnFrame {
        fn_ptr,
        coro: i.cur_coro,
        caller_site: std::mem::replace(&mut i.cur_site, crate::interpreter::frames::NO_SITE),
        strict: c.strict,
        construct: false,
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
        Some(_) if chunk.virt_base.is_none() => {
            Some(i.make_compiled_arguments_object(arg_slice, &env))
        }
        _ => None,
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
    let r = match super::enter(i, chunk, &env, slots, &mut stack, &mut pc, &this_val, None, &native, 0)
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
    i.cur_site = i.fn_frames[top].caller_site;
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
                // The strict tail caller is gone: hidden from `fn.caller` (like the
                // interpreter's trampolines).
                let site = i.cur_site;
                i.cur_site = site | crate::interpreter::frames::SITE_NATIVE;
                r = i.call(g, t, &a);
                i.cur_site = site;
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

// ---- Array.prototype.push ---------------------------------------------------------------------

/// Whether `recv.push` reads the realm's intrinsic `Array.prototype.push`: `recv` is an
/// ordinary Array without an own `push` whose prototype is `Array.prototype`, one realm, and
/// `Array.prototype`'s own data property `push` holds the builtin.
unsafe fn arr_push_check(f: *mut JitFrame, recv: &Value) -> Option<()> {
    let Value::Obj(o) = recv else { return None };
    let i = &*(*f).interp;
    (!i.multi_realm() && crate::bytecode::iter_fast::array_push_ok(i, o, crate::builtins::nf_array_push))
        .then_some(())
}

pub(crate) unsafe extern "C" fn arr_push_guard(f: *mut JitFrame, pc: u32, recv: *const Value) -> u32 {
    if arr_push_check(f, &*recv).is_some() {
        return 1;
    }
    let key = ((*f).chunk as usize, pc);
    MATH_FAILED.with(|s| {
        s.borrow_mut().insert(key);
    });
    0
}

pub(crate) unsafe extern "C" fn arr_push(f: *mut JitFrame, recv: *mut Value, arg: *mut Value) -> u32 {
    let i = &mut *(*f).interp;
    let this = crate::value::take_value_words(recv);
    let v = crate::value::take_value_words(arg);
    let Value::Obj(o) = &this else { unreachable!("guarded push receiver") };
    let v = match crate::builtins::array_push_one(i, o, v) {
        Ok(n) => {
            std::ptr::write(recv, Value::Num(n));
            return STATUS_OK;
        }
        Err(v) => v,
    };
    // The generic builtin may run setters or proxy traps: the caller's caches go stale.
    match i.call_native_fast(crate::builtins::nf_array_push, this, std::slice::from_ref(&v)) {
        Ok(r) => {
            std::ptr::write(recv, r);
            STATUS_OK_SLOW
        }
        Err(e) => fail(f, e),
    }
}

// ---- String intrinsics -------------------------------------------------------------------------

/// The `String.prototype` method called `name` the translator emits inline.
pub(crate) fn str_intrinsic(name: &str) -> Option<crate::value::NativeFn> {
    match name {
        "charCodeAt" => Some(crate::builtins::nf_char_code_at),
        _ => None,
    }
}

/// Whether `GetMethod(m, _)` at `pc` on `recv` reads the intrinsic method: `recv` is a
/// primitive String (whose own properties are indices and `length`, so the lookup goes to
/// `String.prototype`), one realm, and `String.prototype`'s own data property `m` holds the
/// builtin.
unsafe fn str_check(f: *mut JitFrame, pc: usize, recv: &Value) -> Option<()> {
    if !matches!(recv, Value::Str(_)) {
        return None;
    }
    let i = &*(*f).interp;
    if i.multi_realm() {
        return None;
    }
    let chunk = &*(*f).chunk;
    let Op::GetMethod(m, _) = *chunk.ops.get(pc)? else {
        return None;
    };
    let name = chunk.names.get(m as usize)?;
    let want = str_intrinsic(name)?;
    let fv = {
        let sb = i.string_proto.try_borrow().ok()?;
        // `String.prototype` is itself a String wrapper (of ""): its named properties are
        // ordinary.
        if !matches!(sb.exotic, crate::value::Exotic::None | crate::value::Exotic::StrWrap) {
            return None;
        }
        let p = sb.props.entry_at(sb.props.slot_of(name)?)?;
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

pub(crate) unsafe extern "C" fn str_guard(f: *mut JitFrame, pc: u32, recv: *const Value) -> u32 {
    if str_check(f, pc as usize, &*recv).is_some() {
        return 1;
    }
    let key = ((*f).chunk as usize, pc);
    MATH_FAILED.with(|s| {
        s.borrow_mut().insert(key);
    });
    0
}

/// `String.prototype.charCodeAt` on String `s` with Number `pos` (its ToNumber is itself).
pub(crate) unsafe extern "C" fn str_code_at(f: *mut JitFrame, s: *const Value, pos: f64) -> f64 {
    let Value::Str(s) = &*s else { return f64::NAN };
    let idx = if pos.is_nan() { 0.0 } else { pos.trunc() };
    if idx < 0.0 || !idx.is_finite() {
        return f64::NAN;
    }
    match (*(*f).interp).unit_at(s, idx as usize) {
        Some(u) => f64::from(u),
        None => f64::NAN,
    }
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
    let (it, nx) = (&mut *(*f).slots.add(is), &mut *(*f).slots.add(ns));
    // A protocol-free for-of state (bytecode::iter_fast): no user code, nothing to invalidate.
    if is != ns {
        match crate::bytecode::iter_fast::step(i, it, nx) {
            Some(Some(v)) => {
                std::ptr::write(dst, v);
                return 1;
            }
            Some(None) => {
                std::ptr::write(dst, Value::Undefined);
                return 0;
            }
            None => {}
        }
    }
    let (it, nx) = (&*it, &*nx);
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
    /// A site without a view cache fed typed arrays often enough: exit before the op and
    /// recompile it with a cache (see [`super::ta_hot`]).
    pub const RECOMPILE: u32 = 0xfe;
}

/// Fresh typed-array views an element site without a view cache takes before it asks for a
/// recompile with one.
const TA_HOT_VIEWS: u32 = 128;

thread_local! {
    /// Fresh views taken per uncached element site `(chunk, pc)`; `u32::MAX` once the site
    /// is hot (compiled with a view cache from then on).
    static TA_VIEWS: std::cell::RefCell<std::collections::HashMap<(usize, u32), u32>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    /// The site whose [`ta_code::RECOMPILE`] exit is pending (taken by the exit handling).
    static TA_EXIT: std::cell::Cell<(usize, u32)> = const { std::cell::Cell::new((0, u32::MAX)) };
}

/// Whether element site `pc` of `chunk` was fed typed arrays through fresh views (no cache)
/// often: it is compiled typed-array first, with a view cache.
pub(crate) fn ta_hot(chunk: &Chunk, pc: usize) -> bool {
    TA_VIEWS.with(|m| {
        m.borrow()
            .get(&(chunk as *const Chunk as usize, pc as u32))
            .is_some_and(|&n| n == u32::MAX)
    })
}

/// Whether the resume exit at `pc` is the [`ta_code::RECOMPILE`] exit of that site (then the
/// code is retired and recompiled).
pub(crate) fn take_ta_exit(chunk: &Chunk, pc: usize) -> bool {
    let key = (chunk as *const Chunk as usize, pc as u32);
    TA_EXIT.with(|c| {
        if c.get() == key {
            c.set((0, u32::MAX));
            true
        } else {
            false
        }
    })
}

pub(crate) unsafe extern "C" fn ta_view(f: *mut JitFrame, v: *const Value, write: u32) -> u32 {
    let code = ta_view_code(f, v, write & 1);
    // An uncached site (`write` bit 1, its pc above): count the fresh views it takes.
    if code != 0 && write & 2 != 0 {
        let key = ((*f).chunk as usize, write >> 2);
        let hot = TA_VIEWS.with(|m| {
            let mut m = m.borrow_mut();
            if m.len() >= 1 << 16 && !m.contains_key(&key) {
                return false;
            }
            let n = m.entry(key).or_insert(0);
            if *n == u32::MAX {
                return false;
            }
            *n += 1;
            if *n < TA_HOT_VIEWS {
                return false;
            }
            *n = u32::MAX;
            true
        });
        if hot {
            TA_EXIT.with(|c| c.set(key));
            return ta_code::RECOMPILE;
        }
    }
    code
}

unsafe fn ta_view_code(f: *mut JitFrame, v: *const Value, write: u32) -> u32 {
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
    // `Interp::get`'s typed-array case: only while `length` reaches the intrinsic getter.
    match o.try_borrow() {
        Ok(b) if i.ta_meta_intrinsic(&b, 0) => i.ta_len(info).unwrap_or(0) as f64,
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
    let i = &mut *(*f).interp;
    let mut r = finish_exit(i, nf, word, entry as usize);
    // A proper tail call unwound out of the callee (`Interp::call`'s trampoline).
    while r.is_ok() {
        match i.pending_tail.take() {
            Some(bx) => {
                let (g, t, a) = *bx;
                // The strict tail caller is gone: hidden from `fn.caller` (like the
                // interpreter's trampolines).
                let site = i.cur_site;
                i.cur_site = site | crate::interpreter::frames::SITE_NATIVE;
                r = i.call(g, t, &a);
                i.cur_site = site;
            }
            None => break,
        }
    }
    match r {
        Ok(v) => {
            std::ptr::write((*nf).stack, v);
            STATUS_OK
        }
        Err(e) => fail(f, e),
    }
}

/// The result of a directly called frame `nf` whose code exited with `word` (entered at
/// `entry`): the interpreter finishes the call on the frame's slots, which are then dropped
/// (a pending proper tail call is the caller's to run).
pub(crate) unsafe fn finish_exit(
    i: &mut Interp,
    nf: *mut JitFrame,
    word: u64,
    entry: usize,
) -> Result<Value, Abrupt> {
    use crate::bytecode::{drive_vm, VmStep};
    let chunk = &*(*nf).chunk;
    let env = &*(*nf).env;
    let this_val = &*(*nf).this_val;
    let slots = std::slice::from_raw_parts_mut((*nf).slots, chunk.n_slots);
    let area = (*nf).stack;
    let native = super::find_native(chunk, entry);
    // Exits of replaced code are not counted against the current code.
    let current = chunk.jit.dentry.get() == entry;
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
    // The header's 16-byte-aligned words stay trivially droppable (see `JitFrame`).
    (*nf).exit_depth = 0;
    (*nf).exit_hset = 0;
    match r {
        Ok(VmStep::Done(v)) => Ok(v),
        Ok(VmStep::Await(_)) => Err(i.throw("TypeError", "compiled code awaited in a call")),
        Err(e) => Err(e),
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
    let i = &mut *(*f).interp;
    if let Some(fr) = i.fn_frames.pop() {
        i.cur_site = fr.caller_site;
    }
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

pub(crate) unsafe extern "C" fn site_rebind(
    f: *mut JitFrame,
    cell: *mut super::SiteCell,
    v: *const Value,
) -> u32 {
    let i = &*(*f).interp;
    let v = &*v;
    let Some(ic) = crate::bytecode::inline_callee(i, v) else {
        return 0;
    };
    if std::rc::Rc::as_ptr(&ic.chunk) as usize != (*cell).chunk {
        return 0;
    }
    let Some((env, fn_ptr)) = super::callee_env_addr(v) else {
        return 0;
    };
    (*cell).word = super::value_word(v);
    (*cell).fn_ptr = fn_ptr;
    (*cell).env = env;
    (*cell).pin = v.clone();
    1
}

pub(crate) unsafe extern "C" fn bound_this(
    f: *mut JitFrame,
    cell: *mut super::SiteCell,
    v: *const Value,
    need_obj: u32,
) -> *const Value {
    let Value::Obj(o) = &*v else { return std::ptr::null() };
    let Ok(b) = o.try_borrow() else { return std::ptr::null() };
    let crate::value::Callable::Bound(bc) = &b.call else { return std::ptr::null() };
    if !bc.args.is_empty() || (need_obj != 0 && !matches!(bc.this, Value::Obj(_))) {
        return std::ptr::null();
    }
    let target = crate::value::Gc::as_ptr(&bc.target) as usize;
    if target != (*cell).fn_ptr && site_rebind(f, cell, &Value::Obj(bc.target.clone())) == 0 {
        return std::ptr::null();
    }
    // The bound callable is boxed: its address is stable while `v` lives.
    &bc.this as *const Value
}

/// The Array an `apply` list names when its elements `0..length` are all own data properties
/// (reading them runs no code), with that length.
fn apply_list(i: &Interp, v: &Value) -> Option<(crate::value::Gc, u32)> {
    let Value::Obj(a) = v else { return None };
    let p = crate::value::Gc::as_ptr(a) as usize;
    if !i.ordinary_get_ptr(p) || i.mapped_arguments.contains_key(&p) {
        return None;
    }
    let ab = a.try_borrow().ok()?;
    if !matches!(ab.exotic, crate::value::Exotic::Array) {
        return None;
    }
    let len = ab.props.length_property()?.num_value()?;
    if !(0.0..=65536.0).contains(&len) || len.fract() != 0.0 {
        return None;
    }
    let len = len as u32;
    if (0..len).any(|k| ab.props.get_index(k).is_none_or(|p| p.accessor())) {
        return None;
    }
    drop(ab);
    Some((a.clone(), len))
}

pub(crate) unsafe extern "C" fn apply_ok(f: *mut JitFrame, v: *const Value) -> u32 {
    let v = &*v;
    (matches!(v, Value::Undefined | Value::Null) || apply_list(&*(*f).interp, v).is_some()) as u32
}

pub(crate) unsafe extern "C" fn apply_seed(v: *const Value, slots: *mut Value, n: u32) {
    let Value::Obj(a) = &*v else { return };
    let Ok(ab) = a.try_borrow() else { return };
    for k in 0..n {
        match ab.props.get_index(k) {
            Some(p) => std::ptr::write(slots.add(k as usize), p.value()),
            None => break,
        }
    }
}

pub(crate) unsafe extern "C" fn new_this(
    f: *mut JitFrame,
    cell: *const super::SiteCell,
    out: *mut Value,
) {
    let i = &mut *(*f).interp;
    let ctor = &(*cell).pin;
    let Value::Obj(o) = ctor else {
        std::ptr::write(out, Value::Undefined);
        return;
    };
    // A user function's `prototype` is an own data property (non-configurable): read it in
    // place rather than through a full `[[Get]]`.
    let own = o
        .try_borrow()
        .ok()
        .and_then(|b| b.props.get("prototype").filter(|p| !p.accessor()).map(|p| p.value()));
    let proto = match own {
        Some(v) => v,
        None => i.get_member(ctor, "prototype").unwrap_or(Value::Undefined),
    };
    let proto = match proto {
        Value::Obj(p) => p,
        _ => i.object_proto.clone(),
    };
    let cap = i.learned_construct_capacity(o);
    let inst = crate::value::Object::new_with_capacity(Some(proto), cap);
    std::ptr::write(out, Value::Obj(inst));
    let old = std::mem::replace(&mut i.new_target, ctor.clone());
    drop(old);
}

pub(crate) unsafe extern "C" fn new_done(
    f: *mut JitFrame,
    cell: *const super::SiteCell,
    inst: *mut Value,
    res: *mut Value,
    status: u32,
) {
    let i = &mut *(*f).interp;
    let nt = std::mem::take(&mut i.new_target);
    drop(nt);
    let inst = std::ptr::replace(inst, Value::Undefined);
    if status != STATUS_OK {
        return;
    }
    if let (Value::Obj(c), Value::Obj(io)) = (&(*cell).pin, &inst) {
        i.observe_construct_capacity(c, io);
    }
    if !matches!(*res, Value::Obj(_)) {
        drop(std::ptr::replace(res, inst));
    }
}

pub(crate) unsafe extern "C" fn gc_poll(f: *mut JitFrame) -> u32 {
    match (*(*f).interp).gc_check_amortized() {
        Ok(()) => STATUS_OK,
        Err(e) => fail(f, e),
    }
}

pub(crate) unsafe extern "C" fn append_local(f: *mut JitFrame, d: u32, y: *const Value) -> u32 {
    let chunk = &*(*f).chunk;
    let d = d as usize;
    if d >= chunk.n_slots {
        return 0;
    }
    let slots = std::slice::from_raw_parts_mut((*f).slots, chunk.n_slots);
    // `y` may be another slot: read it before the slots are borrowed mutably.
    let y = (*y).clone();
    crate::bytecode::append_to_slot(slots, d, &y) as u32
}

pub(crate) unsafe extern "C" fn elem_get(
    f: *mut JitFrame,
    obj: *mut Value,
    key: f64,
    dst: *mut Value,
    consume: u32,
) -> u32 {
    let i = &mut *(*f).interp;
    let v = match &*obj {
        Value::Str(s) => crate::bytecode::str_index_fast(s, key),
        Value::Obj(o) => i.fast_get_elem(o, key),
        _ => None,
    };
    let Some(v) = v else { return 0 };
    if consume != 0 {
        drop(std::ptr::replace(obj, Value::Undefined));
    }
    std::ptr::write(dst, v);
    1
}

pub(crate) unsafe extern "C" fn strict_eq(f: *mut JitFrame, a: *mut Value, b: *mut Value) -> u32 {
    let a = std::ptr::replace(a, Value::Undefined);
    let b = std::ptr::replace(b, Value::Undefined);
    (*(*f).interp).strict_equals(&a, &b) as u32
}

pub(crate) unsafe extern "C" fn strict_eq_ref(
    f: *mut JitFrame,
    a: *const Value,
    b: *const Value,
) -> u32 {
    let (a, b) = (&*a, &*b);
    if matches!(a, Value::Empty) || matches!(b, Value::Empty) {
        return 2;
    }
    (*(*f).interp).strict_equals(a, b) as u32
}

pub(crate) unsafe extern "C" fn unpack_clone(dst: *mut Value, bits: u64, consume: u32) {
    // SAFETY: `bits` is a live property's word (`PackedValue` is a transparent `u64`); the
    // `ManuallyDrop` keeps the property's own reference, `unpack` takes a new one.
    let word = std::mem::ManuallyDrop::new(std::mem::transmute::<u64, crate::value::PackedValue>(bits));
    let v = word.unpack();
    if consume != 0 {
        drop(std::ptr::replace(dst, Value::Undefined));
    }
    std::ptr::write(dst, v);
}

pub(crate) unsafe extern "C" fn make_lit(f: *mut JitFrame, pc: u32, base: *mut Value, n: u32) {
    let i = &mut *(*f).interp;
    let chunk = &*(*f).chunk;
    let n = n as usize;
    // The values move out (each slot left `Undefined`), in order.
    let vals = (0..n).map(|k| std::ptr::replace(base.add(k), Value::Undefined));
    let v = match chunk.ops.get(pc as usize) {
        Some(&Op::MakeObject(start, count, tidx)) if count as usize == n => {
            let keys = &chunk.names[start as usize..start as usize + n];
            if tidx != u32::MAX {
                i.make_plain_object_templated(&chunk.obj_maps[tidx as usize], keys, vals)
            } else {
                let values: Vec<Value> = vals.collect();
                i.make_plain_object_vm(keys, values)
            }
        }
        _ => i.make_array_iter(vals),
    };
    std::ptr::write(base, v);
}

pub(crate) unsafe extern "C" fn alloc_obj(
    f: *mut JitFrame,
    tpl: *const crate::value::Props,
    base: *mut Value,
    n: u32,
) {
    let i = &*(*f).interp;
    let n = n as usize;
    // SAFETY: the translator checked `fast_template` and the entry count (see `lit_template`).
    let g = crate::value::Object::alloc_from_template(&*tpl, Some(i.object_proto.clone()), base, n);
    for k in 1..n {
        std::ptr::write(base.add(k), Value::Undefined);
    }
    std::ptr::write(base, Value::Obj(g));
}

pub(crate) unsafe extern "C" fn alloc_arr(f: *mut JitFrame, base: *mut Value, n: u32) {
    let i = &*(*f).interp;
    let n = n as usize;
    let v = i.make_array_moved(base, n);
    for k in 1..n {
        std::ptr::write(base.add(k), Value::Undefined);
    }
    std::ptr::write(base, v);
}

unsafe fn frame_slots<'a>(f: *mut JitFrame) -> &'a mut [Value] {
    std::slice::from_raw_parts_mut((*f).slots, (*(*f).chunk).n_slots)
}

pub(crate) unsafe extern "C" fn blk_new(f: *mut JitFrame, dst: u32, parent: u32) {
    use crate::bytecode::block_env;
    block_env::new_env(&*(*f).env, frame_slots(f), dst as u16, parent as u16);
}

pub(crate) unsafe extern "C" fn blk_copy(f: *mut JitFrame, s: u32) {
    crate::bytecode::block_env::copy(frame_slots(f), s as u16);
}

pub(crate) unsafe extern "C" fn blk_decl(f: *mut JitFrame, s: u32, n: u32, k: u32) {
    let name = &(&*(*f).chunk).names[n as usize];
    crate::bytecode::block_env::declare(frame_slots(f), s as u16, name, k != 0);
}

pub(crate) unsafe extern "C" fn blk_load(f: *mut JitFrame, s: u32, n: u32, dst: *mut Value) -> u32 {
    let name = &(&*(*f).chunk).names[n as usize];
    let i = &mut *(*f).interp;
    match crate::bytecode::block_env::load(i, frame_slots(f), s as u16, name) {
        Ok(v) => {
            std::ptr::write(dst, v);
            STATUS_OK
        }
        Err(e) => {
            std::ptr::write(dst, Value::Undefined);
            fail(f, e)
        }
    }
}

pub(crate) unsafe extern "C" fn blk_store(
    f: *mut JitFrame,
    s: u32,
    n: u32,
    src: *mut Value,
    init: u32,
) -> u32 {
    let name = &(&*(*f).chunk).names[n as usize];
    let i = &mut *(*f).interp;
    let v = std::ptr::replace(src, Value::Undefined);
    match crate::bytecode::block_env::store(i, frame_slots(f), s as u16, name, v, init != 0) {
        Ok(()) => STATUS_OK,
        Err(e) => fail(f, e),
    }
}

pub(crate) unsafe extern "C" fn blk_update(f: *mut JitFrame, pc: u32, dst: *mut Value) -> u32 {
    let chunk = &*(*f).chunk;
    let Some(&Op::BlkUpdate(s, n, kind)) = chunk.ops.get(pc as usize) else {
        let e = (*(*f).interp).throw("TypeError", "compiled code ran an unsupported op");
        return fail(f, e);
    };
    let i = &mut *(*f).interp;
    let mut out: Vec<Value> = Vec::new();
    let r = crate::bytecode::block_env::update(
        i,
        &mut out,
        frame_slots(f),
        s,
        &chunk.names[n as usize],
        kind,
    );
    if let Some(v) = out.pop() {
        std::ptr::write(dst, v);
    }
    match r {
        Ok(()) => STATUS_OK,
        Err(e) => fail(f, e),
    }
}

pub(crate) unsafe extern "C" fn make_closure_in(
    f: *mut JitFrame,
    s: u32,
    fidx: u32,
    name: u32,
    dst: *mut Value,
) {
    let i = &mut *(*f).interp;
    let env = crate::bytecode::block_env::env_of(&frame_slots(f)[s as usize]);
    let v = crate::bytecode::make_closure(i, &*(*f).chunk, fidx, name, &env);
    std::ptr::write(dst, v);
}

pub(crate) unsafe extern "C" fn make_closure(f: *mut JitFrame, fidx: u32, name: u32, dst: *mut Value) {
    let i = &mut *(*f).interp;
    let v = crate::bytecode::make_closure(i, &*(*f).chunk, fidx, name, &*(*f).env);
    std::ptr::write(dst, v);
}

pub(crate) unsafe extern "C" fn make_rest(f: *mut JitFrame, dst: *mut Value, src: *mut Value, n: u32) {
    let i = &*(*f).interp;
    let vals = (0..n as usize).map(|k| std::ptr::replace(src.add(k), Value::Undefined));
    std::ptr::write(dst, i.make_array_iter(vals));
}
