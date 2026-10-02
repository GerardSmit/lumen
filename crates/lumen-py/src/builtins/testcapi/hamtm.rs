//! `_testcapi.hamt()`: the persistent immutable mapping `contextvars` is built on in CPython
//! (`Python/hamt.c`). Keys are found by hash, then by identity or equality; every update returns
//! a new mapping that shares the untouched buckets, and an update that changes nothing returns
//! the mapping itself.

use crate::bind::{opaque_instance, type_object, This};
use crate::builtins::native::with_opaque;
use crate::object::*;
use crate::vm::Interp;
use std::collections::BTreeMap;
use std::rc::Rc;

type Buckets = BTreeMap<i64, Rc<Vec<(Value, Value)>>>;

#[lumen_bind::class(name = "hamt", hint(py(final)))]
pub struct Hamt {
    buckets: Rc<Buckets>,
    len: usize,
}

#[lumen_bind::class(name = "hamt_iterator", hint(py(final)))]
pub struct HamtIter {
    items: Vec<Value>,
    pos: usize,
}

#[lumen_bind::methods]
impl HamtIter {
    #[proto(len)]
    fn len(&self) -> usize {
        self.items.len().saturating_sub(self.pos)
    }

    #[proto(iter)]
    fn iter(slf: This<Value>) -> Value {
        slf.0
    }

    #[proto(next)]
    fn next(&mut self) -> Option<Value> {
        let v = self.items.get(self.pos).cloned();
        self.pos += 1;
        v
    }
}

fn snapshot(it: &mut Interp, v: &Value) -> R<(Rc<Buckets>, usize)> {
    match with_opaque::<Hamt, _>(v, |h| (h.buckets.clone(), h.len)) {
        Some(s) => Ok(s),
        None => Err(it.type_error("descriptor requires a 'hamt' object")),
    }
}

fn make(it: &mut Interp, buckets: Rc<Buckets>, len: usize) -> Value {
    let ty = type_object::<Hamt>(it);
    opaque_instance(&ty, Hamt { buckets, len })
}

fn find(it: &mut Interp, bucket: &[(Value, Value)], key: &Value) -> R<Option<usize>> {
    for (i, (k, _)) in bucket.iter().enumerate() {
        if k.is(key) || it.values_eq(k, key)? {
            return Ok(Some(i));
        }
    }
    Ok(None)
}

fn lookup(it: &mut Interp, buckets: &Buckets, key: &Value) -> R<Option<Value>> {
    let h = it.hash_value(key)?;
    let Some(bucket) = buckets.get(&h) else { return Ok(None) };
    Ok(find(it, bucket, key)?.map(|i| bucket[i].1.clone()))
}

fn entries(buckets: &Buckets) -> Vec<(Value, Value)> {
    buckets.values().flat_map(|b| b.iter().cloned()).collect()
}

fn view(it: &mut Interp, items: Vec<Value>) -> Value {
    let ty = type_object::<HamtIter>(it);
    opaque_instance(&ty, HamtIter { items, pos: 0 })
}

#[lumen_bind::methods]
impl Hamt {
    /// Return a new mapping with `key` set to `val`.
    fn set(slf: This<&Value>, it: &mut Interp, key: &Value, val: &Value) -> R<Value> {
        let (buckets, len) = snapshot(it, &slf)?;
        let h = it.hash_value(key)?;
        let mut bucket: Vec<(Value, Value)> = buckets.get(&h).map(|b| b.to_vec()).unwrap_or_default();
        let grown = match find(it, &bucket, key)? {
            Some(i) => {
                if bucket[i].1.is(val) {
                    return Ok(slf.0.clone());
                }
                bucket[i].1 = val.clone();
                0
            }
            None => {
                bucket.push((key.clone(), val.clone()));
                1
            }
        };
        let mut next = (*buckets).clone();
        next.insert(h, Rc::new(bucket));
        Ok(make(it, Rc::new(next), len + grown))
    }

    /// Return a new mapping without `key`; `KeyError` when it is not there.
    fn delete(slf: This<&Value>, it: &mut Interp, key: &Value) -> R<Value> {
        let (buckets, len) = snapshot(it, &slf)?;
        let h = it.hash_value(key)?;
        let Some(bucket) = buckets.get(&h) else {
            return Err(it.new_exc_val("KeyError", key.clone()));
        };
        let Some(i) = find(it, bucket, key)? else {
            return Err(it.new_exc_val("KeyError", key.clone()));
        };
        let mut rest = bucket.to_vec();
        rest.remove(i);
        let mut next = (*buckets).clone();
        if rest.is_empty() {
            next.remove(&h);
        } else {
            next.insert(h, Rc::new(rest));
        }
        Ok(make(it, Rc::new(next), len - 1))
    }

    fn get(slf: This<&Value>, it: &mut Interp, key: &Value, default: Option<&Value>) -> R<Value> {
        let (buckets, _) = snapshot(it, &slf)?;
        Ok(lookup(it, &buckets, key)?.unwrap_or_else(|| default.cloned().unwrap_or(Value::None)))
    }

    fn keys(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        let (buckets, _) = snapshot(it, &slf)?;
        let items = entries(&buckets).into_iter().map(|(k, _)| k).collect();
        Ok(view(it, items))
    }

    fn values(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        let (buckets, _) = snapshot(it, &slf)?;
        let items = entries(&buckets).into_iter().map(|(_, v)| v).collect();
        Ok(view(it, items))
    }

    fn items(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        let (buckets, _) = snapshot(it, &slf)?;
        let items = entries(&buckets).into_iter().map(|(k, v)| Value::tuple(vec![k, v])).collect();
        Ok(view(it, items))
    }

    #[proto(len)]
    fn len(&self) -> usize {
        self.len
    }

    #[proto(contains)]
    fn contains(slf: This<&Value>, it: &mut Interp, key: &Value) -> R<bool> {
        let (buckets, _) = snapshot(it, &slf)?;
        Ok(lookup(it, &buckets, key)?.is_some())
    }

    #[proto(getitem)]
    fn getitem(slf: This<&Value>, it: &mut Interp, key: &Value) -> R<Value> {
        let (buckets, _) = snapshot(it, &slf)?;
        match lookup(it, &buckets, key)? {
            Some(v) => Ok(v),
            None => Err(it.new_exc_val("KeyError", key.clone())),
        }
    }

    #[proto(iter)]
    fn iter(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        let (buckets, _) = snapshot(it, &slf)?;
        let items = entries(&buckets).into_iter().map(|(k, _)| k).collect();
        Ok(view(it, items))
    }

    #[proto(eq)]
    fn eq(slf: This<&Value>, it: &mut Interp, other: &Value) -> R<Value> {
        Ok(compare(it, &slf, other)?.map_or(Value::NotImplemented, Value::Bool))
    }

    #[proto(ne)]
    fn ne(slf: This<&Value>, it: &mut Interp, other: &Value) -> R<Value> {
        Ok(compare(it, &slf, other)?.map_or(Value::NotImplemented, |b| Value::Bool(!b)))
    }
}

fn compare(it: &mut Interp, a: &Value, b: &Value) -> R<Option<bool>> {
    if with_opaque::<Hamt, _>(b, |_| ()).is_none() {
        return Ok(None);
    }
    let (left, left_len) = snapshot(it, a)?;
    let (right, right_len) = snapshot(it, b)?;
    if left_len != right_len {
        return Ok(Some(false));
    }
    for (k, v) in entries(&left) {
        match lookup(it, &right, &k)? {
            Some(w) if it.values_eq(&v, &w)? => {}
            _ => return Ok(Some(false)),
        }
    }
    Ok(Some(true))
}

#[lumen_bind::module(name = "_testcapi")]
pub mod hamtm {
    use super::*;

    /// hamt() -> an empty mapping
    #[op]
    fn hamt(it: &mut Interp) -> Value {
        make(it, Rc::new(Buckets::new()), 0)
    }
}
