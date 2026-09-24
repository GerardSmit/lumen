//! Collection installation and constructors; storage and method families have separate owners.

use super::collection_data::{CollectionData, CollectionKind};
use super::{ab, new_from_ctor, set_to_string_tag, step_iter_with};
use crate::interpreter::Interp;
use crate::value::Gc;
use crate::value::{Callable, NativeFn, Object, Value};

pub(super) mod brand;
pub(crate) mod insert;
mod iteration;
pub(crate) mod lookup;
mod set_methods;
mod strong;
mod weak;
pub(crate) use iteration::map_set_iter_next;
pub(crate) use iteration::{map_set_iter_drain, map_set_iter_step};
use set_methods::install_set_methods;
use strong::{install_map_like, install_map_methods};
use weak::install_weak;

pub(super) fn install_collections(it: &mut Interp) {
    // %MapIteratorPrototype% / %SetIteratorPrototype%: distinct iterator prototypes (proto is
    // %IteratorPrototype%) with the right @@toStringTag and a live `next`.
    for (key, tag) in [
        ("%MapIteratorPrototype%", "Map Iterator"),
        ("%SetIteratorPrototype%", "Set Iterator"),
    ] {
        let proto = Object::new(it.extra_protos.get("%IteratorPrototype%").cloned());
        set_to_string_tag(it, &proto, tag);
        it.def_method(&proto, "next", 0, map_set_iter_next);
        it.extra_protos.insert(key, proto);
    }
    install_map_like(it, "Map", false, map_ctor);
    install_map_like(it, "Set", true, set_ctor);
    install_weak(it, "WeakMap", false, weakmap_ctor);
    install_weak(it, "WeakSet", true, weakset_ctor);
    install_set_methods(it);
    install_map_methods(it);
}

// Non-capturing constructor entry points (native fns must be bare `fn` pointers).
fn map_ctor(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    collection_ctor(i, a, CollectionKind::Map)
}
fn set_ctor(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    collection_ctor(i, a, CollectionKind::Set)
}
fn weakmap_ctor(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    collection_ctor(i, a, CollectionKind::WeakMap)
}
fn weakset_ctor(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    collection_ctor(i, a, CollectionKind::WeakSet)
}

fn collection_ctor(i: &mut Interp, args: &[Value], kind: CollectionKind) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Constructor requires 'new'"));
    }
    let name = kind.name();
    let is_set = matches!(kind, CollectionKind::Set | CollectionKind::WeakSet);
    let obj = new_from_ctor(i, name)?;
    let ptr = Gc::as_ptr(&obj) as usize;
    i.gc_pin(&obj);
    i.map_data.insert(ptr, CollectionData::new(kind));
    let mv = Value::Obj(obj);
    if let Some(src) = args.first() {
        if !matches!(src, Value::Undefined | Value::Null) {
            let add_fn = ab(i.get_member(&mv, if is_set { "add" } else { "set" }))?;
            if !add_fn.is_callable() {
                return Err(i.make_error("TypeError", "adder is not callable"));
            }
            // Step the source lazily: an error while processing an entry closes the iterator.
            let (iter, next) = ab(i.get_iterator(src))?;
            // Intrinsic Array Iterator `next` / intrinsic adder: step and add in place, without
            // iterator result objects or call frames (same operations, same order).
            let arr_next = match &next {
                Value::Obj(f) => match f.borrow().call {
                    Callable::Native(f) if super::array_iterator::is_next(f) => Some(f),
                    _ => None,
                },
                _ => None,
            };
            let adder: Option<NativeFn> = match &add_fn {
                Value::Obj(f) => match f.borrow().call {
                    Callable::Native(f)
                        if f as usize == insert::set_add as NativeFn as usize
                            || f as usize == insert::map_set as NativeFn as usize =>
                    {
                        Some(f)
                    }
                    _ => None,
                },
                _ => None,
            };
            loop {
                let item = match arr_next {
                    Some(f) => match super::array_iterator::step_with(i, f, &iter)? {
                        Some(v) => v,
                        None => break,
                    },
                    None => match step_iter_with(i, &iter, &next)? {
                        Some(v) => v,
                        None => break,
                    },
                };
                let step = if is_set {
                    match adder {
                        Some(f) => f(i, mv.clone(), std::slice::from_ref(&item))
                            .map_err(crate::interpreter::Abrupt::Throw),
                        None => i.call(add_fn.clone(), mv.clone(), &[item]),
                    }
                } else if !matches!(item, Value::Obj(_)) {
                    Err(crate::interpreter::Abrupt::Throw(i.make_error(
                        "TypeError",
                        "iterator value is not an entry object",
                    )))
                } else {
                    let entry = match &item {
                        Value::Obj(o) => super::array_fast::get_elem(i, o, &item, 0)
                            .and_then(|k| {
                                super::array_fast::get_elem(i, o, &item, 1).map(|v| (k, v))
                            })
                            .map_err(crate::interpreter::Abrupt::Throw),
                        _ => unreachable!("entry objects are checked above"),
                    };
                    entry.and_then(|(k, v)| match adder {
                        Some(f) => f(i, mv.clone(), &[k, v]).map_err(crate::interpreter::Abrupt::Throw),
                        None => i.call(add_fn.clone(), mv.clone(), &[k, v]),
                    })
                };
                if let Err(e) = step {
                    i.iterator_close(&iter);
                    return Err(crate::interpreter::abrupt_value(e));
                }
            }
        }
    }
    Ok(mv)
}

/// Count the live (non-tombstone) entries of a collection.
fn coll_live_len(i: &Interp, ptr: usize) -> usize {
    i.map_data.get(&ptr).map(CollectionData::len).unwrap_or(0)
}

/// CoerceKey for Map/Set: `-0` is canonicalized to `+0` so a stored key (and any key handed to a
/// callback or iterated) is `+0`, per spec.
fn canonicalize_map_key(k: Value) -> Value {
    match k {
        Value::Num(n) if n == 0.0 && n.is_sign_negative() => Value::Num(0.0),
        other => other,
    }
}
