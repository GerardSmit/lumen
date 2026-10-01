//! V8's `--allow-natives-syntax`: `%Name(args)` calls a runtime function. The flag is
//! process-wide (as V8's flags are) and only changes how later source parses; a realm gets its
//! runtime functions (bindings named `%Name` in its global scope, which no identifier can
//! spell) when the embedder enables the flag for it.

use super::*;
use crate::interpreter::Binding;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

static ALLOWED: AtomicBool = AtomicBool::new(false);

/// Whether `%Name(...)` parses as a runtime call.
pub(crate) fn syntax_allowed() -> bool {
    ALLOWED.load(AtomicOrdering::Relaxed)
}

/// The runtime functions lumen implements.
pub(crate) const NAMES: &[&str] = &[
    "IsSmi",
    "CollectGarbage",
    "DebugPrint",
    "HaveSameMap",
    "GetUndetectable",
    "HasFastProperties",
];

fn is_smi(_i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    // V8's 64-bit Smi range without pointer compression (Node's build): a 32-bit integer.
    Ok(Value::Bool(match a.first() {
        Some(Value::Num(n)) => {
            n.fract() == 0.0 && *n >= i32::MIN as f64 && *n <= i32::MAX as f64 && !(*n == 0.0 && n.is_sign_negative())
        }
        _ => false,
    }))
}

fn collect_garbage(i: &mut Interp, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    i.collect_garbage_for_host();
    Ok(Value::Undefined)
}

fn debug_print(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    let v = a.first().cloned().unwrap_or(Value::Undefined);
    let text = i.console_format(std::slice::from_ref(&v)).unwrap_or_default();
    println!("DebugPrint: {text}");
    Ok(v)
}

fn have_same_map(_i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    Ok(Value::Bool(match (a.first(), a.get(1)) {
        (Some(Value::Obj(x)), Some(Value::Obj(y))) => {
            let (x, y) = (x.borrow(), y.borrow());
            let same_proto = match (&x.proto, &y.proto) {
                (Some(p), Some(q)) => Gc::ptr_eq(p, q),
                (None, None) => true,
                _ => false,
            };
            same_proto && x.props.shape() == y.props.shape()
        }
        _ => false,
    }))
}

fn has_fast_properties(_i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    Ok(Value::Bool(match a.first() {
        Some(Value::Obj(o)) => o.borrow().props.shape_is_shared(),
        _ => true,
    }))
}

/// An object like `document.all` ([[IsHTMLDDA]]): `typeof` "undefined", falsy, loosely equal to
/// null and undefined.
fn get_undetectable(i: &mut Interp, _t: Value, _a: &[Value]) -> Result<Value, Value> {
    let ddda = i.make_native("Undetectable", 0, |_i, _t, _a| Ok(Value::Null));
    ddda.borrow().ic_plain.set(false);
    i.htmldda.push(ddda.clone());
    Ok(Value::Obj(ddda))
}

impl Interp {
    /// Turn V8's `--allow-natives-syntax` on or off (process-wide), giving this realm its
    /// runtime functions the first time it is turned on.
    pub fn set_natives_syntax(&mut self, on: bool) {
        ALLOWED.store(on, AtomicOrdering::Relaxed);
        if !on || self.global_env.borrow().vars.contains_key("%IsSmi") {
            return;
        }
        let fns: [(&str, usize, NativeFn); 6] = [
            ("IsSmi", 1, is_smi),
            ("CollectGarbage", 1, collect_garbage),
            ("DebugPrint", 1, debug_print),
            ("HaveSameMap", 2, have_same_map),
            ("GetUndetectable", 0, get_undetectable),
            ("HasFastProperties", 1, has_fast_properties),
        ];
        for (name, len, f) in fns {
            let func = self.make_native(name, len, f);
            self.global_env.borrow_mut().vars.insert(
                format!("%{name}"),
                Binding {
                    value: Value::Obj(func),
                    mutable: false,
                    strict_immutable: true,
                    initialized: true,
                    import: false,
                    deletable: false,
                },
            );
        }
    }
}
