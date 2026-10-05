//! `_testcapi` wrappers of the container APIs (`PyDict_*`, `PyList_*`, `PySet_*`, `PyTuple_*`,
//! `PyBytes_*`, `PyByteArray_*`). `None` stands for a `NULL` pointer. Where the C API reads an
//! item slot that was never filled (`PyList_New(n)`), the slot holds `None`.

use super::{bad_internal_call, builtin, nonnull, system_error};
use crate::containers::pydict_of;
use crate::object::*;
use crate::vm::Interp;

fn dict_obj(it: &mut Interp, v: &Value) -> R<Obj> {
    match v {
        Value::Obj(o) if matches!(o.kind, Kind::Dict(_)) => Ok(o.clone()),
        _ => Err(bad_internal_call(it)),
    }
}

fn set_obj(it: &mut Interp, v: &Value) -> R<Obj> {
    match v {
        Value::Obj(o) if matches!(o.kind, Kind::Set(_) | Kind::FrozenSet(_)) => Ok(o.clone()),
        _ => Err(bad_internal_call(it)),
    }
}

fn list_cell<'a>(it: &mut Interp, v: &'a Value) -> R<&'a std::cell::RefCell<Vec<Value>>> {
    match list_of(v) {
        Some(l) => Ok(l),
        None => Err(bad_internal_call(it)),
    }
}

fn tuple_items<'a>(it: &mut Interp, v: &'a Value) -> R<&'a [Value]> {
    match v.tuple_items() {
        Some(t) => Ok(t),
        None => Err(bad_internal_call(it)),
    }
}

fn exact(v: &Value, f: impl Fn(&Kind) -> bool) -> bool {
    matches!(v, Value::Obj(o) if o.cls.is_none() && f(&o.kind))
}

fn kind_is(v: &Value, f: impl Fn(&Kind) -> bool) -> bool {
    matches!(v, Value::Obj(o) if f(&o.kind))
}

fn key_error_class(it: &Interp) -> Value {
    Value::Obj(it.exc_type("KeyError"))
}

fn clamp(i: i64, len: usize) -> usize {
    i.clamp(0, len as i64) as usize
}

fn bytes_of_value(it: &mut Interp, v: &Value) -> R<Vec<u8>> {
    match v {
        Value::Obj(o) => match &o.kind {
            Kind::Bytes(b) => Ok(b.clone()),
            _ => {
                let t = it.tp_name_of(v);
                Err(it.type_error(&format!("expected bytes, {t} found")))
            }
        },
        _ => {
            let t = it.tp_name_of(v);
            Err(it.type_error(&format!("expected bytes, {t} found")))
        }
    }
}

/// The bytes of a `z#` argument: `None` is a NULL pointer (a size is then meaningless).
fn z_arg(v: &Value, it: &mut Interp) -> R<Option<Vec<u8>>> {
    if v.is_none() {
        return Ok(None);
    }
    Ok(Some(it.bytes_from_object(v)?))
}

#[lumen_bind::module(name = "_testcapi")]
pub mod containers {
    use super::*;

    #[op]
    fn dict_check(obj: &Value) -> i64 {
        i64::from(kind_is(obj, |k| matches!(k, Kind::Dict(_))))
    }

    #[op]
    fn dict_checkexact(obj: &Value) -> i64 {
        i64::from(exact(obj, |k| matches!(k, Kind::Dict(_))))
    }

    #[op]
    fn dict_new(it: &mut Interp) -> Value {
        Value::Obj(it.new_dict())
    }

    #[op]
    fn dictproxy_new(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let m = it.import_module("types")?;
        let f = it.get_attr_str(&Value::Obj(m), "MappingProxyType")?;
        it.call(&f, vec![obj.clone()], Vec::new())
    }

    #[op]
    fn dict_clear(it: &mut Interp, obj: &Value) -> R<()> {
        if let Ok(d) = dict_obj(it, obj) {
            if let Some(p) = pydict_of(&d) {
                p.borrow_mut().clear();
            }
        }
        Ok(())
    }

    #[op]
    fn dict_copy(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let d = dict_obj(it, obj)?;
        let copy = it.new_dict();
        it.dict_update_from(&copy, &Value::Obj(d))?;
        Ok(Value::Obj(copy))
    }

    #[op]
    fn dict_contains(it: &mut Interp, obj: &Value, key: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let key = nonnull(it, key)?;
        let d = dict_obj(it, obj)?;
        Ok(i64::from(it.dict_get(&d, key)?.is_some()))
    }

    #[op]
    fn dict_size(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let d = dict_obj(it, obj)?;
        Ok(pydict_of(&d).map_or(0, |p| p.borrow().len()) as i64)
    }

    #[op]
    fn dict_getitem(it: &mut Interp, mapping: &Value, key: &Value) -> Value {
        let found = match (dict_obj(it, mapping), key.is_none()) {
            (Ok(d), false) => it.dict_get(&d, key).ok().flatten(),
            _ => None,
        };
        found.unwrap_or_else(|| key_error_class(it))
    }

    #[op]
    fn dict_getitemstring(it: &mut Interp, mapping: &Value, key: &str) -> Value {
        let found = match dict_obj(it, mapping) {
            Ok(d) => it.dict_get(&d, &Value::str(key)).ok().flatten(),
            Err(_) => None,
        };
        found.unwrap_or_else(|| key_error_class(it))
    }

    #[op]
    fn dict_getitemwitherror(it: &mut Interp, mapping: &Value, key: &Value) -> R<Value> {
        let mapping = nonnull(it, mapping)?;
        let key = nonnull(it, key)?;
        let d = dict_obj(it, mapping)?;
        Ok(it.dict_get(&d, key)?.unwrap_or_else(|| key_error_class(it)))
    }

    #[op]
    fn dict_getitem_knownhash(it: &mut Interp, mapping: &Value, key: &Value, hash: i64) -> R<Value> {
        let d = dict_obj(it, mapping)?;
        let found = it.dict_find(&d, hash, key)?;
        let value = found.and_then(|i| pydict_of(&d).and_then(|p| p.borrow().get(i).map(|e| e.val.clone())));
        match value {
            Some(v) => Ok(v),
            None => Err(it.new_exc(&it.exc_type("KeyError"), vec![key.clone()])),
        }
    }

    #[op]
    fn dict_setitem(it: &mut Interp, mapping: &Value, key: &Value, value: &Value) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        let key = nonnull(it, key)?;
        let value = nonnull(it, value)?;
        let d = dict_obj(it, mapping)?;
        it.dict_set(&d, key.clone(), value.clone())?;
        Ok(0)
    }

    #[op]
    fn dict_setitemstring(it: &mut Interp, mapping: &Value, key: &str, value: &Value) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        let value = nonnull(it, value)?;
        let d = dict_obj(it, mapping)?;
        it.dict_set(&d, Value::str(key), value.clone())?;
        Ok(0)
    }

    #[op]
    fn dict_setdefault(it: &mut Interp, mapping: &Value, key: &Value, default: &Value) -> R<Value> {
        let mapping = nonnull(it, mapping)?;
        let key = nonnull(it, key)?;
        let default = nonnull(it, default)?;
        let d = dict_obj(it, mapping)?;
        if let Some(v) = it.dict_get(&d, key)? {
            return Ok(v);
        }
        it.dict_set(&d, key.clone(), default.clone())?;
        Ok(default.clone())
    }

    #[op]
    fn dict_delitem(it: &mut Interp, mapping: &Value, key: &Value) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        let key = nonnull(it, key)?;
        let d = dict_obj(it, mapping)?;
        if it.dict_remove(&d, key)?.is_none() {
            return Err(it.new_exc(&it.exc_type("KeyError"), vec![key.clone()]));
        }
        Ok(0)
    }

    #[op]
    fn dict_delitemstring(it: &mut Interp, mapping: &Value, key: &str) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        let d = dict_obj(it, mapping)?;
        let k = Value::str(key);
        if it.dict_remove(&d, &k)?.is_none() {
            return Err(it.new_exc(&it.exc_type("KeyError"), vec![k]));
        }
        Ok(0)
    }

    #[op]
    fn dict_keys(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let d = dict_obj(it, obj)?;
        Ok(Value::list(pydict_of(&d).map(|p| p.borrow().keys()).unwrap_or_default()))
    }

    #[op]
    fn dict_values(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let d = dict_obj(it, obj)?;
        Ok(Value::list(pydict_of(&d).map(|p| p.borrow().values()).unwrap_or_default()))
    }

    #[op]
    fn dict_items(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let d = dict_obj(it, obj)?;
        let items = pydict_of(&d).map(|p| p.borrow().iter().map(|e| Value::tuple(vec![e.key.clone(), e.val.clone()])).collect()).unwrap_or_default();
        Ok(Value::list(items))
    }

    #[op]
    fn dict_next(it: &mut Interp, mapping: &Value, pos: i64) -> R<Value> {
        let mapping = nonnull(it, mapping)?;
        let d = dict_obj(it, mapping)?;
        let Some(p) = pydict_of(&d) else { return Ok(Value::None) };
        let next = p.borrow().next_live(pos.max(0) as usize);
        match next {
            None => Ok(Value::None),
            Some(i) => {
                let (k, v) = p.borrow().get(i).map(|e| (e.key.clone(), e.val.clone())).unwrap_or((Value::None, Value::None));
                Ok(Value::tuple(vec![Value::Int(1), Value::Int(i as i64 + 1), k, v]))
            }
        }
    }

    #[op]
    fn dict_merge(it: &mut Interp, mapping: &Value, mapping2: &Value, override_: i64) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        let mapping2 = nonnull(it, mapping2)?;
        let d = dict_obj(it, mapping)?;
        if override_ == 1 {
            it.dict_update_from(&d, mapping2)?;
            return Ok(0);
        }
        let scratch = it.new_dict();
        it.dict_update_from(&scratch, mapping2)?;
        let items: Vec<(Value, Value)> = pydict_of(&scratch).map(|p| p.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect()).unwrap_or_default();
        for (k, v) in items {
            if it.dict_get(&d, &k)?.is_some() {
                if override_ == 2 {
                    return Err(it.new_exc(&it.exc_type("KeyError"), vec![k]));
                }
                continue;
            }
            it.dict_set(&d, k, v)?;
        }
        Ok(0)
    }

    #[op]
    fn dict_update(it: &mut Interp, mapping: &Value, mapping2: &Value) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        let mapping2 = nonnull(it, mapping2)?;
        let d = dict_obj(it, mapping)?;
        it.dict_update_from(&d, mapping2)?;
        Ok(0)
    }

    #[op]
    fn dict_mergefromseq2(it: &mut Interp, mapping: &Value, seq: &Value, override_: i64) -> R<i64> {
        let mapping = nonnull(it, mapping)?;
        let seq = nonnull(it, seq)?;
        let d = dict_obj(it, mapping)?;
        for (i, item) in it.iterate_to_vec(seq)?.into_iter().enumerate() {
            let pair = match it.iterate_to_vec(&item) {
                Ok(p) => p,
                Err(e) if it.exc_is(&e, "TypeError") => {
                    let t = it.tp_name_of(&item);
                    return Err(it.type_error(&format!("cannot convert dictionary update sequence element #{i} to a sequence ({t})")));
                }
                Err(e) => return Err(e),
            };
            if pair.len() != 2 {
                let n = pair.len();
                return Err(it.value_error(&format!("dictionary update sequence element #{i} has length {n}; 2 is required")));
            }
            if override_ != 0 || it.dict_get(&d, &pair[0])?.is_none() {
                it.dict_set(&d, pair[0].clone(), pair[1].clone())?;
            }
        }
        Ok(0)
    }

    #[op]
    fn dict_get_version(it: &mut Interp, dict: &Value) -> R<i64> {
        let d = dict_obj(it, dict)?;
        Ok(pydict_of(&d).map_or(0, |p| p.borrow().version()) as i64)
    }

    #[op]
    fn list_check(obj: &Value) -> i64 {
        i64::from(kind_is(obj, |k| matches!(k, Kind::List(_))))
    }

    #[op]
    fn list_check_exact(obj: &Value) -> i64 {
        i64::from(exact(obj, |k| matches!(k, Kind::List(_))))
    }

    #[op]
    fn list_new(it: &mut Interp, size: i64) -> R<Value> {
        if size < 0 {
            return Err(bad_internal_call(it));
        }
        Ok(Value::list(vec![Value::None; size as usize]))
    }

    #[op]
    fn list_size(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(list_cell(it, obj)?.borrow().len() as i64)
    }

    #[op]
    fn list_get_size(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(list_cell(it, obj)?.borrow().len() as i64)
    }

    #[op]
    fn list_getitem(it: &mut Interp, obj: &Value, i: i64) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let l = list_cell(it, obj)?;
        let found = usize::try_from(i).ok().and_then(|i| l.borrow().get(i).cloned());
        found.ok_or_else(|| it.new_exc_str("IndexError", "list index out of range"))
    }

    #[op]
    fn list_get_item(it: &mut Interp, obj: &Value, i: i64) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let l = list_cell(it, obj)?;
        let found = usize::try_from(i).ok().and_then(|i| l.borrow().get(i).cloned());
        found.ok_or_else(|| it.new_exc_str("IndexError", "list index out of range"))
    }

    #[op]
    fn list_setitem(it: &mut Interp, obj: &Value, i: i64, value: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let l = list_cell(it, obj)?;
        let len = l.borrow().len();
        if i < 0 || i as usize >= len {
            return Err(it.new_exc_str("IndexError", "list assignment index out of range"));
        }
        l.borrow_mut()[i as usize] = value.clone();
        Ok(0)
    }

    #[op]
    fn list_set_item(it: &mut Interp, obj: &Value, i: i64, value: &Value) -> R<()> {
        let obj = nonnull(it, obj)?;
        let l = list_cell(it, obj)?;
        let len = l.borrow().len();
        if i < 0 || i as usize >= len {
            return Err(it.new_exc_str("IndexError", "list assignment index out of range"));
        }
        l.borrow_mut()[i as usize] = value.clone();
        Ok(())
    }

    #[op]
    fn list_insert(it: &mut Interp, obj: &Value, where_: i64, value: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let value = nonnull(it, value)?;
        let l = list_cell(it, obj)?;
        let len = l.borrow().len();
        let at = if where_ < 0 { clamp(where_ + len as i64, len) } else { clamp(where_, len) };
        l.borrow_mut().insert(at, value.clone());
        Ok(0)
    }

    #[op]
    fn list_append(it: &mut Interp, obj: &Value, value: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let value = nonnull(it, value)?;
        list_cell(it, obj)?.borrow_mut().push(value.clone());
        Ok(0)
    }

    #[op]
    fn list_getslice(it: &mut Interp, obj: &Value, low: i64, high: i64) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let l = list_cell(it, obj)?.borrow().clone();
        let lo = clamp(low, l.len());
        let hi = clamp(high, l.len()).max(lo);
        Ok(Value::list(l[lo..hi].to_vec()))
    }

    #[op]
    fn list_setslice(it: &mut Interp, obj: &Value, low: i64, high: i64, value: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let replacement = if value.is_none() { Vec::new() } else { it.iterate_to_vec(value)? };
        let l = list_cell(it, obj)?;
        let len = l.borrow().len();
        let lo = clamp(low, len);
        let hi = clamp(high, len).max(lo);
        l.borrow_mut().splice(lo..hi, replacement);
        Ok(0)
    }

    #[op]
    fn list_sort(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        list_cell(it, obj)?;
        it.call_method(obj, "sort", Vec::new())?;
        Ok(0)
    }

    #[op]
    fn list_reverse(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        list_cell(it, obj)?.borrow_mut().reverse();
        Ok(0)
    }

    #[op]
    fn list_astuple(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let items = list_cell(it, obj)?.borrow().clone();
        Ok(Value::tuple(items))
    }

    #[op]
    fn set_check(obj: &Value) -> i64 {
        i64::from(kind_is(obj, |k| matches!(k, Kind::Set(_))))
    }

    #[op]
    fn set_checkexact(obj: &Value) -> i64 {
        i64::from(exact(obj, |k| matches!(k, Kind::Set(_))))
    }

    #[op]
    fn frozenset_check(obj: &Value) -> i64 {
        i64::from(kind_is(obj, |k| matches!(k, Kind::FrozenSet(_))))
    }

    #[op]
    fn frozenset_checkexact(obj: &Value) -> i64 {
        i64::from(exact(obj, |k| matches!(k, Kind::FrozenSet(_))))
    }

    #[op]
    fn anyset_check(obj: &Value) -> i64 {
        i64::from(kind_is(obj, |k| matches!(k, Kind::Set(_) | Kind::FrozenSet(_))))
    }

    #[op]
    fn anyset_checkexact(obj: &Value) -> i64 {
        i64::from(exact(obj, |k| matches!(k, Kind::Set(_) | Kind::FrozenSet(_))))
    }

    #[op]
    fn set_new(it: &mut Interp, iterable: Option<&Value>) -> R<Value> {
        let items = match iterable.filter(|v| !v.is_none()) {
            Some(v) => it.iterate_to_vec(v)?,
            None => Vec::new(),
        };
        it.new_set(items)
    }

    #[op]
    fn frozenset_new(it: &mut Interp, iterable: Option<&Value>) -> R<Value> {
        let items = match iterable.filter(|v| !v.is_none()) {
            Some(v) => it.iterate_to_vec(v)?,
            None => Vec::new(),
        };
        it.new_frozenset_from(items)
    }

    #[op]
    fn set_size(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let s = set_obj(it, obj)?;
        Ok(pydict_of(&s).map_or(0, |p| p.borrow().len()) as i64)
    }

    #[op]
    fn set_get_size(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let s = set_obj(it, obj)?;
        Ok(pydict_of(&s).map_or(0, |p| p.borrow().len()) as i64)
    }

    #[op]
    fn set_contains(it: &mut Interp, obj: &Value, item: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let item = nonnull(it, item)?;
        let s = set_obj(it, obj)?;
        Ok(i64::from(it.set_contains(&s, item)?))
    }

    #[op]
    fn set_add(it: &mut Interp, obj: &Value, item: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let item = nonnull(it, item)?;
        let s = match obj {
            Value::Obj(o) if matches!(o.kind, Kind::Set(_)) => o.clone(),
            _ => return Err(bad_internal_call(it)),
        };
        it.set_add_obj(&s, item.clone())?;
        Ok(0)
    }

    #[op]
    fn set_discard(it: &mut Interp, obj: &Value, item: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let item = nonnull(it, item)?;
        let s = match obj {
            Value::Obj(o) if matches!(o.kind, Kind::Set(_)) => o.clone(),
            _ => return Err(bad_internal_call(it)),
        };
        Ok(i64::from(it.dict_remove(&s, item)?.is_some()))
    }

    #[op]
    fn set_pop(it: &mut Interp, obj: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        if !kind_is(obj, |k| matches!(k, Kind::Set(_))) {
            return Err(bad_internal_call(it));
        }
        it.call_method(obj, "pop", Vec::new())
    }

    #[op]
    fn set_clear(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        if !kind_is(obj, |k| matches!(k, Kind::Set(_))) {
            return Err(bad_internal_call(it));
        }
        it.call_method(obj, "clear", Vec::new())?;
        Ok(0)
    }

    #[op]
    fn tuple_check(obj: &Value) -> i64 {
        i64::from(kind_is(obj, |k| matches!(k, Kind::Tuple(_))))
    }

    #[op]
    fn tuple_checkexact(obj: &Value) -> i64 {
        i64::from(exact(obj, |k| matches!(k, Kind::Tuple(_))))
    }

    #[op]
    fn tuple_new(it: &mut Interp, size: i64) -> R<Value> {
        if size < 0 {
            return Err(bad_internal_call(it));
        }
        Ok(Value::tuple(vec![Value::None; size as usize]))
    }

    #[op]
    fn tuple_pack(it: &mut Interp, size: i64, arg1: Option<&Value>, arg2: Option<&Value>) -> R<Value> {
        let given: Vec<Value> = [arg1, arg2].into_iter().flatten().cloned().collect();
        if size < 0 || size as usize != given.len() {
            return Err(bad_internal_call(it));
        }
        Ok(Value::tuple(given))
    }

    #[op]
    fn tuple_size(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(tuple_items(it, obj)?.len() as i64)
    }

    #[op]
    fn tuple_get_size(it: &mut Interp, obj: &Value) -> R<i64> {
        let obj = nonnull(it, obj)?;
        Ok(tuple_items(it, obj)?.len() as i64)
    }

    #[op]
    fn tuple_getitem(it: &mut Interp, obj: &Value, i: i64) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let t = tuple_items(it, obj)?;
        let found = usize::try_from(i).ok().and_then(|i| t.get(i).cloned());
        found.ok_or_else(|| it.new_exc_str("IndexError", "tuple index out of range"))
    }

    #[op]
    fn tuple_get_item(it: &mut Interp, obj: &Value, i: i64) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let t = tuple_items(it, obj)?;
        let found = usize::try_from(i).ok().and_then(|i| t.get(i).cloned());
        found.ok_or_else(|| it.new_exc_str("IndexError", "tuple index out of range"))
    }

    #[op]
    fn tuple_getslice(it: &mut Interp, obj: &Value, low: i64, high: i64) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let t = tuple_items(it, obj)?;
        let lo = clamp(low, t.len());
        let hi = clamp(high, t.len()).max(lo);
        Ok(Value::tuple(t[lo..hi].to_vec()))
    }

    #[op]
    fn tuple_setitem(it: &mut Interp, obj: &Value, i: i64, value: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let t = tuple_items(it, obj)?;
        if i < 0 || i as usize >= t.len() {
            return Err(it.new_exc_str("IndexError", "tuple assignment index out of range"));
        }
        let mut items = t.to_vec();
        items[i as usize] = value.clone();
        Ok(Value::tuple(items))
    }

    #[op]
    fn tuple_set_item(it: &mut Interp, obj: &Value, i: i64, value: &Value) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let t = tuple_items(it, obj)?;
        let mut items = t.to_vec();
        let Some(slot) = usize::try_from(i).ok().and_then(|i| items.get_mut(i)) else { return Err(it.new_exc_str("IndexError", "tuple assignment index out of range")) };
        *slot = value.clone();
        Ok(Value::tuple(items))
    }

    #[op]
    fn _tuple_resize(it: &mut Interp, tup: &Value, newsize: i64, new_: Option<bool>) -> R<Value> {
        let _ = new_;
        let tup = nonnull(it, tup)?;
        let t = tuple_items(it, tup)?;
        if newsize < 0 {
            return Err(bad_internal_call(it));
        }
        let mut items = t.to_vec();
        items.resize(newsize as usize, Value::None);
        Ok(Value::tuple(items))
    }

    #[op]
    fn _check_tuple_item_is_NULL(it: &mut Interp, obj: &Value, i: i64) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let t = tuple_items(it, obj)?;
        if i < 0 || i as usize >= t.len() {
            return Err(it.new_exc_str("IndexError", "tuple index out of range"));
        }
        Ok(0)
    }

    #[op]
    fn bytes_check(obj: &Value) -> i64 {
        i64::from(kind_is(obj, |k| matches!(k, Kind::Bytes(_))))
    }

    #[op]
    fn bytes_checkexact(obj: &Value) -> i64 {
        i64::from(exact(obj, |k| matches!(k, Kind::Bytes(_))))
    }

    #[op]
    fn bytes_fromstringandsize(it: &mut Interp, s: &Value, size: Option<i64>) -> R<Value> {
        let data = z_arg(s, it)?;
        let size = size.unwrap_or_else(|| data.as_ref().map_or(0, |d| d.len() as i64));
        if size < 0 {
            return Err(system_error(it, "Negative size passed to PyBytes_FromStringAndSize"));
        }
        match data {
            Some(mut d) => {
                d.resize(size as usize, 0);
                Ok(Value::bytes(d))
            }
            None => Ok(Value::bytes(vec![0; size as usize])),
        }
    }

    #[op]
    fn bytes_fromstring(it: &mut Interp, s: &Value) -> R<Value> {
        match z_arg(s, it)? {
            Some(d) => Ok(Value::bytes(d.into_iter().take_while(|b| *b != 0).collect())),
            None => Err(system_error(it, "null argument to internal routine")),
        }
    }

    #[op]
    fn bytes_fromobject(it: &mut Interp, arg: &Value) -> R<Value> {
        let arg = nonnull(it, arg)?;
        Ok(Value::bytes(it.bytes_from_object(arg)?))
    }

    #[op]
    fn bytes_size(it: &mut Interp, arg: &Value) -> R<i64> {
        let arg = nonnull(it, arg)?;
        Ok(bytes_of_value(it, arg)?.len() as i64)
    }

    #[op]
    fn bytes_asstring(it: &mut Interp, obj: &Value, buflen: i64) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let mut d = bytes_of_value(it, obj)?;
        d.push(0);
        d.resize(buflen.max(0) as usize, 0);
        Ok(Value::bytes(d))
    }

    #[op]
    fn bytes_asstringandsize(it: &mut Interp, obj: &Value, buflen: i64) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let mut d = bytes_of_value(it, obj)?;
        let size = d.len() as i64;
        if d.contains(&0) {
            return Err(it.value_error("embedded null byte"));
        }
        d.resize(buflen.max(0) as usize, 0);
        Ok(Value::tuple(vec![Value::bytes(d), Value::Int(size)]))
    }

    #[op]
    fn bytes_asstringandsize_null(it: &mut Interp, obj: &Value, buflen: i64) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let mut d = bytes_of_value(it, obj)?;
        if d.contains(&0) {
            return Err(it.value_error("embedded null byte"));
        }
        d.resize(buflen.max(0) as usize, 0);
        Ok(Value::bytes(d))
    }

    #[op]
    fn bytes_repr(it: &mut Interp, obj: &Value, smartquotes: i64) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let d = bytes_of_value(it, obj)?;
        let text = it.repr_of(&Value::bytes(d.clone()))?;
        if smartquotes == 0 && text.starts_with("b\"") && text.ends_with('"') {
            let inner = &text[2..text.len() - 1];
            return Ok(Value::string(format!("b'{}'", inner.replace('\'', "\\'"))));
        }
        Ok(Value::string(text))
    }

    #[op]
    fn bytes_concat(it: &mut Interp, left: &Value, right: &Value, new_: Option<bool>) -> R<Value> {
        let _ = new_;
        if left.is_none() {
            return Ok(Value::None);
        }
        let right = nonnull(it, right)?;
        let l = bytes_of_value(it, left)?;
        let mut r = it.bytes_from_object(right).map_err(|_| {
            let t = it.tp_name_of(right);
            it.type_error(&format!("can't concat {t} to bytes"))
        })?;
        let mut out = l;
        out.append(&mut r);
        Ok(Value::bytes(out))
    }

    #[op]
    fn bytes_concatanddel(it: &mut Interp, left: &Value, right: &Value, new_: Option<bool>) -> R<Value> {
        if right.is_none() {
            return Ok(Value::None);
        }
        bytes_concat(it, left, right, new_)
    }

    #[op]
    fn bytes_decodeescape(it: &mut Interp, s: &Value, errors: Option<&Value>) -> R<Value> {
        let data = z_arg(s, it)?.unwrap_or_default();
        let codecs = it.import_module("codecs")?;
        let f = it.get_attr_str(&Value::Obj(codecs), "escape_decode")?;
        let mut args = vec![Value::bytes(data)];
        if let Some(e) = errors.filter(|e| !e.is_none()) {
            args.push(e.clone());
        }
        let r = it.call(&f, args, Vec::new())?;
        Ok(r.tuple_items().and_then(|t| t.first().cloned()).unwrap_or(Value::None))
    }

    #[op]
    fn bytearray_check(obj: &Value) -> i64 {
        i64::from(kind_is(obj, |k| matches!(k, Kind::ByteArray(_))))
    }

    #[op]
    fn bytearray_checkexact(obj: &Value) -> i64 {
        i64::from(exact(obj, |k| matches!(k, Kind::ByteArray(_))))
    }

    #[op]
    fn bytearray_fromstringandsize(it: &mut Interp, s: &Value, size: Option<i64>) -> R<Value> {
        let data = z_arg(s, it)?;
        let size = size.unwrap_or_else(|| data.as_ref().map_or(0, |d| d.len() as i64));
        if size < 0 {
            return Err(system_error(it, "Negative size passed to PyByteArray_FromStringAndSize"));
        }
        let mut d = data.unwrap_or_default();
        d.resize(size as usize, 0);
        Ok(Value::bytearray(d))
    }

    #[op]
    fn bytearray_fromobject(it: &mut Interp, arg: &Value) -> R<Value> {
        let arg = nonnull(it, arg)?;
        let f = builtin(it, "bytearray");
        it.call(&f, vec![arg.clone()], Vec::new())
    }

    fn byte_array<'a>(it: &mut Interp, v: &'a Value) -> R<&'a Value> {
        if kind_is(v, |k| matches!(k, Kind::ByteArray(_))) {
            return Ok(v);
        }
        let t = it.tp_name_of(v);
        Err(it.type_error(&format!("expected bytearray, {t} found")))
    }

    #[op]
    fn bytearray_size(it: &mut Interp, arg: &Value) -> R<i64> {
        let arg = nonnull(it, arg)?;
        let ba = byte_array(it, arg)?;
        Ok(it.len_of(ba)? as i64)
    }

    #[op]
    fn bytearray_asstring(it: &mut Interp, obj: &Value, buflen: i64) -> R<Value> {
        let obj = nonnull(it, obj)?;
        let ba = byte_array(it, obj)?;
        let mut d = it.bytes_from_object(ba)?;
        d.push(0);
        d.resize(buflen.max(0) as usize, 0);
        Ok(Value::bytearray(d))
    }

    #[op]
    fn bytearray_concat(it: &mut Interp, left: &Value, right: &Value) -> R<Value> {
        let left = nonnull(it, left)?;
        let right = nonnull(it, right)?;
        if !it.is_buffer(left) || !it.is_buffer(right) {
            let bad = if it.is_buffer(left) { right } else { left };
            let t = it.tp_name_of(bad);
            return Err(it.type_error(&format!("can't concat {t} to bytearray")));
        }
        let mut l = it.bytes_from_object(left)?;
        l.extend(it.bytes_from_object(right)?);
        Ok(Value::bytearray(l))
    }

    #[op]
    fn bytearray_resize(it: &mut Interp, obj: &Value, size: i64) -> R<i64> {
        let obj = nonnull(it, obj)?;
        let ba = byte_array(it, obj)?;
        if size < 0 {
            return Err(it.value_error("Can only resize to positive sizes"));
        }
        let len = it.len_of(ba)? as i64;
        if size < len {
            let slice = Value::Obj(Object::new(Kind::Slice(Value::Int(size), Value::None, Value::None)));
            it.delitem(ba, &slice)?;
        } else if size > len {
            let pad = Value::bytes(vec![0; (size - len) as usize]);
            it.call_method(ba, "extend", vec![pad])?;
        }
        Ok(0)
    }
}
