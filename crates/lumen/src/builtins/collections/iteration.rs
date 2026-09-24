//! Live Map and Set iteration across mutation.

use crate::builtins::collection_data::CollectionKind;
use crate::builtins::{arg, coll_ptr, coll_ptr_kind, iter_result, map_ptr, set_internal};
use crate::interpreter::Interp;
use crate::value::{set_builtin, Gc, Object, Value};

/// Build a live iterator over a Map/Set. `kind`: 0 = values, 1 = keys, 2 = [key,value].
/// Like [`collection_iter`] but brand-checks the exact collection kind ("Set" / "Map").
pub(super) fn collection_iter_kind(
    i: &mut Interp,
    this: &Value,
    kind: u8,
    want: &str,
) -> Result<Value, Value> {
    coll_ptr_kind(i, this, Some(want))?;
    collection_iter(i, this, kind)
}

/// forEach shared by Map/Set, brand-checking the exact kind.
pub(super) fn collection_for_each(
    i: &mut Interp,
    this: Value,
    a: &[Value],
    want: Option<&str>,
) -> Result<Value, Value> {
    let ptr = coll_ptr_kind(i, &this, want)?;
    let cb = arg(a, 0);
    if !cb.is_callable() {
        return Err(i.make_error("TypeError", "forEach callback is not callable"));
    }
    let cb_this = arg(a, 1);
    // Iterate the LIVE backing list by index (positions are stable — deletes leave tombstones), so
    // entries appended during the callback are visited and deleted entries are skipped.
    let mut idx = 0usize;
    let mut f = crate::bytecode::PreparedCall::new(i, cb, cb_this);
    loop {
        let entry = i.map_data.get(&ptr).and_then(|e| e.next(&mut idx).cloned());
        let (k, v) = match entry {
            Some(kv) => kv,
            None => break,
        };
        f.call3(i, v, k, &this)?;
    }
    Ok(Value::Undefined)
}

fn collection_iter(i: &mut Interp, this: &Value, kind: u8) -> Result<Value, Value> {
    let ptr = coll_ptr(i, this)?;
    let is_set = i.map_data[&ptr].kind() == CollectionKind::Set;
    let key = if is_set {
        "%SetIteratorPrototype%"
    } else {
        "%MapIteratorPrototype%"
    };
    let proto = i
        .extra_protos
        .get(key)
        .cloned()
        .or_else(|| i.extra_protos.get("%IteratorPrototype%").cloned());
    let obj = Object::new(proto);
    set_builtin(&obj, "__ci_coll", this.clone());
    set_builtin(&obj, "__ci_index", Value::Num(0.0));
    set_builtin(&obj, "__ci_kind", Value::Num(kind as f64));
    Ok(Value::Obj(obj))
}

/// A Map/Set iterator over `coll` positioned at backing index `idx` (`done`: already
/// exhausted) — what `collection_iter` builds, for a protocol-free for-of state that must turn
/// into a real iterator object (see `bytecode::iter_fast`).
pub(crate) fn make_collection_iterator(
    i: &Interp,
    coll: Value,
    kind: u8,
    idx: f64,
    done: bool,
) -> Value {
    let is_set = coll
        .as_obj()
        .and_then(|o| i.map_data.get(&(Gc::as_ptr(o) as usize)))
        .is_some_and(|d| d.kind() == CollectionKind::Set);
    let key = if is_set {
        "%SetIteratorPrototype%"
    } else {
        "%MapIteratorPrototype%"
    };
    let proto = i
        .extra_protos
        .get(key)
        .cloned()
        .or_else(|| i.extra_protos.get("%IteratorPrototype%").cloned());
    let obj = Object::new(proto);
    set_builtin(&obj, "__ci_coll", coll);
    set_builtin(&obj, "__ci_index", Value::Num(idx));
    set_builtin(&obj, "__ci_kind", Value::Num(kind as f64));
    if done {
        set_internal(&obj, "__ci_done", Value::Bool(true));
    }
    Value::Obj(obj)
}

/// `next()` for a Map/Set iterator: reads the live backing entries at the current index (so entries
/// appended during iteration are observed). The `__ci_coll` slot is the brand.
pub(crate) fn map_set_iter_next(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let obj = match this.as_obj() {
        Some(o) if o.borrow().props.get("__ci_coll").is_some() => o.clone(),
        _ => return Err(i.make_error("TypeError", "not a Map/Set Iterator")),
    };
    Ok(match map_set_iter_step(i, &obj) {
        Some(val) => iter_result(i, val, false),
        None => iter_result(i, Value::Undefined, true),
    })
}

/// Every remaining value of a Map/Set iterator (`obj` carries the `__ci_coll` brand), leaving
/// it exhausted — the same end state as calling `next()` until `done`, for a caller that runs
/// no user code in between.
pub(crate) fn map_set_iter_drain(i: &mut Interp, obj: &Gc) -> Vec<Value> {
    let (coll, done, mut idx, kind) = {
        let b = obj.borrow();
        let num = |k: &str| -> f64 {
            match b.props.get(k).map(|p| p.value()) {
                Some(Value::Num(n)) => n,
                _ => 0.0,
            }
        };
        (
            b.props.get("__ci_coll").map(|p| p.value()),
            matches!(
                b.props.get("__ci_done").map(|p| p.value()),
                Some(Value::Bool(true))
            ),
            num("__ci_index") as usize,
            num("__ci_kind") as u8,
        )
    };
    if done {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut pairs: Vec<(Value, Value)> = Vec::new();
    if let Some(data) = coll.as_ref().and_then(map_ptr).and_then(|p| i.map_data.get(&p)) {
        while let Some((k, v)) = data.next(&mut idx) {
            match kind {
                1 => out.push(k.clone()),
                2 => pairs.push((k.clone(), v.clone())),
                _ => out.push(v.clone()),
            }
        }
    }
    for (k, v) in pairs {
        out.push(i.make_array(vec![k, v]));
    }
    set_internal(obj, "__ci_index", Value::Num(idx as f64));
    set_internal(obj, "__ci_done", Value::Bool(true));
    out
}

/// One `next()` of a Map/Set iterator (`obj` carries the `__ci_coll` brand) without the
/// iterator-result object: `Some(value)`, or `None` once exhausted. Advances the iterator's
/// state exactly as `next()` does, so a caller that would immediately unpack the fresh result
/// object may use this instead.
pub(crate) fn map_set_iter_step(i: &mut Interp, obj: &Gc) -> Option<Value> {
    let (coll, done, mut idx, kind) = {
        let b = obj.borrow();
        let num = |k: &str| -> f64 {
            match b.props.get(k).map(|p| p.value()) {
                Some(Value::Num(n)) => n,
                _ => 0.0,
            }
        };
        (
            b.props.get("__ci_coll").map(|p| p.value()),
            matches!(
                b.props.get("__ci_done").map(|p| p.value()),
                Some(Value::Bool(true))
            ),
            num("__ci_index") as usize,
            num("__ci_kind") as u8,
        )
    };
    // A once-exhausted iterator stays done, even if the collection later grows.
    if done {
        return None;
    }
    let coll_ptr = coll.as_ref().and_then(map_ptr);
    // Skip tombstoned (deleted) slots so the iterator observes a live view.
    let entry = coll_ptr
        .and_then(|p| i.map_data.get(&p))
        .and_then(|e| e.next(&mut idx).cloned());
    match entry {
        Some((k, v)) => {
            set_internal(obj, "__ci_index", Value::Num(idx as f64));
            Some(match kind {
                1 => k,
                2 => i.make_array(vec![k, v]),
                _ => v,
            })
        }
        None => {
            set_internal(obj, "__ci_index", Value::Num(idx as f64));
            set_internal(obj, "__ci_done", Value::Bool(true));
            None
        }
    }
}
