//! `range`, `slice`, `enumerate`, `zip`, `map`, `filter`, `reversed` and the iterator types.

use super::numeric::{reg_binops, reg_compare};
use super::slots::{reg_iterator, reg_slots};
use crate::object::*;
use crate::ops::slice_len;
use crate::vm::*;

type Kw<'a> = &'a [(Obj, Value)];

fn range_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("range", kw)?;
    let args = &a[1.min(a.len())..];
    if args.is_empty() || args.len() > 3 {
        return Err(if args.is_empty() {
            it.type_error("range expected at least 1 argument, got 0")
        } else {
            it.type_error(&format!("range expected at most 3 arguments, got {}", args.len()))
        });
    }
    let mut ints = Vec::with_capacity(3);
    let mut bigs: Vec<crate::pyint::BigInt> = Vec::with_capacity(3);
    let mut overflow = false;
    for v in args {
        if !it.has_index(v) {
            let t = it.type_name_of(v);
            return Err(it.type_error(&format!("'{}' object cannot be interpreted as an integer", t)));
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
        let (a0, a1, a2) = (it3.next(), it3.next(), it3.next());
        let (start, stop, step) = match (a0, a1, a2) {
            (Some(x), None, None) => (crate::pyint::BigInt::zero(), x, one),
            (Some(x), Some(y), None) => (x, y, one),
            (Some(x), Some(y), Some(z)) => (x, y, z),
            _ => return Err(it.type_error("range expected at least 1 argument, got 0")),
        };
        if step.is_zero() {
            return Err(it.value_error("range() arg 3 must not be zero"));
        }
        return Ok(Value::Obj(Object::new(Kind::BigRange(Box::new([start, stop, step])))));
    }
    let (start, stop, step) = match ints.len() {
        1 => (0, ints[0], 1),
        2 => (ints[0], ints[1], 1),
        _ => (ints[0], ints[1], ints[2]),
    };
    if step == 0 {
        return Err(it.value_error("range() arg 3 must not be zero"));
    }
    Ok(Value::Obj(Object::new(Kind::Range(RangeData { start, stop, step }))))
}

fn range_of<'a>(it: &mut Interp, a: &'a [Value]) -> R<&'a RangeData> {
    match a.first() {
        Some(Value::Obj(o)) => match &o.kind {
            Kind::Range(r) => Ok(r),
            _ => Err(it.type_error("descriptor requires a 'range' object")),
        },
        _ => Err(it.type_error("descriptor requires a 'range' object")),
    }
}

fn range_index(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("range.index", a, 2, 2)?;
    let r = range_of(it, a)?;
    let (start, stop, step) = (r.start, r.stop, r.step);
    if let Value::Int(i) = &a[1] {
        let len = slice_len(start, stop, step) as i64;
        let d = i - start;
        if d % step == 0 {
            let k = d / step;
            if k >= 0 && k < len {
                return Ok(Value::Int(k));
            }
        }
    }
    let s = it.repr_of(&a[1])?;
    Err(it.value_error(&format!("{} is not in range", s)))
}

fn range_count(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("range.count", a, 2, 2)?;
    Ok(Value::Int(if it.native_contains(&a[0], &a[1])? { 1 } else { 0 }))
}

fn range_hash(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(it.native_hash(&a[0])?))
}

fn range_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::string(it.native_repr(&a[0])?))
}

fn slice_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("slice", kw)?;
    let args = &a[1.min(a.len())..];
    let (s, e, st) = match args.len() {
        1 => (Value::None, args[0].clone(), Value::None),
        2 => (args[0].clone(), args[1].clone(), Value::None),
        3 => (args[0].clone(), args[1].clone(), args[2].clone()),
        0 => return Err(it.type_error("slice expected at least 1 argument, got 0")),
        n => return Err(it.type_error(&format!("slice expected at most 3 arguments, got {}", n))),
    };
    Ok(Value::Obj(Object::new(Kind::Slice(s, e, st))))
}

fn slice_indices(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("slice.indices", a, 2, 2)?;
    let len = it.index_of(&a[1])?;
    if len < 0 {
        return Err(it.value_error("length should not be negative"));
    }
    let (s, e, st) = it.slice_bounds(&a[0], len as usize)?;
    Ok(Value::tuple(vec![Value::Int(s), Value::Int(e), Value::Int(st)]))
}

fn slice_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::string(it.native_repr(&a[0])?))
}

fn enumerate_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("enumerate", &a[1.min(a.len())..], kw, &["iterable", "start"], 1)?;
    let src = it.get_iter(&b[0].clone().unwrap_or(Value::None))?;
    let start = match &b[1] {
        Some(v) => it.index_of(v)?,
        None => 0,
    };
    Ok(it.mk_iter(IterState::Enumerate { it: src, idx: start }))
}

fn zip_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let mut strict = false;
    for (k, v) in kw {
        match k.as_str_kind().unwrap_or("") {
            "strict" => strict = it.truthy(v)?,
            other => return Err(it.type_error(&format!("zip() got an unexpected keyword argument '{}'", other))),
        }
    }
    let mut its = Vec::new();
    for v in &a[1.min(a.len())..] {
        its.push(it.get_iter(v)?);
    }
    Ok(it.mk_iter(IterState::Zip { its, strict }))
}

fn map_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("map", kw)?;
    let args = &a[1.min(a.len())..];
    if args.len() < 2 {
        return Err(it.type_error("map() must have at least two arguments."));
    }
    let mut its = Vec::new();
    for v in &args[1..] {
        its.push(it.get_iter(v)?);
    }
    Ok(it.mk_iter(IterState::Map { f: args[0].clone(), its }))
}

fn filter_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("filter", kw)?;
    let args = &a[1.min(a.len())..];
    if args.len() != 2 {
        return Err(it.type_error(&format!("filter expected 2 arguments, got {}", args.len())));
    }
    let src = it.get_iter(&args[1])?;
    Ok(it.mk_iter(IterState::Filter { f: args[0].clone(), it: src }))
}

fn reversed_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("reversed", kw)?;
    let args = &a[1.min(a.len())..];
    if args.len() != 1 {
        return Err(it.type_error(&format!("reversed expected 1 argument, got {}", args.len())));
    }
    let seq = &args[0];
    if let Value::Obj(o) = seq {
        if o.cls.is_some() {
            if let Some(m) = it.user_special(seq, "__reversed__") {
                return it.call_user_special(seq, &m, Vec::new());
            }
        }
    }
    let cls = it.type_of(seq);
    if let Some(m) = it.lookup_mro(&cls, "__reversed__") {
        if !m.is_none() {
            let b = it.bind_descr(&m, seq, &cls)?;
            return it.call(&b, Vec::new(), Vec::new());
        }
    }
    if it.lookup_mro(&cls, "__getitem__").is_none() || it.lookup_mro(&cls, "__len__").is_none() {
        let t = it.type_name_of(seq);
        return Err(it.type_error(&format!("'{}' object is not reversible", t)));
    }
    let n = it.len_of(seq)? as i64;
    Ok(it.mk_iter(IterState::Reversed { seq: seq.clone(), idx: n - 1 }))
}

fn iter_length_hint(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let _ = it;
    if let Value::Obj(o) = &a[0] {
        if let Kind::Iter(st) = &o.kind {
            let n = match &*st.borrow() {
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
            };
            return Ok(Value::Int(n as i64));
        }
    }
    Ok(Value::Int(0))
}

pub fn init(it: &mut Interp) {
    let range = it.types.range.clone();
    it.reg_new(&range, range_new);
    it.reg(&range, "index", range_index);
    it.reg(&range, "count", range_count);
    it.reg(&range, "__hash__", range_hash);
    it.reg(&range, "__repr__", range_repr);
    reg_slots(it, &range, &["__getitem__", "__len__", "__contains__", "__iter__", "__reversed__"]);
    reg_compare(it, &range, false);

    let slice = it.types.slice.clone();
    it.reg_new(&slice, slice_new);
    it.reg(&slice, "indices", slice_indices);
    it.reg(&slice, "__repr__", slice_repr);
    reg_compare(it, &slice, true);

    let (enumerate, zip, map, filter, reversed) =
        (it.types.enumerate.clone(), it.types.zip.clone(), it.types.map.clone(), it.types.filter.clone(), it.types.reversed.clone());
    it.reg_new(&enumerate, enumerate_new);
    it.reg_new(&zip, zip_new);
    it.reg_new(&map, map_new);
    it.reg_new(&filter, filter_new);
    it.reg_new(&reversed, reversed_new);

    let iter_types = [
        enumerate,
        zip,
        map,
        filter,
        reversed,
        it.types.list_iterator.clone(),
        it.types.list_reverseiterator.clone(),
        it.types.tuple_iterator.clone(),
        it.types.str_iterator.clone(),
        it.types.bytes_iterator.clone(),
        it.types.range_iterator.clone(),
        it.types.dict_keyiterator.clone(),
        it.types.dict_valueiterator.clone(),
        it.types.dict_itemiterator.clone(),
        it.types.set_iterator.clone(),
        it.types.iterator.clone(),
        it.types.callable_iterator.clone(),
    ];
    for t in &iter_types {
        reg_iterator(it, t);
        it.reg(t, "__length_hint__", iter_length_hint);
    }
    let _ = reg_binops;
}
