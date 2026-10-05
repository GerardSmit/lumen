//! `range`, `slice`, `enumerate`, `zip`, `map`, `filter`, `reversed` and the iterator types.

use super::slots::reg_compare;
use super::slots::{reg_iterator, reg_slots};
use crate::bind::{Inst, PyCx, PyHost, This};
use crate::object::*;
use crate::ops::slice_len;
use crate::vm::*;
use lumen_bind::{FromArg, Passed, Slot};
use std::cell::RefCell;

/// A builtin iterator's state (the receiver of the iterator types' members).
#[derive(Clone, Copy)]
pub struct IterRef<'a>(pub &'a Obj, pub &'a RefCell<IterState>);

impl<'a> FromArg<'a, PyHost> for IterRef<'a> {
    #[inline]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Iter(st) => Ok(IterRef(o, st)),
                _ => Err(cx.arg_error(at, "iterator", v)),
            },
            _ => Err(cx.arg_error(at, "iterator", v)),
        }
    }
}

/// A new iterator of type `cls`: the builtin type `base` itself, or a subclass of it.
fn new_iter(it: &Interp, cls: &Value, base: &Obj, st: IterState) -> Value {
    match cls {
        Value::Obj(c) if !std::rc::Rc::ptr_eq(c, base) => {
            Value::Obj(Object::with_cls(c.clone(), Kind::Iter(RefCell::new(st))))
        }
        _ => it.mk_iter(st),
    }
}

/// range(stop) -> range object
/// range(start, stop[, step]) -> range object
///
/// Return an object that produces a sequence of integers from start (inclusive)
/// to stop (exclusive) by step.  range(i, j) produces i, i+1, i+2, ..., j-1.
/// start defaults to 0, and stop is omitted!  range(4) produces 0, 1, 2, 3.
/// These are exactly the valid indices for a list of 4 elements.
/// When step is given, it specifies the increment (or decrement).
#[lumen_bind::class(name = "range")]
pub struct Range;

#[lumen_bind::methods]
impl Range {
    #[constructor(hint(py(text_signature = "")))]
    fn new(it: &mut Interp, a: &Value, b: Passed<&Value>, c: Passed<&Value>) -> R<Value> {
        let args: Vec<&Value> = [Some(a), b.0, c.0].into_iter().flatten().collect();
        let mut ints = Vec::with_capacity(3);
        let mut bigs: Vec<crate::pyint::BigInt> = Vec::with_capacity(3);
        let mut overflow = false;
        for v in args {
            if !it.has_index(v) {
                let t = it.type_name_of(v);
                return Err(it.type_error(&format!(
                    "'{}' object cannot be interpreted as an integer",
                    t
                )));
            }
            match v.as_bigint() {
                Some(b) => {
                    match b.to_i64() {
                        Some(i) => ints.push(i),
                        None => {
                            overflow = true;
                            ints.push(0);
                        }
                    }
                    bigs.push(b);
                }
                None => {
                    let i = it.index_of(v)?;
                    ints.push(i);
                    bigs.push(crate::pyint::BigInt::from_i64(i));
                }
            }
        }
        if overflow {
            let one = crate::pyint::BigInt::from_i64(1);
            let mut it3 = bigs.into_iter();
            let (start, stop, step) = match (it3.next(), it3.next(), it3.next()) {
                (Some(x), None, None) => (crate::pyint::BigInt::zero(), x, one),
                (Some(x), Some(y), None) => (x, y, one),
                (Some(x), Some(y), Some(z)) => (x, y, z),
                _ => unreachable!("one to three arguments"),
            };
            if step.is_zero() {
                return Err(it.value_error("range() arg 3 must not be zero"));
            }
            return Ok(Value::Obj(Object::new(Kind::BigRange(Box::new([
                start, stop, step,
            ])))));
        }
        let (start, stop, step) = match ints.len() {
            1 => (0, ints[0], 1),
            2 => (ints[0], ints[1], 1),
            _ => (ints[0], ints[1], ints[2]),
        };
        if step == 0 {
            return Err(it.value_error("range() arg 3 must not be zero"));
        }
        Ok(Value::Obj(Object::new(Kind::Range(RangeData {
            start,
            stop,
            step,
        }))))
    }

    /// rangeobject.index(value) -> integer -- return index of value.
    /// Raise ValueError if the value is not present.
    #[method(hint(py(text_signature = "")))]
    fn index(slf: This<Inst<'_, Range>>, it: &mut Interp, value: &Value) -> R<i64> {
        if let (Kind::Range(r), Value::Int(i)) = (&slf.0 .0.kind, value) {
            let len = slice_len(r.start, r.stop, r.step) as i64;
            let d = i - r.start;
            if d % r.step == 0 {
                let k = d / r.step;
                if k >= 0 && k < len {
                    return Ok(k);
                }
            }
        }
        let s = it.repr_of(value)?;
        Err(it.value_error(&format!("{} is not in range", s)))
    }

    /// rangeobject.count(value) -> integer -- return number of occurrences of value
    #[method(hint(py(text_signature = "")))]
    fn count(slf: This<Inst<'_, Range>>, it: &mut Interp, value: &Value) -> R<i64> {
        let r = Value::Obj(slf.0 .0.clone());
        Ok(if it.native_contains(&r, value)? { 1 } else { 0 })
    }

    #[proto(hash)]
    fn hash(slf: This<Inst<'_, Range>>, it: &mut Interp) -> R<i64> {
        it.native_hash(&Value::Obj(slf.0 .0.clone()))
    }

    #[proto(repr)]
    fn repr(slf: This<Inst<'_, Range>>, it: &mut Interp) -> R<String> {
        it.native_repr(&Value::Obj(slf.0 .0.clone()))
    }

    // CPython's range_reduce is METH_VARARGS and ignores its arguments.
    #[method(name = "__reduce__", hint(py(text_signature = "")))]
    fn reduce(slf: This<Inst<'_, Range>>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = args;
        start_stop_step_reduce(it, &Value::Obj(slf.0 .0.clone()))
    }
}

/// slice(stop)
/// slice(start, stop[, step])
///
/// Create a slice object.  This is used for extended slicing (e.g. a[0:10:2]).
#[lumen_bind::class(name = "slice")]
pub struct Slice;

#[lumen_bind::methods]
impl Slice {
    #[constructor(hint(py(text_signature = "")))]
    fn new(a: &Value, b: Passed<&Value>, c: Passed<&Value>) -> Value {
        let (s, e, st) = match (b.0, c.0) {
            (None, _) => (Value::None, a.clone(), Value::None),
            (Some(b), None) => (a.clone(), b.clone(), Value::None),
            (Some(b), Some(c)) => (a.clone(), b.clone(), c.clone()),
        };
        Value::Obj(Object::new(Kind::Slice(s, e, st)))
    }

    /// S.indices(len) -> (start, stop, stride)
    ///
    /// Assuming a sequence of length len, calculate the start and stop
    /// indices, and the stride length of the extended slice described by
    /// S. Out of bounds indices are clipped in a manner consistent with the
    /// handling of normal slices.
    #[method(hint(py(text_signature = "")))]
    fn indices(slf: This<Inst<'_, Slice>>, it: &mut Interp, len: &Value) -> R<(i64, i64, i64)> {
        let len = it.index_of(len)?;
        if len < 0 {
            return Err(it.value_error("length should not be negative"));
        }
        it.slice_bounds(&Value::Obj(slf.0 .0.clone()), len as usize)
    }

    #[proto(repr)]
    fn repr(slf: This<Inst<'_, Slice>>, it: &mut Interp) -> R<String> {
        it.native_repr(&Value::Obj(slf.0 .0.clone()))
    }

    /// Return state information for pickling.
    #[method(name = "__reduce__", hint(py(text_signature = "")))]
    fn reduce(slf: This<Inst<'_, Slice>>, it: &mut Interp) -> R<Value> {
        start_stop_step_reduce(it, &Value::Obj(slf.0 .0.clone()))
    }
}

/// `range.__reduce__` and `slice.__reduce__`: `(type, (start, stop, step))`.
fn start_stop_step_reduce(it: &mut Interp, v: &Value) -> R<Value> {
    let mut parts = Vec::with_capacity(3);
    for name in ["start", "stop", "step"] {
        parts.push(it.get_attr_str(v, name)?);
    }
    let t = it.type_of(v);
    Ok(Value::tuple(vec![Value::Obj(t), Value::tuple(parts)]))
}

/// Return an enumerate object.
///
///   iterable
///     an object supporting iteration
///
/// The enumerate object yields pairs containing a count (from start, which
/// defaults to zero) and a value yielded by the iterable argument.
///
/// enumerate is useful for obtaining an indexed list:
///     (0, seq[0]), (1, seq[1]), (2, seq[2]), ...
#[lumen_bind::class(name = "enumerate")]
pub struct Enumerate;

#[lumen_bind::methods]
impl Enumerate {
    #[constructor(hint(py(text_signature = "(iterable, start=0)")))]
    fn new(
        cls: This<Value>,
        it: &mut Interp,
        #[kw] iterable: &Value,
        #[kw] start: Passed<&Value>,
    ) -> R<Value> {
        let src = it.get_iter(iterable)?;
        let idx = match start.0 {
            Some(v) => it.index_of(v)?,
            None => 0,
        };
        let base = it.types.enumerate.clone();
        Ok(new_iter(
            it,
            &cls,
            &base,
            IterState::Enumerate { it: src, idx },
        ))
    }

    /// See PEP 585
    #[classmethod(name = "__class_getitem__", hint(py(text_signature = "")))]
    fn class_getitem(cls: This<Value>, it: &mut Interp, item: &Value) -> Value {
        it.make_alias(cls.0, item)
    }
}

/// zip(*iterables, strict=False) --> Yield tuples until an input is exhausted.
///
///    >>> list(zip('abcdefg', range(3), range(4)))
///    [('a', 0, 0), ('b', 1, 1), ('c', 2, 2)]
///
/// The zip object yields n-length tuples, where n is the number of iterables
/// passed as positional arguments to zip().  The i-th element in every tuple
/// comes from the i-th iterable argument to zip().  This continues until the
/// shortest argument is exhausted.
///
/// If strict is true and one of the arguments is exhausted before the others,
/// raise a ValueError.
#[lumen_bind::class(name = "zip")]
pub struct Zip;

#[lumen_bind::methods]
impl Zip {
    #[constructor(hint(py(text_signature = "")))]
    fn new(
        cls: This<Value>,
        it: &mut Interp,
        #[varargs] iterables: &[Value],
        #[kwonly] strict: Option<&Value>,
    ) -> R<Value> {
        let strict = match strict {
            Some(v) => it.truthy(v)?,
            None => false,
        };
        let mut its = Vec::with_capacity(iterables.len());
        for v in iterables {
            its.push(it.get_iter(v)?);
        }
        let base = it.types.zip.clone();
        Ok(new_iter(it, &cls, &base, IterState::Zip { its, strict }))
    }

    /// Set state information for unpickling.
    #[method(name = "__setstate__", hint(py(text_signature = "")))]
    fn setstate(slf: This<IterRef<'_>>, it: &mut Interp, state: &Value) -> R<()> {
        let flag = it.truthy(state)?;
        if let IterState::Zip { strict, .. } = &mut *slf.0 .1.borrow_mut() {
            *strict = flag;
        }
        Ok(())
    }
}

/// Make an iterator that computes the function using arguments from
/// each of the iterables.  Stops when the shortest iterable is exhausted.
///
/// If strict is true and one of the arguments is exhausted before the others,
/// raise a ValueError.
#[lumen_bind::class(name = "map")]
pub struct Map;

#[lumen_bind::methods]
impl Map {
    #[constructor(hint(py(text_signature = "(function, iterable, /, *iterables, strict=False)")))]
    fn new(
        cls: This<Value>,
        it: &mut Interp,
        #[varargs] args: &[Value],
        #[kwonly] strict: Option<&Value>,
    ) -> R<Value> {
        if args.len() < 2 {
            return Err(it.type_error("map() must have at least two arguments."));
        }
        let strict = match strict {
            Some(v) => it.truthy(v)?,
            None => false,
        };
        let mut its = Vec::with_capacity(args.len() - 1);
        for v in &args[1..] {
            its.push(it.get_iter(v)?);
        }
        let base = it.types.map.clone();
        Ok(new_iter(
            it,
            &cls,
            &base,
            IterState::Map {
                f: args[0].clone(),
                its,
                strict,
            },
        ))
    }

    /// Set state information for unpickling.
    #[method(name = "__setstate__", hint(py(text_signature = "")))]
    fn setstate(slf: This<IterRef<'_>>, it: &mut Interp, state: &Value) -> R<()> {
        let flag = it.truthy(state)?;
        if let IterState::Map { strict, .. } = &mut *slf.0 .1.borrow_mut() {
            *strict = flag;
        }
        Ok(())
    }
}

/// filter(function or None, iterable) --> filter object
///
/// Return an iterator yielding those items of iterable for which function(item)
/// is true. If function is None, return the items that are true.
#[lumen_bind::class(name = "filter")]
pub struct Filter;

#[lumen_bind::methods]
impl Filter {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, function: &Value, iterable: &Value) -> R<Value> {
        let src = it.get_iter(iterable)?;
        let base = it.types.filter.clone();
        Ok(new_iter(
            it,
            &cls,
            &base,
            IterState::Filter {
                f: function.clone(),
                it: src,
            },
        ))
    }
}

/// Return a reverse iterator over the values of the given sequence.
#[lumen_bind::class(name = "reversed")]
pub struct Reversed;

#[lumen_bind::methods]
impl Reversed {
    #[constructor(hint(py(text_signature = "(sequence, /)")))]
    fn new(cls: This<Value>, it: &mut Interp, sequence: &Value) -> R<Value> {
        let seq = sequence;
        let base = it.types.reversed.clone();
        if let Value::Obj(o) = seq {
            if o.cls.is_some() {
                if let Some(m) = it.user_special(seq, "__reversed__") {
                    return it.call_user_special(seq, &m, Vec::new());
                }
            }
        }
        let t = it.type_of(seq);
        if let Some(m) = it.lookup_mro(&t, "__reversed__") {
            if !m.is_none() {
                let b = it.bind_descr(&m, seq, &t)?;
                return it.call(&b, Vec::new(), Vec::new());
            }
        }
        if it.lookup_mro(&t, "__getitem__").is_none() || it.lookup_mro(&t, "__len__").is_none() {
            let t = it.type_name_of(seq);
            return Err(it.type_error(&format!("'{}' object is not reversible", t)));
        }
        let n = it.len_of(seq)? as i64;
        Ok(new_iter(
            it,
            &cls,
            &base,
            IterState::Reversed {
                seq: seq.clone(),
                idx: n - 1,
            },
        ))
    }
}

/// The pickling and length-hint members of the builtin iterator types, installed into each type
/// that has them in CPython.
#[lumen_bind::class(name = "iterator", hint(py(shared)))]
pub struct IterMethods;

#[lumen_bind::methods]
impl IterMethods {
    /// Private method returning an estimate of len(list(it)).
    #[method(name = "__length_hint__", hint(py(text_signature = "")))]
    fn length_hint(slf: This<IterRef<'_>>) -> usize {
        match &*slf.0 .1.borrow() {
            IterState::List { list, idx } => match &list.kind {
                Kind::List(l) => l.borrow().len().saturating_sub(*idx),
                _ => 0,
            },
            IterState::Tuple { tup, idx } => match &tup.kind {
                Kind::Tuple(t) => t.len().saturating_sub(*idx),
                _ => 0,
            },
            IterState::Range { cur, stop, step } => slice_len(*cur, *stop, *step),
            _ => 0,
        }
    }

    /// Return state information for pickling.
    #[method(name = "__reduce__", hint(py(text_signature = "")))]
    fn reduce(slf: This<IterRef<'_>>, it: &mut Interp) -> R<Value> {
        iter_reduce(it, slf.0)
    }

    /// Set state information for unpickling.
    #[method(name = "__setstate__", hint(py(text_signature = "")))]
    fn setstate(slf: This<IterRef<'_>>, it: &mut Interp, state: &Value) -> R<()> {
        iter_setstate(it, slf.0 .1, state)
    }
}

fn builtin_fn(it: &mut Interp, name: &str) -> R<Value> {
    let b = Value::Obj(it.import_module("builtins")?);
    it.get_attr_str(&b, name)
}

/// The keys, values or items a dict or set iterator has left, from entry `pos`.
fn remaining_entries(d: &Obj, pos: usize, kind: Option<ViewKind>) -> Vec<Value> {
    let Some(pd) = crate::containers::pydict_of(d) else {
        return Vec::new();
    };
    let pd = pd.borrow();
    let mut out = Vec::new();
    let mut i = pos;
    while let Some(j) = pd.next_live(i) {
        let Some(e) = pd.get(j) else { break };
        out.push(match kind {
            None | Some(ViewKind::Keys) => e.key.clone(),
            Some(ViewKind::Values) => e.val.clone(),
            Some(ViewKind::Items) => Value::tuple(vec![e.key.clone(), e.val.clone()]),
        });
        i = j + 1;
    }
    out
}

/// `__reduce__` of the builtin iterators: CPython's `(callable, args[, index])` forms.
fn iter_reduce(it: &mut Interp, IterRef(o, st): IterRef<'_>) -> R<Value> {
    enum Plan {
        Indexed(&'static str, Value, i64),
        Done(&'static str, Value),
        Range(i64, i64, i64),
        Args(&'static str, Vec<Value>),
        Zip(Vec<Value>, bool),
        Map(Vec<Value>, bool),
        Unpicklable,
    }
    let plan = match &*st.borrow() {
        IterState::List { list, idx } => {
            Plan::Indexed("iter", Value::Obj(list.clone()), *idx as i64)
        }
        IterState::Tuple { tup, idx } => {
            Plan::Indexed("iter", Value::Obj(tup.clone()), *idx as i64)
        }
        IterState::Str { s, pos } => {
            let n = match &s.kind {
                Kind::Str(ps) => lumen_common::smuggle::code_points(&ps.s[..*pos]).count(),
                _ => 0,
            };
            Plan::Indexed("iter", Value::Obj(s.clone()), n as i64)
        }
        IterState::Bytes { b, idx } => Plan::Indexed("iter", Value::Obj(b.clone()), *idx as i64),
        IterState::Range { cur, stop, step } => Plan::Range(*cur, *stop, *step),
        IterState::Dict {
            dict, pos, kind, ..
        } => Plan::Done(
            "iter",
            Value::list(remaining_entries(dict, *pos, Some(*kind))),
        ),
        IterState::Set { set, pos, .. } => {
            Plan::Done("iter", Value::list(remaining_entries(set, *pos, None)))
        }
        IterState::Seq { idx, .. } if *idx == i64::MIN => {
            Plan::Done("iter", Value::tuple(Vec::new()))
        }
        IterState::Seq { obj, idx } => Plan::Indexed("iter", obj.clone(), *idx),
        IterState::CallIter { done: true, .. } => Plan::Done("iter", Value::tuple(Vec::new())),
        IterState::CallIter { f, sentinel, .. } => {
            Plan::Args("iter", vec![f.clone(), sentinel.clone()])
        }
        IterState::Reversed { seq, idx } if *idx < 0 => {
            let is_list = matches!(seq, Value::Obj(so) if matches!(so.kind, Kind::List(_)) && so.cls.is_none());
            Plan::Done(
                "reversed",
                if is_list {
                    Value::list(Vec::new())
                } else {
                    Value::tuple(Vec::new())
                },
            )
        }
        IterState::Reversed { seq, idx } => Plan::Indexed("reversed", seq.clone(), *idx),
        IterState::Enumerate { it: inner, idx } => {
            Plan::Args("enumerate", vec![inner.clone(), Value::Int(*idx)])
        }
        IterState::Zip { its, strict } => Plan::Zip(its.clone(), *strict),
        IterState::Map { f, its, strict } => {
            let mut args = vec![f.clone()];
            args.extend(its.iter().cloned());
            Plan::Map(args, *strict)
        }
        IterState::Filter { f, it: inner } => Plan::Args("filter", vec![f.clone(), inner.clone()]),
        IterState::Empty => Plan::Done("iter", Value::tuple(Vec::new())),
        IterState::Native(_) | IterState::Running => Plan::Unpicklable,
    };
    Ok(match plan {
        Plan::Indexed(f, seq, i) => {
            let f = builtin_fn(it, f)?;
            Value::tuple(vec![f, Value::tuple(vec![seq]), Value::Int(i)])
        }
        Plan::Done(f, seq) => {
            let f = builtin_fn(it, f)?;
            Value::tuple(vec![f, Value::tuple(vec![seq])])
        }
        Plan::Range(cur, stop, step) => {
            let n = slice_len(cur, stop, step) as i64;
            let end = cur.saturating_add(n.saturating_mul(step));
            let r = Value::Obj(Object::new(Kind::Range(RangeData {
                start: cur,
                stop: end,
                step,
            })));
            let f = builtin_fn(it, "iter")?;
            Value::tuple(vec![f, Value::tuple(vec![r]), Value::None])
        }
        Plan::Args(f, args) => {
            let f = builtin_fn(it, f)?;
            Value::tuple(vec![f, Value::tuple(args)])
        }
        Plan::Map(args, strict) => {
            let f = builtin_fn(it, "map")?;
            let mut out = vec![f, Value::tuple(args)];
            if strict {
                out.push(Value::Bool(true));
            }
            Value::tuple(out)
        }
        Plan::Zip(its, strict) => {
            let zip = Value::Obj(it.types.zip.clone());
            let mut out = vec![zip, Value::tuple(its)];
            if strict {
                out.push(Value::Bool(true));
            }
            Value::tuple(out)
        }
        Plan::Unpicklable => {
            let t = it.type_name(&it.type_of_obj(o));
            return Err(it.type_error(&format!("cannot pickle '{}' object", t)));
        }
    })
}

/// `__setstate__` of the indexed builtin iterators: the position, clamped as CPython does.
fn iter_setstate(it: &mut Interp, st: &RefCell<IterState>, state: &Value) -> R<()> {
    let i = it.index_of(state)?;
    let reversed_seq = match &*st.borrow() {
        IterState::Reversed { seq, idx } if *idx >= 0 => Some(seq.clone()),
        _ => None,
    };
    if let Some(seq) = reversed_seq {
        let n = it.len_of(&seq)? as i64;
        if let IterState::Reversed { idx, .. } = &mut *st.borrow_mut() {
            *idx = i.clamp(-1, n - 1);
        }
        return Ok(());
    }
    match &mut *st.borrow_mut() {
        IterState::List { list, idx } => {
            let n = match &list.kind {
                Kind::List(l) => l.borrow().len() as i64,
                _ => 0,
            };
            *idx = i.clamp(0, n) as usize;
        }
        IterState::Tuple { tup, idx } => {
            let n = match &tup.kind {
                Kind::Tuple(t) => t.len() as i64,
                _ => 0,
            };
            *idx = i.clamp(0, n) as usize;
        }
        IterState::Bytes { b, idx } => {
            let n = match &b.kind {
                Kind::Bytes(x) => x.len() as i64,
                Kind::ByteArray(x) => x.len() as i64,
                _ => 0,
            };
            *idx = i.clamp(0, n) as usize;
        }
        IterState::Str { s: so, pos } => {
            if let Kind::Str(ps) = &so.kind {
                let mut p = 0;
                for _ in 0..i.max(0) {
                    if p >= ps.s.len() {
                        break;
                    }
                    p += lumen_common::smuggle::decode_at(&ps.s, p).1;
                }
                *pos = p;
            }
        }
        IterState::Range { cur, stop, step } => {
            let n = slice_len(*cur, *stop, *step) as i64;
            *cur = cur.saturating_add(i.clamp(0, n).saturating_mul(*step));
        }
        IterState::Seq { idx, .. } if *idx != i64::MIN => *idx = i.max(0),
        _ => {}
    }
    Ok(())
}

pub fn init(it: &mut Interp) {
    use crate::bind::{extend_type, install_into};
    let range = it.types.range.clone();
    extend_type::<Range>(it, &range);
    reg_slots(
        it,
        &range,
        &[
            "__getitem__",
            "__len__",
            "__contains__",
            "__iter__",
            "__reversed__",
        ],
    );
    reg_compare(it, &range, false);

    let slice = it.types.slice.clone();
    extend_type::<Slice>(it, &slice);
    reg_compare(it, &slice, true);

    let types = &it.types;
    let (enumerate, zip, map, filter, reversed) = (
        types.enumerate.clone(),
        types.zip.clone(),
        types.map.clone(),
        types.filter.clone(),
        types.reversed.clone(),
    );
    extend_type::<Enumerate>(it, &enumerate);
    extend_type::<Zip>(it, &zip);
    extend_type::<Map>(it, &map);
    extend_type::<Filter>(it, &filter);
    extend_type::<Reversed>(it, &reversed);

    let types = &it.types;
    let all = ["__length_hint__", "__reduce__", "__setstate__"];
    let reduce_only = ["__reduce__"];
    let hint_reduce = ["__length_hint__", "__reduce__"];
    let iter_types: [(Obj, &[&str]); 17] = [
        (enumerate, &reduce_only),
        (zip, &reduce_only),
        (map, &reduce_only),
        (filter, &reduce_only),
        (reversed, &all),
        (types.list_iterator.clone(), &all),
        (types.list_reverseiterator.clone(), &all),
        (types.tuple_iterator.clone(), &all),
        (types.str_iterator.clone(), &all),
        (types.bytes_iterator.clone(), &all),
        (types.range_iterator.clone(), &all),
        (types.dict_keyiterator.clone(), &hint_reduce),
        (types.dict_valueiterator.clone(), &hint_reduce),
        (types.dict_itemiterator.clone(), &hint_reduce),
        (types.set_iterator.clone(), &hint_reduce),
        (types.iterator.clone(), &all),
        (types.callable_iterator.clone(), &reduce_only),
    ];
    for (t, members) in &iter_types {
        reg_iterator(it, t);
        install_into::<IterMethods>(t, members);
    }
}
