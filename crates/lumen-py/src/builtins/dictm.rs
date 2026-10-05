//! `dict`, dict views, `set` and `frozenset`.

use super::slots::{reg_binops, reg_compare, reg_slots};
use crate::ast::{BinOp, CmpOp};
use crate::bind::{KwArgs, PyCx, PyHost, This};
use crate::containers::pydict_of;
use crate::dict::PyDict;
use crate::object::*;
use crate::vm::*;
use lumen_bind::{FromArg, Passed, Slot};
use std::cell::RefCell;
use std::rc::Rc;

/// A `dict` (or subclass instance): the receiver of the dict methods.
#[derive(Clone, Copy)]
pub struct DictRef<'a>(pub &'a Obj);

impl<'a> FromArg<'a, PyHost> for DictRef<'a> {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Dict(_)) => Ok(DictRef(o)),
            _ => Err(cx.arg_error(at, "dict", v)),
        }
    }
}

/// A dict view: the receiver of the view methods.
#[derive(Clone, Copy)]
pub struct ViewRef<'a>(pub &'a Obj, pub ViewKind);

impl<'a> FromArg<'a, PyHost> for ViewRef<'a> {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::DictView(d, vk) => Ok(ViewRef(d, *vk)),
                _ => Err(cx.arg_error(at, "dict view", v)),
            },
            _ => Err(cx.arg_error(at, "dict view", v)),
        }
    }
}

/// A `set` or `frozenset` (or subclass instance): the receiver of the non-mutating set methods.
#[derive(Clone, Copy)]
pub struct AnySetRef<'a>(pub &'a Obj);

impl<'a> FromArg<'a, PyHost> for AnySetRef<'a> {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Set(_) | Kind::FrozenSet(_)) => {
                Ok(AnySetRef(o))
            }
            _ => Err(cx.arg_error(at, "set", v)),
        }
    }
}

/// A `set` (or subclass instance): the receiver of the mutating set methods.
#[derive(Clone, Copy)]
pub struct SetRef<'a>(pub &'a Obj);

impl<'a> FromArg<'a, PyHost> for SetRef<'a> {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Set(_)) => Ok(SetRef(o)),
            _ => Err(cx.arg_error(at, "set", v)),
        }
    }
}

fn update_dict(it: &mut Interp, d: &Obj, name: &str, args: &[Value], kwargs: KwArgs) -> R<()> {
    if args.len() > 1 {
        return Err(it.type_error(&format!(
            "{} expected at most 1 argument, got {}",
            name,
            args.len()
        )));
    }
    if let Some(src) = args.first() {
        let into_empty = pydict_of(d).filter(|p| p.borrow().watch() != 0 && p.borrow().is_empty());
        let from_dict =
            matches!(src, Value::Obj(o) if o.cls.is_none() && matches!(o.kind, Kind::Dict(_)));
        match into_empty {
            Some(p) if from_dict => {
                let mask = p.borrow().watch();
                p.borrow().set_watch(0);
                let r = it.dict_update_from(d, src);
                p.borrow().set_watch(mask);
                r?;
                if !p.borrow().is_empty() {
                    p.borrow().notify_cloned();
                }
            }
            _ => it.dict_update_from(d, src)?,
        }
    }
    for (k, v) in kwargs.to_vec() {
        it.dict_set(d, Value::Obj(k), v)?;
    }
    Ok(())
}

/// dict() -> new empty dictionary
/// dict(mapping) -> new dictionary initialized from a mapping object's
///     (key, value) pairs
/// dict(iterable) -> new dictionary initialized as if via:
///     d = {}
///     for k, v in iterable:
///         d[k] = v
/// dict(**kwargs) -> new dictionary initialized with the name=value pairs
///     in the keyword argument list.  For example:  dict(one=1, two=2)
#[lumen_bind::class(name = "dict")]
pub struct Dict;

#[lumen_bind::methods]
impl Dict {
    #[constructor(hint(py(text_signature = "")))]
    fn new(
        cls: This<Value>,
        it: &mut Interp,
        #[varargs] args: &[Value],
        #[varkw] kwargs: KwArgs,
    ) -> R<Value> {
        let _ = (args, kwargs);
        let Value::Obj(cls) = &*cls else {
            unreachable!("checked by the entry")
        };
        it.alloc_instance(cls)
    }

    #[proto(init)]
    fn init(
        slf: This<DictRef<'_>>,
        it: &mut Interp,
        #[varargs] args: &[Value],
        #[varkw] kwargs: KwArgs,
    ) -> R<()> {
        update_dict(it, slf.0 .0, "dict", args, kwargs)
    }

    /// True if the dictionary has the specified key, else False.
    #[method(name = "__contains__")]
    fn contains(slf: This<DictRef<'_>>, it: &mut Interp, key: &Value) -> R<bool> {
        Ok(it.dict_get(slf.0 .0, key)?.is_some())
    }

    /// Return self[key].
    #[method(name = "__getitem__")]
    fn getitem(slf: This<&Value>, it: &mut Interp, key: &Value) -> R<Value> {
        it.native_getitem(&slf, key)
    }

    /// Return the value for key if key is in the dictionary, else default.
    #[method]
    fn get(
        slf: This<DictRef<'_>>,
        it: &mut Interp,
        key: &Value,
        #[default(Value::None)] default: Value,
    ) -> R<Value> {
        Ok(it.dict_get(slf.0 .0, key)?.unwrap_or(default))
    }

    /// Insert key with a value of default if key is not in the dictionary.
    ///
    /// Return the value for key if key is in the dictionary, else default.
    #[method]
    fn setdefault(
        slf: This<DictRef<'_>>,
        it: &mut Interp,
        key: &Value,
        #[default(Value::None)] default: Value,
    ) -> R<Value> {
        let d = slf.0 .0;
        if let Some(v) = it.dict_get(d, key)? {
            return Ok(v);
        }
        it.dict_set(d, key.clone(), default.clone())?;
        Ok(default)
    }

    /// D.pop(k[,d]) -> v, remove specified key and return the corresponding value.
    ///
    /// If the key is not found, return the default if given; otherwise,
    /// raise a KeyError.
    #[method(hint(py(text_signature = "($self, key, default=<unrepresentable>, /)")))]
    fn pop(
        slf: This<DictRef<'_>>,
        it: &mut Interp,
        key: &Value,
        default: Passed<Value>,
    ) -> R<Value> {
        match it.dict_remove(slf.0 .0, key)? {
            Some(v) => Ok(v),
            None => match default.0 {
                Some(d) => Ok(d),
                None => Err(it.new_exc_val("KeyError", key.clone())),
            },
        }
    }

    /// Remove and return a (key, value) pair as a 2-tuple.
    ///
    /// Pairs are returned in LIFO (last-in, first-out) order.
    /// Raises KeyError if the dict is empty.
    #[method]
    fn popitem(slf: This<DictRef<'_>>, it: &mut Interp) -> R<Value> {
        let Some(pd) = pydict_of(slf.0 .0) else {
            return Err(it.new_exc_str("KeyError", "popitem(): dictionary is empty"));
        };
        let last = pd.borrow().last_live();
        let e = last.and_then(|i| pd.borrow_mut().remove(i));
        match e {
            Some(e) => Ok(Value::tuple(vec![e.key, e.val])),
            None => Err(it.new_exc_str("KeyError", "popitem(): dictionary is empty")),
        }
    }

    /// D.keys() -> a set-like object providing a view on D's keys
    #[method(hint(py(text_signature = "")))]
    fn keys(slf: This<DictRef<'_>>) -> Value {
        Value::Obj(Object::new(Kind::DictView(
            slf.0 .0.clone(),
            ViewKind::Keys,
        )))
    }

    /// D.values() -> an object providing a view on D's values
    #[method(hint(py(text_signature = "")))]
    fn values(slf: This<DictRef<'_>>) -> Value {
        Value::Obj(Object::new(Kind::DictView(
            slf.0 .0.clone(),
            ViewKind::Values,
        )))
    }

    /// D.items() -> a set-like object providing a view on D's items
    #[method(hint(py(text_signature = "")))]
    fn items(slf: This<DictRef<'_>>) -> Value {
        Value::Obj(Object::new(Kind::DictView(
            slf.0 .0.clone(),
            ViewKind::Items,
        )))
    }

    /// D.update([E, ]**F) -> None.  Update D from mapping/iterable E and F.
    /// If E is present and has a .keys() method, then does:  for k in E.keys(): D[k] = E[k]
    /// If E is present and lacks a .keys() method, then does:  for k, v in E: D[k] = v
    /// In either case, this is followed by: for k in F:  D[k] = F[k]
    #[method(hint(py(text_signature = "")))]
    fn update(
        slf: This<DictRef<'_>>,
        it: &mut Interp,
        #[varargs] args: &[Value],
        #[varkw] kwargs: KwArgs,
    ) -> R<()> {
        update_dict(it, slf.0 .0, "update", args, kwargs)
    }

    /// D.clear() -> None.  Remove all items from D.
    #[method(hint(py(text_signature = "")))]
    fn clear(slf: This<DictRef<'_>>) {
        if let Some(p) = pydict_of(slf.0 .0) {
            p.borrow_mut().clear();
        }
    }

    /// D.copy() -> a shallow copy of D
    #[method(hint(py(text_signature = "")))]
    fn copy(slf: This<DictRef<'_>>) -> Value {
        Value::dict(
            pydict_of(slf.0 .0)
                .map(|p| p.borrow().clone())
                .unwrap_or_default(),
        )
    }

    /// Create a new dictionary with keys from iterable and values set to value.
    #[classmethod]
    fn fromkeys(
        cls: This<Value>,
        it: &mut Interp,
        iterable: &Value,
        #[default(Value::None)] value: Value,
    ) -> R<Value> {
        let d = it.call(&cls, Vec::new(), Vec::new())?;
        for k in it.iterate_to_vec(iterable)? {
            it.setitem(&d, k, value.clone())?;
        }
        Ok(d)
    }

    /// Return a reverse iterator over the dict keys.
    #[method(name = "__reversed__")]
    fn reversed(slf: This<DictRef<'_>>, it: &mut Interp) -> R<Value> {
        let ks = pydict_of(slf.0 .0)
            .map(|p| p.borrow().keys())
            .unwrap_or_default();
        let l = Value::list(ks.into_iter().rev().collect());
        it.get_iter(&l)
    }

    #[proto(repr)]
    fn repr(slf: This<DictRef<'_>>, it: &mut Interp) -> R<String> {
        it.native_repr(&Value::Obj(slf.0 .0.clone()))
    }

    #[proto(ior)]
    fn ior(slf: This<Value>, it: &mut Interp, value: &Value) -> R<Value> {
        let Value::Obj(d) = &slf.0 else {
            unreachable!("checked by the entry")
        };
        it.dict_update_from(d, value)?;
        Ok(slf.0)
    }
}

// `isdisjoint`, `__repr__` and `mapping` of the three dict views.
#[lumen_bind::class(name = "dict_view", hint(py(shared)))]
pub struct DictViews;

#[lumen_bind::methods]
impl DictViews {
    /// Return True if the view and the given iterable have a null intersection.
    #[method(hint(py(text_signature = "")))]
    fn isdisjoint(slf: This<&Value>, it: &mut Interp, other: &Value) -> R<bool> {
        for x in it.iterate_to_vec(other)? {
            if it.native_contains(&slf, &x)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[proto(repr)]
    fn repr(slf: This<&Value>, it: &mut Interp) -> R<String> {
        it.native_repr(&slf)
    }

    /// dictionary that this view refers to
    #[getter]
    fn mapping(slf: This<ViewRef<'_>>, it: &mut Interp) -> Value {
        it.new_mappingproxy(Value::Obj(slf.0 .0.clone()))
    }
}

fn view_reversed(it: &mut Interp, v: &Value) -> R<Value> {
    let mut items = it.iterate_to_vec(v)?;
    items.reverse();
    let l = Value::list(items);
    it.get_iter(&l)
}

#[lumen_bind::class(name = "dict_keys")]
pub struct DictKeys;

#[lumen_bind::methods]
impl DictKeys {
    /// Return a reverse iterator over the dict keys.
    #[method(name = "__reversed__", hint(py(text_signature = "")))]
    fn reversed(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        view_reversed(it, &slf)
    }
}

#[lumen_bind::class(name = "dict_values")]
pub struct DictValues;

#[lumen_bind::methods]
impl DictValues {
    /// Return a reverse iterator over the dict values.
    #[method(name = "__reversed__", hint(py(text_signature = "")))]
    fn reversed(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        view_reversed(it, &slf)
    }
}

#[lumen_bind::class(name = "dict_items")]
pub struct DictItems;

#[lumen_bind::methods]
impl DictItems {
    /// Return a reverse iterator over the dict items.
    #[method(name = "__reversed__", hint(py(text_signature = "")))]
    fn reversed(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        view_reversed(it, &slf)
    }
}

// ---- set / frozenset ------------------------------------------------------------------------------

fn set_hashable_key(it: &mut Interp, k: &Value) -> Value {
    if let Value::Obj(o) = k {
        if let Kind::Set(d) = &o.kind {
            let keys = d.borrow().keys();
            if let Ok(fs) = it.new_frozenset_from(keys) {
                return fs;
            }
        }
    }
    k.clone()
}

fn set_copy(s: &Obj) -> Value {
    let d = pydict_of(s).map(|p| p.borrow().clone()).unwrap_or_default();
    Value::Obj(Object::new(match &s.kind {
        Kind::FrozenSet(_) => Kind::FrozenSet(RefCell::new(d)),
        _ => Kind::Set(RefCell::new(d)),
    }))
}

fn as_set_value(it: &mut Interp, v: &Value) -> R<Value> {
    if is_any_set(v) {
        return Ok(v.clone());
    }
    let items = it.iterate_to_vec(v)?;
    it.new_set(items)
}

fn is_any_set(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Set(_) | Kind::FrozenSet(_)))
}

fn set_algebra(it: &mut Interp, s: &Obj, others: &[Value], op: BinOp) -> R<Value> {
    let mut cur = Value::Obj(s.clone());
    for other in others {
        let o = as_set_value(it, other)?;
        cur = it.set_binop(op, &cur, &o)?.unwrap_or(Value::None);
    }
    if let Value::Obj(c) = &cur {
        if Rc::ptr_eq(c, s) {
            return Ok(set_copy(s));
        }
    }
    Ok(cur)
}

fn set_update_with(it: &mut Interp, s: &Obj, others: &[Value], op: BinOp) -> R<()> {
    let res = set_algebra(it, s, others, op)?;
    if let (Value::Obj(r), Some(dst)) = (&res, pydict_of(s)) {
        if let Some(src) = pydict_of(r) {
            *dst.borrow_mut() = src.borrow().clone();
        }
    }
    Ok(())
}

fn set_inplace(it: &mut Interp, slf: Value, value: &Value, op: BinOp) -> R<Value> {
    if !is_any_set(value) {
        return Ok(Value::NotImplemented);
    }
    let Value::Obj(s) = &slf else {
        unreachable!("checked by the entry")
    };
    set_update_with(it, s, std::slice::from_ref(value), op)?;
    Ok(slf)
}

/// set() -> new empty set object
/// set(iterable) -> new set object
///
/// Build an unordered collection of unique elements.
#[lumen_bind::class(name = "set")]
pub struct Set;

#[lumen_bind::methods]
impl Set {
    #[constructor(hint(py(text_signature = "")))]
    fn new(
        cls: This<Value>,
        it: &mut Interp,
        #[varargs] args: &[Value],
        #[varkw] kwargs: KwArgs,
    ) -> R<Value> {
        let _ = (args, kwargs);
        let Value::Obj(cls) = &*cls else {
            unreachable!("checked by the entry")
        };
        it.alloc_instance(cls)
    }

    #[proto(init)]
    fn init(slf: This<SetRef<'_>>, it: &mut Interp, iterable: Passed<&Value>) -> R<()> {
        let s = slf.0 .0;
        if let Some(p) = pydict_of(s) {
            p.borrow_mut().clear();
        }
        if let Some(src) = iterable.0 {
            for x in it.iterate_to_vec(src)? {
                it.set_add_obj(s, x)?;
            }
        }
        Ok(())
    }

    /// x.__contains__(y) <==> y in x.
    #[method(name = "__contains__", hint(py(text_signature = "")))]
    fn contains(slf: This<&Value>, it: &mut Interp, key: &Value) -> R<bool> {
        it.native_contains(&slf, key)
    }

    /// Add an element to a set.
    ///
    /// This has no effect if the element is already present.
    #[method(hint(py(text_signature = "")))]
    fn add(slf: This<SetRef<'_>>, it: &mut Interp, elem: &Value) -> R<()> {
        it.set_add_obj(slf.0 .0, elem.clone())
    }

    /// Remove an element from a set; it must be a member.
    ///
    /// If the element is not a member, raise a KeyError.
    #[method(hint(py(text_signature = "")))]
    fn remove(slf: This<SetRef<'_>>, it: &mut Interp, elem: &Value) -> R<()> {
        let key = set_hashable_key(it, elem);
        match it.dict_remove(slf.0 .0, &key)? {
            Some(_) => Ok(()),
            None => Err(it.new_exc_val("KeyError", elem.clone())),
        }
    }

    /// Remove an element from a set if it is a member.
    ///
    /// Unlike set.remove(), the discard() method does not raise
    /// an exception when an element is missing from the set.
    #[method(hint(py(text_signature = "")))]
    fn discard(slf: This<SetRef<'_>>, it: &mut Interp, elem: &Value) -> R<()> {
        let key = set_hashable_key(it, elem);
        it.dict_remove(slf.0 .0, &key)?;
        Ok(())
    }

    /// Remove and return an arbitrary set element.
    /// Raises KeyError if the set is empty.
    #[method(hint(py(text_signature = "")))]
    fn pop(slf: This<SetRef<'_>>, it: &mut Interp) -> R<Value> {
        let Some(pd) = pydict_of(slf.0 .0) else {
            return Err(it.new_exc_str("KeyError", "pop from an empty set"));
        };
        let first = pd.borrow().next_live(0);
        match first.and_then(|i| pd.borrow_mut().remove(i)) {
            Some(e) => Ok(e.key),
            None => Err(it.new_exc_str("KeyError", "pop from an empty set")),
        }
    }

    /// Remove all elements from this set.
    #[method(hint(py(text_signature = "")))]
    fn clear(slf: This<SetRef<'_>>) {
        if let Some(p) = pydict_of(slf.0 .0) {
            p.borrow_mut().clear();
        }
    }

    /// Return a shallow copy of a set.
    #[method(hint(py(text_signature = "")))]
    fn copy(slf: This<AnySetRef<'_>>) -> Value {
        let s = slf.0 .0;
        if matches!(s.kind, Kind::FrozenSet(_)) && s.cls.is_none() {
            return Value::Obj(s.clone());
        }
        set_copy(s)
    }

    /// Return the union of sets as a new set.
    ///
    /// (i.e. all elements that are in either set.)
    #[method(hint(py(text_signature = "")))]
    fn union(slf: This<AnySetRef<'_>>, it: &mut Interp, #[varargs] others: &[Value]) -> R<Value> {
        set_algebra(it, slf.0 .0, others, BinOp::BitOr)
    }

    /// Return the intersection of two sets as a new set.
    ///
    /// (i.e. all elements that are in both sets.)
    #[method(hint(py(text_signature = "")))]
    fn intersection(
        slf: This<AnySetRef<'_>>,
        it: &mut Interp,
        #[varargs] others: &[Value],
    ) -> R<Value> {
        set_algebra(it, slf.0 .0, others, BinOp::BitAnd)
    }

    /// Return the difference of two or more sets as a new set.
    ///
    /// (i.e. all elements that are in this set but not the others.)
    #[method(hint(py(text_signature = "")))]
    fn difference(
        slf: This<AnySetRef<'_>>,
        it: &mut Interp,
        #[varargs] others: &[Value],
    ) -> R<Value> {
        set_algebra(it, slf.0 .0, others, BinOp::Sub)
    }

    /// Return the symmetric difference of two sets as a new set.
    ///
    /// (i.e. all elements that are in exactly one of the sets.)
    #[method(hint(py(text_signature = "")))]
    fn symmetric_difference(slf: This<AnySetRef<'_>>, it: &mut Interp, other: &Value) -> R<Value> {
        set_algebra(it, slf.0 .0, std::slice::from_ref(other), BinOp::BitXor)
    }

    /// Test whether every element in the set is in other.
    #[method]
    fn issubset(slf: This<&Value>, it: &mut Interp, other: &Value) -> R<bool> {
        let o = as_set_value(it, other)?;
        Ok(it.native_compare(CmpOp::LtE, &slf, &o)?.unwrap_or(false))
    }

    /// Test whether every element in other is in the set.
    #[method]
    fn issuperset(slf: This<&Value>, it: &mut Interp, other: &Value) -> R<bool> {
        let o = as_set_value(it, other)?;
        Ok(it.native_compare(CmpOp::GtE, &slf, &o)?.unwrap_or(false))
    }

    /// Return True if two sets have a null intersection.
    #[method(hint(py(text_signature = "")))]
    fn isdisjoint(slf: This<AnySetRef<'_>>, it: &mut Interp, other: &Value) -> R<bool> {
        for x in it.iterate_to_vec(other)? {
            if it.set_contains(slf.0 .0, &x)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Update a set with the union of itself and others.
    #[method(hint(py(text_signature = "")))]
    fn update(slf: This<SetRef<'_>>, it: &mut Interp, #[varargs] others: &[Value]) -> R<()> {
        set_update_with(it, slf.0 .0, others, BinOp::BitOr)
    }

    /// Update a set with the intersection of itself and another.
    #[method(hint(py(text_signature = "")))]
    fn intersection_update(
        slf: This<SetRef<'_>>,
        it: &mut Interp,
        #[varargs] others: &[Value],
    ) -> R<()> {
        set_update_with(it, slf.0 .0, others, BinOp::BitAnd)
    }

    /// Remove all elements of another set from this set.
    #[method(hint(py(text_signature = "")))]
    fn difference_update(
        slf: This<SetRef<'_>>,
        it: &mut Interp,
        #[varargs] others: &[Value],
    ) -> R<()> {
        set_update_with(it, slf.0 .0, others, BinOp::Sub)
    }

    /// Update a set with the symmetric difference of itself and another.
    #[method(hint(py(text_signature = "")))]
    fn symmetric_difference_update(slf: This<SetRef<'_>>, it: &mut Interp, other: &Value) -> R<()> {
        set_update_with(it, slf.0 .0, std::slice::from_ref(other), BinOp::BitXor)
    }

    #[proto(ior)]
    fn ior(slf: This<Value>, it: &mut Interp, value: &Value) -> R<Value> {
        set_inplace(it, slf.0, value, BinOp::BitOr)
    }

    #[proto(iand)]
    fn iand(slf: This<Value>, it: &mut Interp, value: &Value) -> R<Value> {
        set_inplace(it, slf.0, value, BinOp::BitAnd)
    }

    #[proto(isub)]
    fn isub(slf: This<Value>, it: &mut Interp, value: &Value) -> R<Value> {
        set_inplace(it, slf.0, value, BinOp::Sub)
    }

    #[proto(ixor)]
    fn ixor(slf: This<Value>, it: &mut Interp, value: &Value) -> R<Value> {
        set_inplace(it, slf.0, value, BinOp::BitXor)
    }

    /// Return state information for pickling.
    #[method(name = "__reduce__", hint(py(text_signature = "")))]
    fn reduce(slf: This<AnySetRef<'_>>, it: &mut Interp) -> R<Value> {
        let s = slf.0 .0;
        let v = Value::Obj(s.clone());
        let keys = pydict_of(s).map(|p| p.borrow().keys()).unwrap_or_default();
        let cls = Value::Obj(it.type_of(&v));
        let state = match it.get_attr_str(&v, "__dict__") {
            Ok(d) => d,
            Err(e) if it.exc_is(&e, "AttributeError") => Value::None,
            Err(e) => return Err(e),
        };
        Ok(Value::tuple(vec![
            cls,
            Value::tuple(vec![Value::list(keys)]),
            state,
        ]))
    }

    #[proto(repr)]
    fn repr(slf: This<&Value>, it: &mut Interp) -> R<String> {
        it.native_repr(&slf)
    }
}

/// frozenset() -> empty frozenset object
/// frozenset(iterable) -> frozenset object
///
/// Build an immutable unordered collection of unique elements.
#[lumen_bind::class(name = "frozenset")]
pub struct FrozenSet;

#[lumen_bind::methods]
impl FrozenSet {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, iterable: Passed<&Value>) -> R<Value> {
        let Value::Obj(cls) = &*cls else {
            unreachable!("checked by the entry")
        };
        let items = match iterable.0 {
            Some(src) => it.iterate_to_vec(src)?,
            None => Vec::new(),
        };
        let fs = Object::new(Kind::FrozenSet(RefCell::new(PyDict::new_set())));
        for x in items {
            it.set_add_obj(&fs, x)?;
        }
        if Rc::ptr_eq(cls, &it.types.frozenset) {
            return Ok(Value::Obj(fs));
        }
        let d = match &fs.kind {
            Kind::FrozenSet(d) => d.borrow().clone(),
            _ => PyDict::new(),
        };
        Ok(Value::Obj(Object::with_cls(
            cls.clone(),
            Kind::FrozenSet(RefCell::new(d)),
        )))
    }

    #[proto(hash)]
    fn hash(slf: This<&Value>, it: &mut Interp) -> R<i64> {
        it.native_hash(&slf)
    }
}

pub fn init(it: &mut Interp) {
    use crate::bind::{extend_type_documented, install_into};
    let (dict, set, frozenset) = (
        it.types.dict.clone(),
        it.types.set.clone(),
        it.types.frozenset.clone(),
    );
    extend_type_documented::<Dict>(it, &dict);
    if let Some(d) = dict.dict.borrow().as_ref() {
        dict_set_str(d, "__hash__", Value::None);
    }
    reg_slots(
        it,
        &dict,
        &["__setitem__", "__delitem__", "__len__", "__iter__"],
    );
    reg_binops(it, &dict, &["__or__", "__ror__"]);
    reg_compare(it, &dict, true);

    let views = [
        it.types.dict_keys.clone(),
        it.types.dict_values.clone(),
        it.types.dict_items.clone(),
    ];
    crate::bind::extend_type::<DictKeys>(it, &views[0]);
    crate::bind::extend_type::<DictValues>(it, &views[1]);
    crate::bind::extend_type::<DictItems>(it, &views[2]);
    for (i, vt) in views.iter().enumerate() {
        let set_like = i != 1;
        install_into::<DictViews>(
            vt,
            if set_like {
                &["isdisjoint", "__repr__"]
            } else {
                &["__repr__"]
            },
        );
        reg_slots(
            it,
            vt,
            if set_like {
                &["__len__", "__contains__", "__iter__"]
            } else {
                &["__len__", "__iter__"]
            },
        );
        if set_like {
            reg_binops(
                it,
                vt,
                &[
                    "__and__", "__rand__", "__or__", "__ror__", "__sub__", "__rsub__", "__xor__",
                    "__rxor__",
                ],
            );
            reg_compare(it, vt, true);
            if let Some(d) = vt.dict.borrow().as_ref() {
                dict_set_str(d, "__hash__", Value::None);
            }
        }
    }

    extend_type_documented::<Set>(it, &set);
    if let Some(d) = set.dict.borrow().as_ref() {
        dict_set_str(d, "__hash__", Value::None);
    }
    extend_type_documented::<FrozenSet>(it, &frozenset);
    install_into::<Set>(
        &frozenset,
        &[
            "__contains__",
            "copy",
            "union",
            "intersection",
            "difference",
            "symmetric_difference",
            "issubset",
            "issuperset",
            "isdisjoint",
            "__reduce__",
            "__repr__",
        ],
    );
    for t in [&set, &frozenset] {
        reg_slots(it, t, &["__len__", "__iter__"]);
        reg_binops(
            it,
            t,
            &[
                "__and__", "__rand__", "__or__", "__ror__", "__sub__", "__rsub__", "__xor__",
                "__rxor__",
            ],
        );
        reg_compare(it, t, true);
    }
}

/// The `mapping` attribute of the dict views as CPython's getset descriptor (its type exists once
/// `descr` is initialised).
pub fn init_descriptors(it: &mut Interp) {
    for vt in [
        it.types.dict_keys.clone(),
        it.types.dict_values.clone(),
        it.types.dict_items.clone(),
    ] {
        super::descr::install_getsets::<DictViews>(it, &vt, &[]);
    }
}
