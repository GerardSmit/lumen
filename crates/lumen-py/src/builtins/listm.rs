//! `list` and `tuple` methods, and the stable sort shared with `sorted()`.

use super::slots::{reg_binops, reg_compare, reg_method_forms, reg_slots};
use crate::ast::CmpOp;
use crate::bind::{KwArgs, PyCx, PyHost, This};
use crate::object::*;
use crate::vm::*;
use lumen_bind::{FromArg, Passed, Slot};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

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


/// A `list` (or subclass instance): the receiver of the list methods.
#[derive(Clone, Copy)]
pub struct ListRef<'a>(pub &'a Value, pub &'a RefCell<Vec<Value>>);

impl<'a> FromArg<'a, PyHost> for ListRef<'a> {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match list_of(v) {
            Some(l) => Ok(ListRef(v, l)),
            None => Err(cx.arg_error(at, "list", v)),
        }
    }
}

/// A `tuple` (or subclass instance): the receiver of the tuple methods.
#[derive(Clone, Copy)]
pub struct TupleRef<'a>(pub &'a [Value]);

impl<'a> FromArg<'a, PyHost> for TupleRef<'a> {
    #[inline(always)]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v.tuple_items() {
            Some(t) => Ok(TupleRef(t)),
            None => Err(cx.arg_error(at, "tuple", v)),
        }
    }
}

#[lumen_bind::class(name = "list")]
pub struct List;

#[lumen_bind::methods]
impl List {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        let _ = (args, kwargs);
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        it.alloc_instance(cls)
    }

    #[proto(init)]
    fn init(slf: This<ListRef<'_>>, it: &mut Interp, iterable: Passed<&Value>) -> R<()> {
        let items = match iterable.0 {
            Some(src) => it.iterate_to_vec(src)?,
            None => Vec::new(),
        };
        *slf.0 .1.borrow_mut() = items;
        Ok(())
    }

    /// Append object to the end of the list.
    #[method]
    fn append(slf: This<ListRef<'_>>, object: &Value) {
        slf.0 .1.borrow_mut().push(object.clone());
    }

    /// Extend list by appending elements from the iterable.
    #[method]
    fn extend(slf: This<ListRef<'_>>, it: &mut Interp, iterable: &Value) -> R<()> {
        let items = it.iterate_to_vec(iterable)?;
        slf.0 .1.borrow_mut().extend(items);
        Ok(())
    }

    /// Insert object before index.
    #[method]
    fn insert(slf: This<ListRef<'_>>, index: isize, object: &Value) {
        let mut lm = slf.0 .1.borrow_mut();
        let (i, n) = (index as i64, lm.len() as i64);
        let pos = if i < 0 { (i + n).max(0) } else { i.min(n) };
        lm.insert(pos as usize, object.clone());
    }

    /// Remove and return item at index (default last).
    ///
    /// Raises IndexError if list is empty or index is out of range.
    #[method]
    fn pop(slf: This<ListRef<'_>>, it: &mut Interp, #[default(-1)] index: isize) -> R<Value> {
        let mut lm = slf.0 .1.borrow_mut();
        if lm.is_empty() {
            drop(lm);
            return Err(it.new_exc_str("IndexError", "pop from empty list"));
        }
        let (i, n) = (index as i64, lm.len() as i64);
        let j = if i < 0 { i + n } else { i };
        if j < 0 || j >= n {
            drop(lm);
            return Err(it.new_exc_str("IndexError", "pop index out of range"));
        }
        Ok(lm.remove(j as usize))
    }

    /// Remove first occurrence of value.
    ///
    /// Raises ValueError if the value is not present.
    #[method]
    fn remove(slf: This<ListRef<'_>>, it: &mut Interp, value: &Value) -> R<()> {
        let l = slf.0 .1;
        let mut i = 0;
        loop {
            let x = match l.borrow().get(i) {
                Some(x) => x.clone(),
                None => break,
            };
            if it.values_eq(&x, value)? {
                let mut lm = l.borrow_mut();
                if i < lm.len() {
                    lm.remove(i);
                }
                return Ok(());
            }
            i += 1;
        }
        Err(it.value_error("list.remove(x): x not in list"))
    }

    /// Return first index of value.
    ///
    /// Raises ValueError if the value is not present.
    #[method(hint(py(text_signature = "($self, value, start=0, stop=sys.maxsize, /)")))]
    fn index(slf: This<ListRef<'_>>, it: &mut Interp, value: &Value, start: Passed<&Value>, stop: Passed<&Value>) -> R<i64> {
        let items = slf.0 .1.borrow().clone();
        match seq_index(it, &items, value, start, stop)? {
            Some(i) => Ok(i),
            None => {
                let r = it.repr_of(value)?;
                Err(it.value_error(&format!("{} is not in list", r)))
            }
        }
    }

    /// Return number of occurrences of value.
    #[method]
    fn count(slf: This<ListRef<'_>>, it: &mut Interp, value: &Value) -> R<i64> {
        let items = slf.0 .1.borrow().clone();
        seq_count(it, &items, value)
    }

    /// Reverse *IN PLACE*.
    #[method]
    fn reverse(slf: This<ListRef<'_>>) {
        slf.0 .1.borrow_mut().reverse();
    }

    /// Sort the list in ascending order and return None.
    ///
    /// The sort is in-place (i.e. the list itself is modified) and stable (i.e. the
    /// order of two equal elements is maintained).
    ///
    /// If a key function is given, apply it once to each list item and sort them,
    /// ascending or descending, according to their function values.
    ///
    /// The reverse flag can be set to sort in descending order.
    #[method(hint(py(text_signature = "($self, /, *, key=None, reverse=False)")))]
    fn sort(slf: This<ListRef<'_>>, it: &mut Interp, #[kwonly] key: Option<&Value>, #[kwonly] #[default(false)] reverse: bool) -> R<()> {
        let l = slf.0 .1;
        let mut items = std::mem::take(&mut *l.borrow_mut());
        let r = it.sort_values(&mut items, key.cloned(), reverse);
        let modified = !l.borrow().is_empty();
        *l.borrow_mut() = items;
        r?;
        if modified {
            return Err(it.value_error("list modified during sort"));
        }
        Ok(())
    }

    /// Return a shallow copy of the list.
    #[method]
    fn copy(slf: This<ListRef<'_>>) -> Value {
        Value::list(slf.0 .1.borrow().clone())
    }

    /// Remove all items from list.
    #[method]
    fn clear(slf: This<ListRef<'_>>) {
        slf.0 .1.borrow_mut().clear();
    }

    #[proto(iadd)]
    fn iadd(slf: This<ListRef<'_>>, it: &mut Interp, value: &Value) -> R<Value> {
        let items = it.iterate_to_vec(value)?;
        slf.0 .1.borrow_mut().extend(items);
        Ok(slf.0 .0.clone())
    }

    #[proto(repr)]
    fn repr(slf: This<ListRef<'_>>, it: &mut Interp) -> R<String> {
        it.native_repr(slf.0 .0)
    }
}

/// The first index in `items[start:stop]` equal to `value` (`list.index` / `tuple.index`).
fn seq_index(it: &mut Interp, items: &[Value], value: &Value, start: Passed<&Value>, stop: Passed<&Value>) -> R<Option<i64>> {
    let n = items.len() as i64;
    let mut start = match start.0 {
        Some(v) => it.slice_index(v)?,
        None => 0,
    };
    let mut stop = match stop.0 {
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
        if it.values_eq(&items[i as usize], value)? {
            return Ok(Some(i));
        }
        i += 1;
    }
    Ok(None)
}

fn seq_count(it: &mut Interp, items: &[Value], x: &Value) -> R<i64> {
    let mut n = 0;
    for v in items {
        if it.values_eq(v, x)? {
            n += 1;
        }
    }
    Ok(n)
}

// ---- tuple -------------------------------------------------------------------------------------

#[lumen_bind::class(name = "tuple")]
pub struct Tuple;

#[lumen_bind::methods]
impl Tuple {
    #[constructor(hint(py(text_signature = "")))]
    fn new(cls: This<Value>, it: &mut Interp, iterable: Passed<&Value>) -> R<Value> {
        let Value::Obj(cls) = &*cls else { unreachable!("checked by the entry") };
        let exact = Rc::ptr_eq(cls, &it.types.tuple);
        let items = match iterable.0 {
            Some(src) => {
                if exact {
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
        if exact {
            Ok(Value::tuple(items))
        } else {
            Ok(Value::Obj(Object::with_cls(cls.clone(), Kind::Tuple(items))))
        }
    }

    /// Return first index of value.
    ///
    /// Raises ValueError if the value is not present.
    #[method(hint(py(text_signature = "($self, value, start=0, stop=sys.maxsize, /)")))]
    fn index(slf: This<TupleRef<'_>>, it: &mut Interp, value: &Value, start: Passed<&Value>, stop: Passed<&Value>) -> R<i64> {
        match seq_index(it, slf.0 .0, value, start, stop)? {
            Some(i) => Ok(i),
            None => Err(it.value_error("tuple.index(x): x not in tuple")),
        }
    }

    /// Return number of occurrences of value.
    #[method]
    fn count(slf: This<TupleRef<'_>>, it: &mut Interp, value: &Value) -> R<i64> {
        seq_count(it, slf.0 .0, value)
    }

    #[method(name = "__getnewargs__")]
    fn getnewargs(slf: This<TupleRef<'_>>) -> Value {
        Value::tuple(vec![Value::tuple(slf.0 .0.to_vec())])
    }

    #[proto(hash)]
    fn hash(slf: This<&Value>, it: &mut Interp) -> R<i64> {
        it.native_hash(&slf)
    }

    #[proto(repr)]
    fn repr(slf: This<&Value>, it: &mut Interp) -> R<String> {
        it.native_repr(&slf)
    }
}

pub fn init(it: &mut Interp) {
    let (list, tuple) = (it.types.list.clone(), it.types.tuple.clone());
    crate::bind::extend_type::<List>(it, &list);
    if let Some(d) = list.dict.borrow().as_ref() {
        dict_set_str(d, "__hash__", Value::None);
    }
    reg_slots(it, &list, &["__setitem__", "__delitem__", "__len__", "__contains__", "__iter__", "__reversed__"]);
    reg_method_forms(&list, &["__getitem__"]);
    reg_binops(it, &list, &["__add__", "__mul__", "__rmul__"]);
    reg_compare(it, &list, true);

    crate::bind::extend_type::<Tuple>(it, &tuple);
    reg_slots(it, &tuple, &["__getitem__", "__len__", "__contains__", "__iter__"]);
    reg_binops(it, &tuple, &["__add__", "__mul__", "__rmul__"]);
    reg_compare(it, &tuple, true);
}
