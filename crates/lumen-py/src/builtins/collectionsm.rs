//! `_collections`: `deque`, `defaultdict`, `_tuplegetter` and `_count_elements`.

use super::native::*;
use crate::ast::{BinOp, CmpOp};
use crate::object::*;
use crate::vm::*;
use std::collections::VecDeque;

pub struct Deque {
    items: VecDeque<Value>,
    maxlen: Option<usize>,
    state: u64,
}

struct DequeIter {
    deque: Value,
    idx: usize,
    state: u64,
    remaining: usize,
    reverse: bool,
}

pub struct TupleGetter {
    pub index: usize,
    pub doc: Value,
}

const MUTATED: &str = "deque mutated during iteration";

fn dq<X>(it: &mut Interp, v: &Value, f: impl FnOnce(&mut Deque) -> X) -> R<X> {
    match with_opaque::<Deque, _>(v, f) {
        Some(x) => Ok(x),
        None => Err(it.self_state_err("collections.deque")),
    }
}

fn is_deque(v: &Value) -> bool {
    with_opaque::<Deque, _>(v, |_| ()).is_some()
}

fn push_back(d: &mut Deque, v: Value) {
    d.items.push_back(v);
    if let Some(m) = d.maxlen {
        if d.items.len() > m {
            d.items.pop_front();
        }
    }
    d.state += 1;
}

fn push_front(d: &mut Deque, v: Value) {
    d.items.push_front(v);
    if let Some(m) = d.maxlen {
        if d.items.len() > m {
            d.items.pop_back();
        }
    }
    d.state += 1;
}

fn deque_new(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match a.first() {
        Some(Value::Obj(c)) => Ok(new_opaque(c, Deque { items: VecDeque::new(), maxlen: None, state: 0 })),
        _ => Err(it.type_error("deque.__new__(X): X is not a type object")),
    }
}

fn deque_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("deque", &a[1.min(a.len())..], kw, &["iterable", "maxlen"], 0)?;
    let maxlen = match &b[1] {
        Some(v) if !v.is_none() => {
            if !it.has_index(v) {
                return Err(it.type_error("an integer is required"));
            }
            let n = it.index_of(v)?;
            if n < 0 {
                return Err(it.value_error("maxlen must be non-negative"));
            }
            Some(n as usize)
        }
        _ => None,
    };
    dq(it, &a[0], |d| {
        d.maxlen = maxlen;
        d.items.clear();
        d.state += 1;
    })?;
    if let Some(src) = &b[0] {
        extend_with(it, &a[0], src, false)?;
    }
    Ok(Value::None)
}

fn extend_with(it: &mut Interp, target: &Value, src: &Value, left: bool) -> R<()> {
    let items = if src.is(target) { dq(it, target, |d| d.items.iter().cloned().collect::<Vec<_>>())? } else { it.iterate_to_vec(src)? };
    dq(it, target, |d| {
        for v in items {
            if left {
                push_front(d, v);
            } else {
                push_back(d, v);
            }
        }
    })
}

fn deque_append(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("append", a, 2, 2)?;
    dq(it, &a[0], |d| push_back(d, a[1].clone()))?;
    Ok(Value::None)
}

fn deque_appendleft(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("appendleft", a, 2, 2)?;
    dq(it, &a[0], |d| push_front(d, a[1].clone()))?;
    Ok(Value::None)
}

fn deque_pop(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("pop", a, 1, 1)?;
    match dq(it, &a[0], |d| {
        d.state += 1;
        d.items.pop_back()
    })? {
        Some(v) => Ok(v),
        None => Err(it.new_exc_str("IndexError", "pop from an empty deque")),
    }
}

fn deque_popleft(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("popleft", a, 1, 1)?;
    match dq(it, &a[0], |d| {
        d.state += 1;
        d.items.pop_front()
    })? {
        Some(v) => Ok(v),
        None => Err(it.new_exc_str("IndexError", "pop from an empty deque")),
    }
}

fn deque_extend(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("extend", a, 2, 2)?;
    extend_with(it, &a[0], &a[1], false)?;
    Ok(Value::None)
}

fn deque_extendleft(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("extendleft", a, 2, 2)?;
    extend_with(it, &a[0], &a[1], true)?;
    Ok(Value::None)
}

fn deque_clear(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("clear", a, 1, 1)?;
    let old = dq(it, &a[0], |d| {
        d.state += 1;
        std::mem::take(&mut d.items)
    })?;
    drop(old);
    Ok(Value::None)
}

fn deque_copy(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("copy", a, 1, 1)?;
    let (items, maxlen) = dq(it, &a[0], |d| (d.items.iter().cloned().collect::<Vec<_>>(), d.maxlen))?;
    let ty = it.type_of(&a[0]);
    let dt = deque_type(it);
    let exact = it.is_exact(&a[0], &dt);
    if exact {
        return Ok(new_opaque(&ty, Deque { items: items.into_iter().collect(), maxlen, state: 0 }));
    }
    let args = vec![Value::list(items), maxlen.map(|m| Value::Int(m as i64)).unwrap_or(Value::None)];
    it.call(&Value::Obj(ty), args, Vec::new())
}

fn deque_type(it: &mut Interp) -> Obj {
    let m = match dict_get_str(&it.modules, "_collections") {
        Some(Value::Obj(m)) => m,
        _ => unreachable!("_collections is loaded"),
    };
    let d = it.module_dict(&m);
    match dict_get_str(&d, "deque") {
        Some(Value::Obj(t)) => t,
        _ => unreachable!(),
    }
}

fn item_at(it: &mut Interp, v: &Value, i: usize) -> R<Option<Value>> {
    dq(it, v, |d| d.items.get(i).cloned())
}

fn state_of(it: &mut Interp, v: &Value) -> R<u64> {
    dq(it, v, |d| d.state)
}

fn len_of(it: &mut Interp, v: &Value) -> R<usize> {
    dq(it, v, |d| d.items.len())
}

fn deque_count(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("count", a, 2, 2)?;
    let st = state_of(it, &a[0])?;
    let n = len_of(it, &a[0])?;
    let mut count = 0;
    for i in 0..n {
        let Some(x) = item_at(it, &a[0], i)? else { break };
        if it.values_eq(&x, &a[1])? {
            count += 1;
        }
        if state_of(it, &a[0])? != st {
            return Err(it.new_exc_str("RuntimeError", MUTATED));
        }
    }
    Ok(Value::Int(count))
}

fn clamp_index(it: &mut Interp, v: &Value, len: usize) -> R<usize> {
    let i = it.index_of(v)?;
    Ok(if i < 0 { (i + len as i64).max(0) as usize } else { (i as usize).min(len) })
}

fn deque_index(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("index", a, 2, 4)?;
    let n = len_of(it, &a[0])?;
    let start = match a.get(2) {
        Some(v) => clamp_index(it, v, n)?,
        None => 0,
    };
    let stop = match a.get(3) {
        Some(v) => clamp_index(it, v, n)?,
        None => n,
    };
    let st = state_of(it, &a[0])?;
    for i in start..stop.min(n) {
        let Some(x) = item_at(it, &a[0], i)? else { break };
        if it.values_eq(&x, &a[1])? {
            return Ok(Value::Int(i as i64));
        }
        if state_of(it, &a[0])? != st {
            return Err(it.new_exc_str("RuntimeError", MUTATED));
        }
    }
    let r = it.repr_of(&a[1])?;
    Err(it.value_error(&format!("{} is not in deque", r)))
}

fn deque_insert(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("insert", a, 3, 3)?;
    let i = it.index_of(&a[1])?;
    let full = dq(it, &a[0], |d| d.maxlen.is_some_and(|m| d.items.len() >= m))?;
    if full {
        return Err(it.new_exc_str("IndexError", "deque already at its maximum size"));
    }
    dq(it, &a[0], |d| {
        let n = d.items.len() as i64;
        let pos = if i < 0 { (i + n).max(0) } else { i.min(n) } as usize;
        d.items.insert(pos, a[2].clone());
        d.state += 1;
    })?;
    Ok(Value::None)
}

fn deque_remove(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("remove", a, 2, 2)?;
    let st = state_of(it, &a[0])?;
    let n = len_of(it, &a[0])?;
    for i in 0..n {
        let Some(x) = item_at(it, &a[0], i)? else { break };
        let eq = it.values_eq(&x, &a[1])?;
        if state_of(it, &a[0])? != st {
            return Err(it.new_exc_str("IndexError", MUTATED));
        }
        if eq {
            dq(it, &a[0], |d| {
                d.items.remove(i);
                d.state += 1;
            })?;
            return Ok(Value::None);
        }
    }
    let r = it.repr_of(&a[1])?;
    Err(it.value_error(&format!("{} is not in deque", r)))
}

fn deque_reverse(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("reverse", a, 1, 1)?;
    dq(it, &a[0], |d| {
        d.items.make_contiguous().reverse();
        d.state += 1;
    })?;
    Ok(Value::None)
}

fn deque_rotate(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("rotate", a, 1, 2)?;
    let n = match a.get(1) {
        Some(v) => it.index_of(v)?,
        None => 1,
    };
    dq(it, &a[0], |d| {
        let len = d.items.len() as i64;
        if len > 1 {
            let k = n.rem_euclid(len) as usize;
            d.items.rotate_right(k);
        }
        d.state += 1;
    })?;
    Ok(Value::None)
}

fn deque_maxlen(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(dq(it, &a[0], |d| d.maxlen)?.map(|m| Value::Int(m as i64)).unwrap_or(Value::None))
}

fn deque_len(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(len_of(it, &a[0])? as i64))
}

fn deque_index_arg(it: &mut Interp, key: &Value, len: usize) -> R<usize> {
    if it.is_slice(key) {
        return Err(it.type_error("sequence index must be integer, not 'slice'"));
    }
    if !it.has_index(key) {
        let t = it.type_name_of(key);
        return Err(it.type_error(&format!("sequence index must be integer, not '{}'", t)));
    }
    let i = it.index_of(key)?;
    let j = if i < 0 { i + len as i64 } else { i };
    if j < 0 || j >= len as i64 {
        return Err(it.new_exc_str("IndexError", "deque index out of range"));
    }
    Ok(j as usize)
}

fn deque_getitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__getitem__", a, 2, 2)?;
    let n = len_of(it, &a[0])?;
    let i = deque_index_arg(it, &a[1], n)?;
    Ok(item_at(it, &a[0], i)?.unwrap_or(Value::None))
}

fn deque_setitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__setitem__", a, 3, 3)?;
    let n = len_of(it, &a[0])?;
    let i = deque_index_arg(it, &a[1], n)?;
    dq(it, &a[0], |d| {
        if let Some(slot) = d.items.get_mut(i) {
            *slot = a[2].clone();
        }
    })?;
    Ok(Value::None)
}

fn deque_delitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__delitem__", a, 2, 2)?;
    let n = len_of(it, &a[0])?;
    let i = deque_index_arg(it, &a[1], n)?;
    let old = dq(it, &a[0], |d| {
        d.state += 1;
        d.items.remove(i)
    })?;
    drop(old);
    Ok(Value::None)
}

fn deque_contains(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__contains__", a, 2, 2)?;
    let st = state_of(it, &a[0])?;
    let n = len_of(it, &a[0])?;
    for i in 0..n {
        let Some(x) = item_at(it, &a[0], i)? else { break };
        if it.values_eq(&x, &a[1])? {
            return Ok(Value::Bool(true));
        }
        if state_of(it, &a[0])? != st {
            return Err(it.new_exc_str("RuntimeError", MUTATED));
        }
    }
    Ok(Value::Bool(false))
}

fn deque_iter_obj(it: &mut Interp, deque: &Value, reverse: bool) -> R<Value> {
    let (st, n) = dq(it, deque, |d| (d.state, d.items.len()))?;
    let ty = iter_type(it, reverse);
    Ok(new_opaque(&ty, DequeIter { deque: deque.clone(), idx: 0, state: st, remaining: n, reverse }))
}

fn iter_type(it: &mut Interp, reverse: bool) -> Obj {
    let m = match dict_get_str(&it.modules, "_collections") {
        Some(Value::Obj(m)) => m,
        _ => unreachable!("_collections is loaded"),
    };
    let d = it.module_dict(&m);
    match dict_get_str(&d, if reverse { "_deque_reverse_iterator" } else { "_deque_iterator" }) {
        Some(Value::Obj(t)) => t,
        _ => unreachable!(),
    }
}

fn deque_iter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    deque_iter_obj(it, &a[0], false)
}

fn deque_reversed(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    deque_iter_obj(it, &a[0], true)
}

fn dit_next(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let Some((deque, idx, st, remaining, reverse)) = with_opaque::<DequeIter, _>(&a[0], |d| (d.deque.clone(), d.idx, d.state, d.remaining, d.reverse)) else {
        return Err(it.self_state_err("_deque_iterator"));
    };
    if state_of(it, &deque)? != st {
        with_opaque::<DequeIter, _>(&a[0], |d| d.remaining = 0);
        return Err(it.new_exc_str("RuntimeError", MUTATED));
    }
    if remaining == 0 {
        return Err(it.new_exc_str("StopIteration", ""));
    }
    let n = len_of(it, &deque)?;
    let pos = if reverse { n.checked_sub(1 + idx) } else { Some(idx) };
    let v = pos.and_then(|p| dq(it, &deque, |d| d.items.get(p).cloned()).ok().flatten());
    with_opaque::<DequeIter, _>(&a[0], |d| {
        d.idx += 1;
        d.remaining -= 1;
    });
    match v {
        Some(v) => Ok(v),
        None => Err(it.new_exc_str("StopIteration", "")),
    }
}

fn dit_iter(_it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(a[0].clone())
}

fn dit_len(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match with_opaque::<DequeIter, _>(&a[0], |d| d.remaining) {
        Some(n) => Ok(Value::Int(n as i64)),
        None => Err(it.self_state_err("_deque_iterator")),
    }
}

fn dit_new(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("_deque_iterator", &a[1.min(a.len())..], 1, 2)?;
    if !is_deque(&a[1]) {
        return Err(it.type_error("_deque_iterator() argument 1 must be collections.deque"));
    }
    deque_iter_obj(it, &a[1], false)
}

fn deque_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let Value::Obj(o) = &a[0] else { return Ok(Value::str("deque([])")) };
    let ty = it.type_of(&a[0]);
    let name = it.type_name(&ty);
    if it.repr_enter(o) {
        return Ok(Value::string(format!("{}(...)", name)));
    }
    let (items, maxlen) = dq(it, &a[0], |d| (d.items.iter().cloned().collect::<Vec<_>>(), d.maxlen))?;
    let mut parts = Vec::new();
    let mut err = None;
    for v in &items {
        match it.repr_of(v) {
            Ok(s) => parts.push(s),
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    it.repr_leave();
    if let Some(e) = err {
        return Err(e);
    }
    Ok(Value::string(match maxlen {
        Some(m) => format!("{}([{}], maxlen={})", name, parts.join(", "), m),
        None => format!("{}([{}])", name, parts.join(", ")),
    }))
}

fn deque_cmp(it: &mut Interp, a: &[Value], op: CmpOp) -> R<Value> {
    it.check_args("comparison", a, 2, 2)?;
    if !is_deque(&a[1]) {
        return Ok(Value::NotImplemented);
    }
    let x = Value::list(dq(it, &a[0], |d| d.items.iter().cloned().collect())?);
    let y = Value::list(dq(it, &a[1], |d| d.items.iter().cloned().collect())?);
    it.compare_op(op, &x, &y)
}

macro_rules! deque_cmp_fn {
    ($name:ident, $op:expr) => {
        fn $name(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
            deque_cmp(it, a, $op)
        }
    };
}
deque_cmp_fn!(deque_eq, CmpOp::Eq);
deque_cmp_fn!(deque_ne, CmpOp::NotEq);
deque_cmp_fn!(deque_lt, CmpOp::Lt);
deque_cmp_fn!(deque_le, CmpOp::LtE);
deque_cmp_fn!(deque_gt, CmpOp::Gt);
deque_cmp_fn!(deque_ge, CmpOp::GtE);

fn deque_add(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__add__", a, 2, 2)?;
    if !is_deque(&a[1]) {
        let t = it.type_name_of(&a[1]);
        return Err(it.type_error(&format!("can only concatenate deque (not \"{}\") to deque", t)));
    }
    let copy = deque_copy(it, &a[..1], &[])?;
    extend_with(it, &copy, &a[1], false)?;
    Ok(copy)
}

fn deque_iadd(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__iadd__", a, 2, 2)?;
    extend_with(it, &a[0], &a[1], false)?;
    Ok(a[0].clone())
}

fn repeat_items(it: &mut Interp, items: Vec<Value>, n: i64) -> R<Vec<Value>> {
    if n <= 0 || items.is_empty() {
        return Ok(Vec::new());
    }
    if (items.len() as i128) * (n as i128) > (isize::MAX as i128) / 16 {
        return Err(it.new_exc_str("MemoryError", ""));
    }
    let mut out = Vec::with_capacity(items.len() * n as usize);
    for _ in 0..n {
        out.extend(items.iter().cloned());
    }
    Ok(out)
}

fn deque_mul(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__mul__", a, 2, 2)?;
    if !it.has_index(&a[1]) {
        return Ok(Value::NotImplemented);
    }
    let n = it.index_of(&a[1])?;
    let copy = deque_copy(it, &a[..1], &[])?;
    deque_imul_n(it, &copy, n)?;
    Ok(copy)
}

fn deque_imul_n(it: &mut Interp, target: &Value, n: i64) -> R<()> {
    let items: Vec<Value> = dq(it, target, |d| d.items.iter().cloned().collect())?;
    let out = repeat_items(it, items, n)?;
    dq(it, target, |d| {
        d.items.clear();
        for v in out {
            push_back(d, v);
        }
        d.state += 1;
    })
}

fn deque_imul(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__imul__", a, 2, 2)?;
    if !it.has_index(&a[1]) {
        return Ok(Value::NotImplemented);
    }
    let n = it.index_of(&a[1])?;
    deque_imul_n(it, &a[0], n)?;
    Ok(a[0].clone())
}

fn deque_reduce(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let maxlen = dq(it, &a[0], |d| d.maxlen)?;
    let ty = Value::Obj(it.type_of(&a[0]));
    let args = match maxlen {
        Some(m) => Value::tuple(vec![Value::list(Vec::new()), Value::Int(m as i64)]),
        None => Value::tuple(Vec::new()),
    };
    let items = it.get_iter(&a[0])?;
    Ok(Value::tuple(vec![ty, args, Value::None, items]))
}

fn deque_sizeof(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(64 + 8 * len_of(it, &a[0])? as i64))
}

fn class_getitem(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__class_getitem__", a, 2, 2)?;
    Ok(it.make_alias(a[0].clone(), &a[1]))
}

// ---- defaultdict --------------------------------------------------------------------------------

fn factory_of(it: &mut Interp, v: &Value) -> Value {
    match v {
        Value::Obj(o) => {
            let d = it.instance_dict(o);
            dict_get_str(&d, "default_factory").unwrap_or(Value::None)
        }
        _ => Value::None,
    }
}

fn dd_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let factory = match a.get(1) {
        Some(f) => {
            if !f.is_none() && !it.is_callable(f) {
                return Err(it.type_error("first argument must be callable or None"));
            }
            f.clone()
        }
        None => Value::None,
    };
    if let Value::Obj(o) = &a[0] {
        let d = it.instance_dict(o);
        dict_set_str(&d, "default_factory", factory);
    }
    let dict_ty = it.types.dict.clone();
    let init = it.lookup_mro(&dict_ty, "__init__").ok_or_else(|| it.type_error("dict has no __init__"))?;
    let mut args = vec![a[0].clone()];
    args.extend(a.iter().skip(2).cloned());
    it.call(&init, args, kw.to_vec())?;
    Ok(Value::None)
}

fn dd_missing(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__missing__", a, 2, 2)?;
    let f = factory_of(it, &a[0]);
    if f.is_none() {
        return Err(it.new_exc_val("KeyError", a[1].clone()));
    }
    let v = it.call(&f, Vec::new(), Vec::new())?;
    it.setitem(&a[0], a[1].clone(), v.clone())?;
    Ok(v)
}

fn dd_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let ty = it.type_of(&a[0]);
    let name = it.type_name(&ty);
    let f = factory_of(it, &a[0]);
    let fr = if f.is_none() { "None".to_string() } else { it.repr_of(&f)? };
    let d = it.native_repr(&a[0])?;
    Ok(Value::string(format!("{}({}, {})", name, fr, d)))
}

fn dd_copy(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("copy", a, 1, 1)?;
    let f = factory_of(it, &a[0]);
    let ty = Value::Obj(it.type_of(&a[0]));
    it.call(&ty, vec![f, a[0].clone()], Vec::new())
}

fn dd_reduce(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let f = factory_of(it, &a[0]);
    let ty = Value::Obj(it.type_of(&a[0]));
    let args = if f.is_none() { Value::tuple(Vec::new()) } else { Value::tuple(vec![f]) };
    let items = it.call_method(&a[0], "items", Vec::new())?;
    let items = it.get_iter(&items)?;
    Ok(Value::tuple(vec![ty, args, Value::None, Value::None, items]))
}

fn dd_or(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__or__", a, 2, 2)?;
    let is_dict = matches!(&a[1], Value::Obj(o) if matches!(o.kind, Kind::Dict(_)));
    if !is_dict {
        return Ok(Value::NotImplemented);
    }
    let new = dd_copy(it, &a[..1], &[])?;
    let upd = it.get_attr_str(&new, "update")?;
    it.call(&upd, vec![a[1].clone()], Vec::new())?;
    Ok(new)
}

fn dd_ror(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__ror__", a, 2, 2)?;
    let is_dict = matches!(&a[1], Value::Obj(o) if matches!(o.kind, Kind::Dict(_)));
    if !is_dict {
        return Ok(Value::NotImplemented);
    }
    let f = factory_of(it, &a[0]);
    let ty = Value::Obj(it.type_of(&a[0]));
    let new = it.call(&ty, vec![f, a[1].clone()], Vec::new())?;
    let upd = it.get_attr_str(&new, "update")?;
    it.call(&upd, vec![a[0].clone()], Vec::new())?;
    Ok(new)
}

// ---- helpers ------------------------------------------------------------------------------------

fn tg_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("_tuplegetter", kw)?;
    it.check_args("_tuplegetter", &a[1.min(a.len())..], 2, 2)?;
    let index = it.index_of(&a[1])?;
    if index < 0 {
        return Err(it.value_error("index must be non-negative"));
    }
    let Value::Obj(cls) = &a[0] else { unreachable!() };
    Ok(new_opaque(cls, TupleGetter { index: index as usize, doc: a[2].clone() }))
}

fn tg_get(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__get__", a, 2, 3)?;
    if a[1].is_none() {
        return Ok(a[0].clone());
    }
    tuple_getter_get(it, &a[0], &a[1])
}

pub fn tuple_getter_get(it: &mut Interp, getter: &Value, obj: &Value) -> R<Value> {
    let Some(index) = with_opaque::<TupleGetter, _>(getter, |g| g.index) else { return Err(it.self_state_err("_tuplegetter")) };
    match obj.tuple_items() {
        Some(items) => match items.get(index) {
            Some(v) => Ok(v.clone()),
            None => Err(it.new_exc_str("IndexError", "tuple index out of range")),
        },
        None => {
            let t = it.type_name_of(obj);
            Err(it.type_error(&format!("descriptor for index '{}' for tuple subclasses doesn't apply to '{}' object", index, t)))
        }
    }
}

fn tg_set(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Err(it.new_exc_str("AttributeError", "can't set attribute"))
}

fn tg_doc(_it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(with_opaque::<TupleGetter, _>(&a[0], |g| g.doc.clone()).unwrap_or(Value::None))
}

fn count_elements(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("_count_elements", a, 2, 2)?;
    let get = it.get_attr_str(&a[0], "get")?;
    let src = it.get_iter(&a[1])?;
    while let Some(elem) = it.iter_next(&src)? {
        let old = it.call(&get, vec![elem.clone(), Value::Int(0)], Vec::new())?;
        let new = match &old {
            Value::Int(n) => match n.checked_add(1) {
                Some(s) => Value::Int(s),
                None => it.binary_op(BinOp::Add, &old, &Value::Int(1))?,
            },
            _ => it.binary_op(BinOp::Add, &old, &Value::Int(1))?,
        };
        it.setitem(&a[0], elem, new)?;
    }
    Ok(Value::None)
}

pub fn make(it: &mut Interp) -> Obj {
    let m = it.new_module("_collections");
    let d = it.module_dict(&m);
    it.register_module("_collections", &m);

    let deque = new_type(it, "collections", "deque", None, Layout::Other);
    it.reg_new(&deque, deque_new);
    let methods: &[(&'static str, NativeFn)] = &[
        ("__init__", deque_init),
        ("append", deque_append),
        ("appendleft", deque_appendleft),
        ("pop", deque_pop),
        ("popleft", deque_popleft),
        ("extend", deque_extend),
        ("extendleft", deque_extendleft),
        ("clear", deque_clear),
        ("copy", deque_copy),
        ("__copy__", deque_copy),
        ("count", deque_count),
        ("index", deque_index),
        ("insert", deque_insert),
        ("remove", deque_remove),
        ("reverse", deque_reverse),
        ("rotate", deque_rotate),
        ("__len__", deque_len),
        ("__getitem__", deque_getitem),
        ("__setitem__", deque_setitem),
        ("__delitem__", deque_delitem),
        ("__contains__", deque_contains),
        ("__iter__", deque_iter),
        ("__reversed__", deque_reversed),
        ("__repr__", deque_repr),
        ("__eq__", deque_eq),
        ("__ne__", deque_ne),
        ("__lt__", deque_lt),
        ("__le__", deque_le),
        ("__gt__", deque_gt),
        ("__ge__", deque_ge),
        ("__add__", deque_add),
        ("__iadd__", deque_iadd),
        ("__mul__", deque_mul),
        ("__rmul__", deque_mul),
        ("__imul__", deque_imul),
        ("__reduce__", deque_reduce),
        ("__sizeof__", deque_sizeof),
    ];
    for (n, f) in methods {
        it.reg(&deque, n, *f);
    }
    it.reg_prop(&deque, "maxlen", deque_maxlen);
    it.reg_class(&deque, "__class_getitem__", class_getitem);
    if let Some(dd) = deque.dict.borrow().as_ref() {
        dict_set_str(dd, "__hash__", Value::None);
    }
    set_type(&d, "deque", &deque);

    for (name, reverse) in [("_deque_iterator", false), ("_deque_reverse_iterator", true)] {
        let ty = new_type(it, "_collections", name, None, Layout::Other);
        it.reg(&ty, "__iter__", dit_iter);
        it.reg(&ty, "__next__", dit_next);
        it.reg(&ty, "__length_hint__", dit_len);
        if !reverse {
            it.reg_new(&ty, dit_new);
        }
        set_type(&d, name, &ty);
    }

    let dict_ty = it.types.dict.clone();
    let dd = new_type(it, "collections", "defaultdict", Some(&dict_ty), Layout::Dict);
    it.reg(&dd, "__init__", dd_init);
    it.reg(&dd, "__missing__", dd_missing);
    it.reg(&dd, "__repr__", dd_repr);
    it.reg(&dd, "copy", dd_copy);
    it.reg(&dd, "__copy__", dd_copy);
    it.reg(&dd, "__reduce__", dd_reduce);
    it.reg(&dd, "__or__", dd_or);
    it.reg(&dd, "__ror__", dd_ror);
    it.reg_class(&dd, "__class_getitem__", class_getitem);
    set_type(&d, "defaultdict", &dd);

    let tg = new_type(it, "_collections", "_tuplegetter", None, Layout::Other);
    it.reg_new(&tg, tg_new);
    it.reg(&tg, "__get__", tg_get);
    it.reg(&tg, "__set__", tg_set);
    it.reg(&tg, "__delete__", tg_set);
    it.reg_prop(&tg, "__doc__", tg_doc);
    set_type(&d, "_tuplegetter", &tg);

    set_fn(it, &d, "_count_elements", count_elements);
    m
}
