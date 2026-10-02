//! `list` and `tuple` methods, and the stable sort shared with `sorted()`.

use super::numeric::{reg_binops, reg_compare};
use super::slots::reg_slots;
use crate::ast::CmpOp;
use crate::object::*;
use crate::vm::*;
use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

type Kw<'a> = &'a [(Obj, Value)];

fn list_this<'a>(it: &mut Interp, a: &'a [Value], name: &str) -> R<&'a RefCell<Vec<Value>>> {
    match a.first().and_then(list_of) {
        Some(l) => Ok(l),
        None => {
            let t = a.first().map(|v| it.type_name_of(v)).unwrap_or_default();
            Err(it.type_error(&format!("descriptor '{}' for 'list' objects doesn't apply to a '{}' object", name, t)))
        }
    }
}

impl Interp {
    pub fn sort_values(&mut self, items: &mut Vec<Value>, key: Option<Value>, reverse: bool) -> R<()> {
        let n = items.len();
        if n < 2 {
            return Ok(());
        }
        let keys: Vec<Value> = match &key {
            Some(k) => {
                let mut ks = Vec::with_capacity(n);
                for v in items.iter() {
                    ks.push(self.call(k, vec![v.clone()], Vec::new())?);
                }
                ks
            }
            None => items.clone(),
        };
        let mut order: Vec<usize> = (0..n).collect();
        if reverse {
            order.reverse();
        }
        if keys.iter().all(|k| matches!(k, Value::Int(_))) {
            let ik: Vec<i64> = keys.iter().map(|k| if let Value::Int(i) = k { *i } else { 0 }).collect();
            self.sort_order(&mut order, |a, b| ik[a].cmp(&ik[b]))?;
        } else if keys.iter().all(|k| matches!(k, Value::Float(f) if !f.is_nan())) {
            let fk: Vec<f64> = keys.iter().map(|k| if let Value::Float(f) = k { *f } else { 0.0 }).collect();
            self.sort_order(&mut order, |a, b| fk[a].partial_cmp(&fk[b]).unwrap_or(Ordering::Equal))?;
        } else if keys.iter().all(|k| matches!(k, Value::Obj(o) if o.cls.is_none() && matches!(o.kind, Kind::Str(_)))) {
            let sk: Vec<&str> = keys.iter().map(|k| k.as_str().unwrap_or("")).collect();
            self.sort_order(&mut order, |a, b| lumen_common::smuggle::cmp_code_points(sk[a], sk[b]))?;
        } else {
            order = self.merge_sort_indices(order, &keys)?;
        }
        if reverse {
            order.reverse();
        }
        let old = std::mem::take(items);
        let mut slots: Vec<Option<Value>> = old.into_iter().map(Some).collect();
        *items = order.into_iter().filter_map(|i| slots[i].take()).collect();
        Ok(())
    }

    /// A stable sort of `order` that polls for interrupts between runs: sorted runs of
    /// `SORT_RUN` indices are merged pairwise.
    fn sort_order(&mut self, order: &mut Vec<usize>, mut cmp: impl FnMut(usize, usize) -> Ordering) -> R<()> {
        const SORT_RUN: usize = 1 << 15;
        let n = order.len();
        if n <= SORT_RUN {
            order.sort_by(|&a, &b| cmp(a, b));
            return Ok(());
        }
        for run in order.chunks_mut(SORT_RUN) {
            run.sort_by(|&a, &b| cmp(a, b));
            self.poll()?;
        }
        let mut buf: Vec<usize> = Vec::with_capacity(n);
        let mut width = SORT_RUN;
        while width < n {
            buf.clear();
            for pair in order.chunks(2 * width) {
                let (left, right) = pair.split_at(width.min(pair.len()));
                let (mut i, mut j) = (0, 0);
                while i < left.len() && j < right.len() {
                    if cmp(right[j], left[i]) == Ordering::Less {
                        buf.push(right[j]);
                        j += 1;
                    } else {
                        buf.push(left[i]);
                        i += 1;
                    }
                    if buf.len() & 0xffff == 0 {
                        self.poll()?;
                    }
                }
                buf.extend_from_slice(&left[i..]);
                buf.extend_from_slice(&right[j..]);
            }
            std::mem::swap(order, &mut buf);
            self.poll()?;
            width *= 2;
        }
        Ok(())
    }

    fn key_lt(&mut self, a: &Value, b: &Value) -> R<bool> {
        self.poll()?;
        let r = self.compare_op(CmpOp::Lt, a, b)?;
        self.truthy(&r)
    }

    fn merge_sort_indices(&mut self, idx: Vec<usize>, keys: &[Value]) -> R<Vec<usize>> {
        let n = idx.len();
        if n <= 1 {
            return Ok(idx);
        }
        if n <= 6 {
            let mut v = idx;
            for i in 1..n {
                let mut j = i;
                while j > 0 && self.key_lt(&keys[v[j]], &keys[v[j - 1]])? {
                    v.swap(j, j - 1);
                    j -= 1;
                }
            }
            return Ok(v);
        }
        let mut right = idx;
        let left_part: Vec<usize> = right.drain(..n / 2).collect();
        let l = self.merge_sort_indices(left_part, keys)?;
        let r = self.merge_sort_indices(right, keys)?;
        let mut out = Vec::with_capacity(n);
        let (mut i, mut j) = (0, 0);
        while i < l.len() && j < r.len() {
            if self.key_lt(&keys[r[j]], &keys[l[i]])? {
                out.push(r[j]);
                j += 1;
            } else {
                out.push(l[i]);
                i += 1;
            }
        }
        out.extend_from_slice(&l[i..]);
        out.extend_from_slice(&r[j..]);
        Ok(out)
    }
}

fn list_new(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    match a.first() {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => it.alloc_instance(c),
        _ => Err(it.type_error("list.__new__(X): X is not a type object")),
    }
}

fn list_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.no_kwargs("list", kw)?;
    it.check_args("list", &a[1.min(a.len())..], 0, 1)?;
    let l = list_this(it, a, "__init__")?;
    let items = match a.get(1) {
        Some(src) => it.iterate_to_vec(src)?,
        None => Vec::new(),
    };
    *l.borrow_mut() = items;
    Ok(Value::None)
}

fn append(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("list.append", a, 2, 2)?;
    list_this(it, a, "append")?.borrow_mut().push(a[1].clone());
    Ok(Value::None)
}

fn extend(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("list.extend", a, 2, 2)?;
    let l = list_this(it, a, "extend")?;
    let items = it.iterate_to_vec(&a[1])?;
    l.borrow_mut().extend(items);
    Ok(Value::None)
}

fn insert(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("list.insert", a, 3, 3)?;
    let l = list_this(it, a, "insert")?;
    let i = it.index_of(&a[1])?;
    let mut lm = l.borrow_mut();
    let n = lm.len() as i64;
    let pos = if i < 0 { (i + n).max(0) } else { i.min(n) };
    lm.insert(pos as usize, a[2].clone());
    Ok(Value::None)
}

fn pop(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("list.pop", a, 1, 2)?;
    let l = list_this(it, a, "pop")?;
    let i = match a.get(1) {
        Some(v) => it.index_of(v)?,
        None => -1,
    };
    let mut lm = l.borrow_mut();
    if lm.is_empty() {
        drop(lm);
        return Err(it.new_exc_str("IndexError", "pop from empty list"));
    }
    let n = lm.len() as i64;
    let j = if i < 0 { i + n } else { i };
    if j < 0 || j >= n {
        drop(lm);
        return Err(it.new_exc_str("IndexError", "pop index out of range"));
    }
    Ok(lm.remove(j as usize))
}

fn remove(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("list.remove", a, 2, 2)?;
    let l = list_this(it, a, "remove")?;
    let mut i = 0;
    loop {
        let x = match l.borrow().get(i) {
            Some(x) => x.clone(),
            None => break,
        };
        if it.values_eq(&x, &a[1])? {
            let mut lm = l.borrow_mut();
            if i < lm.len() {
                lm.remove(i);
            }
            return Ok(Value::None);
        }
        i += 1;
    }
    Err(it.value_error("list.remove(x): x not in list"))
}

fn seq_index(it: &mut Interp, items: Vec<Value>, a: &[Value], kind: &str) -> R<Value> {
    let n = items.len() as i64;
    let mut start = match a.get(2) {
        Some(v) => it.slice_index(v)?,
        None => 0,
    };
    let mut stop = match a.get(3) {
        Some(v) => it.slice_index(v)?,
        None => n,
    };
    if start < 0 {
        start = start.saturating_add(n).max(0);
    }
    if stop < 0 {
        stop = stop.saturating_add(n).max(0);
    }
    let stop = stop.min(n);
    let mut i = start;
    while i < stop {
        if it.values_eq(&items[i as usize], &a[1])? {
            return Ok(Value::Int(i));
        }
        i += 1;
    }
    if kind == "list" {
        Err(it.value_error("list.index(x): x not in list"))
    } else {
        Err(it.value_error("tuple.index(x): x not in tuple"))
    }
}

fn list_index(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("list.index", a, 2, 4)?;
    let items = list_this(it, a, "index")?.borrow().clone();
    seq_index(it, items, a, "list")
}

fn seq_count(it: &mut Interp, items: Vec<Value>, x: &Value) -> R<Value> {
    let mut n = 0;
    for v in items {
        if it.values_eq(&v, x)? {
            n += 1;
        }
    }
    Ok(Value::Int(n))
}

fn list_count(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("list.count", a, 2, 2)?;
    let items = list_this(it, a, "count")?.borrow().clone();
    seq_count(it, items, &a[1])
}

fn reverse(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("list.reverse", a, 1, 1)?;
    list_this(it, a, "reverse")?.borrow_mut().reverse();
    Ok(Value::None)
}

fn sort(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    if a.len() != 1 {
        return Err(it.type_error("sort() takes no positional arguments"));
    }
    let l = list_this(it, a, "sort")?;
    let mut key = None;
    let mut rev = false;
    for (k, v) in kw {
        match k.as_str_kind().unwrap_or("") {
            "key" => {
                if !v.is_none() {
                    key = Some(v.clone())
                }
            }
            "reverse" => rev = it.truthy(v)?,
            other => return Err(it.type_error(&format!("sort() got an unexpected keyword argument '{}'", other))),
        }
    }
    let mut items = std::mem::take(&mut *l.borrow_mut());
    let r = it.sort_values(&mut items, key, rev);
    *l.borrow_mut() = items;
    r?;
    Ok(Value::None)
}

fn copy(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("list.copy", a, 1, 1)?;
    Ok(Value::list(list_this(it, a, "copy")?.borrow().clone()))
}

fn clear(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("list.clear", a, 1, 1)?;
    list_this(it, a, "clear")?.borrow_mut().clear();
    Ok(Value::None)
}

fn list_iadd(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__iadd__", a, 2, 2)?;
    let l = list_this(it, a, "__iadd__")?;
    let items = it.iterate_to_vec(&a[1])?;
    l.borrow_mut().extend(items);
    Ok(a[0].clone())
}

fn seq_hash(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(it.native_hash(&a[0])?))
}

fn seq_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::string(it.native_repr(&a[0])?))
}

// ---- tuple -------------------------------------------------------------------------------------

fn tuple_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let cls = match a.first() {
        Some(Value::Obj(c)) if matches!(c.kind, Kind::Type(_)) => c.clone(),
        _ => return Err(it.type_error("tuple.__new__(X): X is not a type object")),
    };
    it.no_kwargs("tuple", kw)?;
    it.check_args("tuple", &a[1..], 0, 1)?;
    let items = match a.get(1) {
        Some(src) => {
            if Rc::ptr_eq(&cls, &it.types.tuple) {
                if let Value::Obj(o) = src {
                    if o.cls.is_none() && matches!(o.kind, Kind::Tuple(_)) {
                        return Ok(src.clone());
                    }
                }
            }
            it.iterate_to_vec(src)?
        }
        None => Vec::new(),
    };
    if Rc::ptr_eq(&cls, &it.types.tuple) {
        Ok(Value::tuple(items))
    } else {
        Ok(Value::Obj(Object::with_cls(cls, Kind::Tuple(items))))
    }
}

fn tuple_items<'a>(it: &mut Interp, a: &'a [Value]) -> R<&'a [Value]> {
    match a.first().and_then(|v| v.tuple_items()) {
        Some(t) => Ok(t),
        None => Err(it.type_error("descriptor requires a 'tuple' object")),
    }
}

fn tuple_index(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("tuple.index", a, 2, 4)?;
    let items = tuple_items(it, a)?.to_vec();
    seq_index(it, items, a, "tuple")
}

fn tuple_count(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("tuple.count", a, 2, 2)?;
    let items = tuple_items(it, a)?.to_vec();
    seq_count(it, items, &a[1])
}

fn tuple_getnewargs(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let items = tuple_items(it, a)?.to_vec();
    Ok(Value::tuple(vec![Value::tuple(items)]))
}

pub fn init(it: &mut Interp) {
    let (list, tuple) = (it.types.list.clone(), it.types.tuple.clone());
    it.reg_new(&list, list_new);
    it.reg(&list, "__init__", list_init);
    let methods: &[(&'static str, NativeFn)] = &[
        ("append", append),
        ("extend", extend),
        ("insert", insert),
        ("pop", pop),
        ("remove", remove),
        ("index", list_index),
        ("count", list_count),
        ("reverse", reverse),
        ("sort", sort),
        ("copy", copy),
        ("clear", clear),
        ("__iadd__", list_iadd),
        ("__hash__", seq_hash),
        ("__repr__", seq_repr),
    ];
    for (n, f) in methods {
        it.reg(&list, n, *f);
    }
    if let Some(d) = list.dict.borrow().as_ref() {
        dict_set_str(d, "__hash__", Value::None);
    }
    reg_slots(it, &list, &["__getitem__", "__setitem__", "__delitem__", "__len__", "__contains__", "__iter__", "__reversed__"]);
    reg_binops(it, &list, &["__add__", "__mul__", "__rmul__"]);
    reg_compare(it, &list, true);

    it.reg_new(&tuple, tuple_new);
    it.reg(&tuple, "index", tuple_index);
    it.reg(&tuple, "count", tuple_count);
    it.reg(&tuple, "__getnewargs__", tuple_getnewargs);
    it.reg(&tuple, "__hash__", seq_hash);
    it.reg(&tuple, "__repr__", seq_repr);
    reg_slots(it, &tuple, &["__getitem__", "__len__", "__contains__", "__iter__"]);
    reg_binops(it, &tuple, &["__add__", "__mul__", "__rmul__"]);
    reg_compare(it, &tuple, true);
}
