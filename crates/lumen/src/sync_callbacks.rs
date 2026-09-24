//! Synchronous, non-escaping callback parameters.
//!
//! Many natives take a function argument that they call only *during* their own invocation and
//! never store: `Array.prototype.map`'s callbackfn, a `sort` comparator, a `replace` function,
//! the `JSON.parse` reviver... For such a parameter the callee can never observe the closure
//! object after the call returns, which lets an optimizing tier (see `docs/sync-callbacks.md`)
//! run the callback in the caller's frame, keep its captures in caller slots, and skip
//! allocating the closure at all when the native is inlined as a loop.
//!
//! Two sources feed one query, [`Interp::sync_callback_params`], which is keyed by native-fn
//! identity (the `fn` pointer behind `Callable::Native`):
//! - `#[lumen::op]` functions: a parameter typed [`SyncFn<'call>`] sets bit
//!   `OP_SYNC_CB_SHIFT + i` (JS argument `i`) in [`OpDesc::flags`](crate::embed::OpDesc);
//!   [`Interp::op_desc_of`] finds the descriptor from the fn pointer.
//! - builtins: [`BUILTINS`], resolved once to fn pointers by [`record_builtins`] at realm setup.

#[cfg(feature = "embed")]
use crate::embed_convert::{ArgCx, FromJs, OpError, Slot};
use crate::interpreter::Interp;
use crate::value::{Callable, Value};
use std::collections::HashMap;
#[cfg(feature = "embed")]
use std::marker::PhantomData;
use std::sync::{Mutex, OnceLock};

/// `OpDesc::flags` bit of JS argument 0 being a [`SyncFn`]; argument `i` is this `<< i`
/// (`i < 24`).
#[allow(dead_code)]
pub const OP_SYNC_CB_SHIFT: u32 = 8;

#[cfg(feature = "embed")]
/// A callable argument the op may call only while it runs: bound to the op invocation
/// (`'call`), not `'static`, not `Clone`, not `Send`, and not convertible to a `Value`, so it
/// cannot be stored, returned or handed to another thread. Declaring a parameter as
/// `SyncFn<'_>` (rather than `JsFunction`) is the op's promise that the callback does not
/// escape; the macro records it (see the module docs) for the optimizing tier.
pub struct SyncFn<'call> {
    f: &'call Value,
    /// `!Send + !Sync`.
    _not_send: PhantomData<*const ()>,
}

#[cfg(feature = "embed")]
impl<'call> SyncFn<'call> {
    /// Call it; a JS exception comes back as `OpError::thrown`, so `?` rethrows it unchanged.
    pub fn call(&self, ctx: &mut Interp, this: Value, args: &[Value]) -> Result<Value, OpError> {
        ctx.invoke(self.f.clone(), this, args).map_err(OpError::thrown)
    }
}

#[cfg(feature = "embed")]
impl<'a> FromJs<'a> for SyncFn<'a> {
    fn from_js(cx: &'a ArgCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Value> {
        if v.is_callable() {
            Ok(SyncFn { f: v, _not_send: PhantomData })
        } else {
            Err(cx.type_error(at, "must be a function"))
        }
    }
}

/// Builtins whose argument(s) are sync non-escaping callbacks: `(path, argument bitmask)`.
/// A path is `Global.prop...`; a leading `%TypedArray%` is `Object.getPrototypeOf(Int8Array)`.
pub const BUILTINS: &[(&str, u32)] = &[
    ("Array.prototype.forEach", 1),
    ("Array.prototype.map", 1),
    ("Array.prototype.filter", 1),
    ("Array.prototype.some", 1),
    ("Array.prototype.every", 1),
    ("Array.prototype.find", 1),
    ("Array.prototype.findIndex", 1),
    ("Array.prototype.findLast", 1),
    ("Array.prototype.findLastIndex", 1),
    ("Array.prototype.reduce", 1),
    ("Array.prototype.reduceRight", 1),
    ("Array.prototype.flatMap", 1),
    ("Array.prototype.sort", 1),
    ("Array.prototype.toSorted", 1),
    ("Array.from", 2),
    ("%TypedArray%.prototype.forEach", 1),
    ("%TypedArray%.prototype.map", 1),
    ("%TypedArray%.prototype.filter", 1),
    ("%TypedArray%.prototype.some", 1),
    ("%TypedArray%.prototype.every", 1),
    ("%TypedArray%.prototype.find", 1),
    ("%TypedArray%.prototype.findIndex", 1),
    ("%TypedArray%.prototype.findLast", 1),
    ("%TypedArray%.prototype.findLastIndex", 1),
    ("%TypedArray%.prototype.reduce", 1),
    ("%TypedArray%.prototype.reduceRight", 1),
    ("%TypedArray%.prototype.sort", 1),
    ("%TypedArray%.prototype.toSorted", 1),
    ("%TypedArray%.from", 2),
    ("String.prototype.replace", 2),
    ("String.prototype.replaceAll", 2),
    ("Map.prototype.forEach", 1),
    ("Set.prototype.forEach", 1),
    ("JSON.parse", 2),
    ("JSON.stringify", 2),
];

fn table() -> &'static Mutex<HashMap<usize, u32>> {
    static T: OnceLock<Mutex<HashMap<usize, u32>>> = OnceLock::new();
    T.get_or_init(Default::default)
}

fn resolve(i: &mut Interp, path: &str) -> Option<Value> {
    let mut parts = path.split('.');
    let head = parts.next()?;
    let mut v = if head == "%TypedArray%" {
        let g = Value::Obj(i.global.clone());
        let i8 = i.get_member(&g, "Int8Array").ok()?;
        let p = i8.as_obj()?.borrow().proto.clone()?;
        Value::Obj(p)
    } else {
        let g = Value::Obj(i.global.clone());
        i.get_member(&g, head).ok()?
    };
    for p in parts {
        v = i.get_member(&v, p).ok()?;
    }
    Some(v)
}

/// Resolve [`BUILTINS`] to native fn pointers (realm setup; the pointers are process-wide, so
/// this runs once). A builtin that is not a plain `Callable::Native` is skipped.
pub(crate) fn record_builtins(i: &mut Interp) {
    static DONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if DONE.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let mut found = Vec::new();
    for &(path, mask) in BUILTINS {
        if let Some(Value::Obj(o)) = resolve(i, path) {
            if let Callable::Native(f) = &o.borrow().call {
                found.push((*f as usize, mask));
            }
        }
    }
    let mut t = table().lock().unwrap_or_else(|e| e.into_inner());
    t.extend(found);
}

#[allow(dead_code)]
impl Interp {
    /// Bitmask of `callee`'s arguments that are sync non-escaping callbacks (bit `i` = JS
    /// argument `i`), by native-fn identity: a marked builtin ([`BUILTINS`]) or a
    /// `#[lumen::op]` with [`SyncFn`] parameters. `0` for anything else (including every
    /// user function).
    pub fn sync_callback_params(&self, callee: &Value) -> u32 {
        let Some(o) = callee.as_obj() else { return 0 };
        let f = match &o.borrow().call {
            Callable::Native(f) => *f,
            _ => return 0,
        };
        #[cfg(feature = "embed")]
        if let Some(d) = self.op_desc_of(callee) {
            return d.flags >> OP_SYNC_CB_SHIFT;
        }
        sync_callback_params_of(f)
    }
}

/// [`Interp::sync_callback_params`] for a builtin, by its fn pointer alone.
#[allow(dead_code)]
pub fn sync_callback_params_of(f: crate::value::NativeFn) -> u32 {
    table()
        .lock()
        .map(|t| t.get(&(f as usize)).copied().unwrap_or(0))
        .unwrap_or(0)
}
