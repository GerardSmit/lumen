//! `_collections`: `deque`, `defaultdict`, `_tuplegetter` and `_count_elements`.

use super::native::*;
use crate::ast::{BinOp, CmpOp};
use crate::bind::{opaque_instance, type_object, Inst, KwArgs, NativeError, NativeResult, Py, This};
use crate::object::*;
use crate::vm::*;
use std::collections::VecDeque;

#[lumen_bind::class(name = "_tuplegetter", module = "collections")]
pub struct TupleGetter {
    index: i64,
    doc: Value,
}

#[lumen_bind::methods]
impl TupleGetter {
    #[constructor]
    fn new(it: &mut Interp, index: &Value, doc: &Value) -> R<TupleGetter> {
        let index = it.index_of(index)?;
        Ok(TupleGetter { index, doc: doc.clone() })
    }

    #[method(name = "__get__")]
    fn get(slf: This<Value>, it: &mut Interp, obj: &Value, owner: Option<&Value>) -> R<Value> {
        if obj.is_none() {
            if owner.is_none_or(|o| o.is_none()) {
                return Err(it.type_error("__get__(None, None) is invalid"));
            }
            return Ok(slf.0);
        }
        tuple_getter_get(it, &slf.0, obj)
    }

    #[method(name = "__set__")]
    fn set(&self, it: &mut Interp, obj: &Value, value: &Value) -> R<()> {
        let _ = (obj, value);
        Err(it.new_exc_str("AttributeError", "can't set attribute"))
    }

    #[method(name = "__delete__")]
    fn delete(&self, it: &mut Interp, obj: &Value) -> R<()> {
        let _ = obj;
        Err(it.new_exc_str("AttributeError", "can't delete attribute"))
    }

    #[getter(name = "__doc__")]
    fn doc(&self) -> Value {
        self.doc.clone()
    }

    #[setter(name = "__doc__")]
    fn set_doc(&mut self, v: &Value) {
        self.doc = v.clone();
    }

    #[method(name = "__reduce__")]
    fn reduce(slf: This<Value>, it: &mut Interp) -> R<Value> {
        let Some((index, doc)) = with_opaque::<TupleGetter, _>(&slf.0, |g| (g.index, g.doc.clone())) else { return Err(it.self_state_err("_tuplegetter")) };
        let t = it.type_of(&slf.0);
        Ok(Value::tuple(vec![Value::Obj(t), Value::tuple(vec![Value::Int(index), doc])]))
    }
}

const MUTATED: &str = "deque mutated during iteration";

#[lumen_bind::class(name = "deque", module = "collections", generic, hint(py(unhashable)))]
pub struct Deque {
    items: VecDeque<Value>,
    maxlen: Option<usize>,
    state: u64,
}

impl Deque {
    fn push_back(&mut self, v: Value) {
        self.items.push_back(v);
        if self.maxlen.is_some_and(|m| self.items.len() > m) {
            self.items.pop_front();
        }
        self.state += 1;
    }

    fn push_front(&mut self, v: Value) {
        self.items.push_front(v);
        if self.maxlen.is_some_and(|m| self.items.len() > m) {
            self.items.pop_back();
        }
        self.state += 1;
    }

    fn snapshot(slf: &Py<Self>, it: &mut Interp) -> R<Vec<Value>> {
        Ok(slf.borrow(it)?.items.iter().cloned().collect())
    }

    fn extend_with(slf: &Py<Self>, it: &mut Interp, src: &Value, left: bool) -> R<()> {
        let items = if src.is(slf.value()) { Self::snapshot(slf, it)? } else { it.iterate_to_vec(src)? };
        let mut d = slf.borrow_mut(it)?;
        for v in items {
            if left {
                d.push_front(v);
            } else {
                d.push_back(v);
            }
        }
        Ok(())
    }

    /// The item at `i` unless the deque changed since `state` (Python code run by a comparison
    /// may mutate it).
    fn item_checked(slf: &Py<Self>, it: &mut Interp, i: usize, state: u64, exc: &str) -> R<Option<Value>> {
        let d = slf.borrow(it)?;
        if d.state != state {
            drop(d);
            return Err(it.new_exc_str(exc, MUTATED));
        }
        Ok(d.items.get(i).cloned())
    }

    /// The position of the first item equal to `value` in `start..stop`.
    fn find(slf: &Py<Self>, it: &mut Interp, value: &Value, start: usize, stop: usize, exc: &str) -> R<Option<usize>> {
        let state = slf.borrow(it)?.state;
        for i in start..stop {
            let Some(x) = Self::item_checked(slf, it, i, state, exc)? else { break };
            let eq = it.values_eq(&x, value)?;
            if slf.borrow(it)?.state != state {
                return Err(it.new_exc_str(exc, MUTATED));
            }
            if eq {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }

    fn index_arg(it: &mut Interp, key: &Value, len: usize) -> R<usize> {
        if it.is_slice(key) || !it.has_index(key) {
            let t = it.type_name_of(key);
            return Err(it.type_error(&format!("sequence index must be integer, not '{}'", t)));
        }
        let i = it.seq_index(key)?;
        let j = if i < 0 { i + len as i64 } else { i };
        if j < 0 || j >= len as i64 {
            return Err(it.new_exc_str("IndexError", "deque index out of range"));
        }
        Ok(j as usize)
    }

    fn clamp_index(it: &mut Interp, v: &Value, len: usize) -> R<usize> {
        let i = it.slice_index(v)?;
        Ok(if i < 0 { i.saturating_add(len as i64).max(0) as usize } else { (i as usize).min(len) })
    }

    fn compare(slf: Py<Self>, it: &mut Interp, other: &Value, op: CmpOp) -> R<Value> {
        let Some(other) = Py::<Deque>::from_value(it, other) else { return Ok(Value::NotImplemented) };
        let x = Value::list(Self::snapshot(&slf, it)?);
        let y = Value::list(Self::snapshot(&other, it)?);
        it.compare_op(op, &x, &y)
    }

    fn repeat(slf: &Py<Self>, it: &mut Interp, n: i64) -> R<()> {
        let items = Self::snapshot(slf, it)?;
        let mut out = Vec::new();
        if n > 0 && !items.is_empty() {
            if (items.len() as i128) * (n as i128) > (isize::MAX as i128) / 16 {
                return Err(it.new_exc_str("MemoryError", ""));
            }
            out.reserve(items.len() * n as usize);
            for _ in 0..n {
                out.extend(items.iter().cloned());
            }
        }
        let old = {
            let mut d = slf.borrow_mut(it)?;
            let old = std::mem::take(&mut d.items);
            for v in out {
                d.push_back(v);
            }
            d.state += 1;
            old
        };
        drop(old);
        Ok(())
    }
}

#[lumen_bind::methods]
impl Deque {
    #[constructor(hint(py(text_signature = "")))]
    fn new(#[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> Deque {
        let _ = (args, kwargs);
        Deque { items: VecDeque::new(), maxlen: None, state: 0 }
    }

    #[proto(init)]
    #[method(hint(py(text_signature = "($self, /, *args, **kwargs)")))]
    fn __init__(slf: This<Py<Self>>, it: &mut Interp, #[kw] iterable: Option<&Value>, #[kw] maxlen: Option<&Value>) -> R<()> {
        let slf = slf.0;
        let maxlen = match maxlen {
            Some(v) => {
                if !it.has_index(v) {
                    return Err(it.type_error("an integer is required"));
                }
                let n = it.index_of(v)?;
                if n < 0 {
                    return Err(it.value_error("maxlen must be non-negative"));
                }
                Some(n as usize)
            }
            None => None,
        };
        let old = {
            let mut d = slf.borrow_mut(it)?;
            d.maxlen = maxlen;
            d.state += 1;
            std::mem::take(&mut d.items)
        };
        drop(old);
        if let Some(src) = iterable {
            Self::extend_with(&slf, it, src, false)?;
        }
        Ok(())
    }

    #[method(hint(py(text_signature = "")))]
    fn append(&mut self, item: Value) {
        self.push_back(item);
    }

    #[method(hint(py(text_signature = "")))]
    fn appendleft(&mut self, item: Value) {
        self.push_front(item);
    }

    #[method(hint(py(text_signature = "")))]
    fn pop(&mut self) -> NativeResult<Value> {
        self.state += 1;
        self.items.pop_back().ok_or_else(|| NativeError::index_error("pop from an empty deque"))
    }

    #[method(hint(py(text_signature = "")))]
    fn popleft(&mut self) -> NativeResult<Value> {
        self.state += 1;
        self.items.pop_front().ok_or_else(|| NativeError::index_error("pop from an empty deque"))
    }

    #[method(hint(py(text_signature = "")))]
    fn extend(slf: This<Py<Self>>, it: &mut Interp, iterable: &Value) -> R<()> {
        let slf = slf.0;
        Self::extend_with(&slf, it, iterable, false)
    }

    #[method(hint(py(text_signature = "")))]
    fn extendleft(slf: This<Py<Self>>, it: &mut Interp, iterable: &Value) -> R<()> {
        let slf = slf.0;
        Self::extend_with(&slf, it, iterable, true)
    }

    #[method(hint(py(text_signature = "")))]
    fn clear(slf: This<Py<Self>>, it: &mut Interp) -> R<()> {
        let slf = slf.0;
        let old = {
            let mut d = slf.borrow_mut(it)?;
            d.state += 1;
            std::mem::take(&mut d.items)
        };
        drop(old);
        Ok(())
    }

    #[method(hint(py(aliases = "__copy__", text_signature = "")))]
    fn copy(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let slf = slf.0;
        let items = Self::snapshot(&slf, it)?;
        let maxlen = slf.borrow(it)?.maxlen;
        let ty = it.type_of(slf.value());
        let exact = type_object::<Deque>(it);
        if it.is_exact(slf.value(), &exact) {
            return Ok(opaque_instance(&ty, Deque { items: items.into(), maxlen, state: 0 }));
        }
        let args = vec![Value::list(items), maxlen.map(|m| Value::Int(m as i64)).unwrap_or(Value::None)];
        it.call(&Value::Obj(ty), args, Vec::new())
    }

    #[method(hint(py(text_signature = "")))]
    fn count(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<i64> {
        let slf = slf.0;
        let (state, n) = {
            let d = slf.borrow(it)?;
            (d.state, d.items.len())
        };
        let mut count = 0;
        for i in 0..n {
            let Some(x) = Self::item_checked(&slf, it, i, state, "RuntimeError")? else { break };
            if it.values_eq(&x, value)? {
                count += 1;
            }
            if slf.borrow(it)?.state != state {
                return Err(it.new_exc_str("RuntimeError", MUTATED));
            }
        }
        Ok(count)
    }

    #[method(hint(py(text_signature = "", arg_style = "parse")))]
    fn index(slf: This<Py<Self>>, it: &mut Interp, value: &Value, start: Option<&Value>, stop: Option<&Value>) -> R<usize> {
        let slf = slf.0;
        let n = slf.borrow(it)?.items.len();
        let start = match start {
            Some(v) => Self::clamp_index(it, v, n)?,
            None => 0,
        };
        let stop = match stop {
            Some(v) => Self::clamp_index(it, v, n)?,
            None => n,
        };
        if let Some(i) = Self::find(&slf, it, value, start, stop.min(n), "RuntimeError")? {
            return Ok(i);
        }
        let r = it.repr_of(value)?;
        Err(it.value_error(&format!("{} is not in deque", r)))
    }

    #[method(hint(py(text_signature = "", arg_style = "parse")))]
    fn insert(&mut self, index: isize, value: Value) -> NativeResult<()> {
        if self.maxlen.is_some_and(|m| self.items.len() >= m) {
            return Err(NativeError::index_error("deque already at its maximum size"));
        }
        let n = self.items.len() as isize;
        let pos = if index < 0 { (index + n).max(0) } else { index.min(n) } as usize;
        self.items.insert(pos, value);
        self.state += 1;
        Ok(())
    }

    #[method(hint(py(text_signature = "")))]
    fn remove(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
        let slf = slf.0;
        let n = slf.borrow(it)?.items.len();
        if let Some(i) = Self::find(&slf, it, value, 0, n, "IndexError")? {
            let old = {
                let mut d = slf.borrow_mut(it)?;
                d.state += 1;
                d.items.remove(i)
            };
            drop(old);
            return Ok(());
        }
        let r = it.repr_of(value)?;
        Err(it.value_error(&format!("{} is not in deque", r)))
    }

    #[method(hint(py(text_signature = "")))]
    fn reverse(&mut self) {
        self.items.make_contiguous().reverse();
        self.state += 1;
    }

    #[method(hint(py(text_signature = "", arg_name = "deque.rotate")))]
    fn rotate(&mut self, #[default(1)] n: isize) {
        let len = self.items.len() as isize;
        if len > 1 {
            self.items.rotate_right(n.rem_euclid(len) as usize);
        }
        self.state += 1;
    }

    #[getter]
    fn maxlen(&self) -> Option<usize> {
        self.maxlen
    }

    #[proto(len)]
    fn __len__(&self) -> usize {
        self.items.len()
    }

    #[proto(getitem)]
    fn __getitem__(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<Value> {
        let slf = slf.0;
        let n = slf.borrow(it)?.items.len();
        let i = Self::index_arg(it, key, n)?;
        let v = slf.borrow(it)?.items.get(i).cloned().unwrap_or(Value::None);
        Ok(v)
    }

    #[proto(setitem)]
    fn __setitem__(slf: This<Py<Self>>, it: &mut Interp, key: &Value, value: Value) -> R<()> {
        let slf = slf.0;
        let n = slf.borrow(it)?.items.len();
        let i = Self::index_arg(it, key, n)?;
        let old = slf.borrow_mut(it)?.items.get_mut(i).map(|slot| std::mem::replace(slot, value));
        drop(old);
        Ok(())
    }

    #[proto(delitem)]
    fn __delitem__(slf: This<Py<Self>>, it: &mut Interp, key: &Value) -> R<()> {
        let slf = slf.0;
        let n = slf.borrow(it)?.items.len();
        let i = Self::index_arg(it, key, n)?;
        let old = {
            let mut d = slf.borrow_mut(it)?;
            d.state += 1;
            d.items.remove(i)
        };
        drop(old);
        Ok(())
    }

    #[proto(contains)]
    fn __contains__(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<bool> {
        let slf = slf.0;
        let n = slf.borrow(it)?.items.len();
        Ok(Self::find(&slf, it, value, 0, n, "RuntimeError")?.is_some())
    }

    #[proto(iter)]
    fn __iter__(slf: This<Py<Self>>, it: &mut Interp) -> R<DequeIter> {
        let slf = slf.0;
        DequeIter::over(slf, it)
    }

    #[proto(reversed)]
    #[method(hint(py(text_signature = "")))]
    fn __reversed__(slf: This<Py<Self>>, it: &mut Interp) -> R<DequeRevIter> {
        let slf = slf.0;
        Ok(DequeRevIter(DequeIter::over(slf, it)?))
    }

    #[proto(repr)]
    fn __repr__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let slf = slf.0;
        let Value::Obj(o) = slf.value() else { return Ok(Value::str("deque([])")) };
        let ty = it.type_of(slf.value());
        let name = it.type_name(&ty);
        if it.repr_enter(o) {
            return Ok(Value::string(format!("{}(...)", name)));
        }
        let items = Self::snapshot(&slf, it);
        let maxlen = slf.borrow(it).map(|d| d.maxlen);
        let (items, maxlen) = match (items, maxlen) {
            (Ok(i), Ok(m)) => (i, m),
            (Err(e), _) | (_, Err(e)) => {
                it.repr_leave();
                return Err(e);
            }
        };
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

    #[proto(eq)]
    fn __eq__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
        let slf = slf.0;
        Self::compare(slf, it, other, CmpOp::Eq)
    }

    #[proto(ne)]
    fn __ne__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
        let slf = slf.0;
        Self::compare(slf, it, other, CmpOp::NotEq)
    }

    #[proto(lt)]
    fn __lt__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
        let slf = slf.0;
        Self::compare(slf, it, other, CmpOp::Lt)
    }

    #[proto(le)]
    fn __le__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
        let slf = slf.0;
        Self::compare(slf, it, other, CmpOp::LtE)
    }

    #[proto(gt)]
    fn __gt__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
        let slf = slf.0;
        Self::compare(slf, it, other, CmpOp::Gt)
    }

    #[proto(ge)]
    fn __ge__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
        let slf = slf.0;
        Self::compare(slf, it, other, CmpOp::GtE)
    }

    #[proto(add)]
    fn __add__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Value> {
        let slf = slf.0;
        if Py::<Deque>::from_value(it, other).is_none() {
            let t = it.type_name_of(other);
            return Err(it.type_error(&format!("can only concatenate deque (not \"{}\") to deque", t)));
        }
        let copy = Self::copy(This(slf), it)?;
        if let Some(copy_d) = Py::<Deque>::from_value(it, &copy) {
            Self::extend_with(&copy_d, it, other, false)?;
        }
        Ok(copy)
    }

    #[proto(iadd)]
    fn __iadd__(slf: This<Py<Self>>, it: &mut Interp, other: &Value) -> R<Py<Self>> {
        let slf = slf.0;
        Self::extend_with(&slf, it, other, false)?;
        Ok(slf)
    }

    #[proto(mul)]
    #[method(hint(py(aliases = "__rmul__")))]
    fn __mul__(slf: This<Py<Self>>, it: &mut Interp, n: &Value) -> R<Value> {
        let slf = slf.0;
        if !it.has_index(n) {
            return Ok(Value::NotImplemented);
        }
        let n = it.index_of(n)?;
        let copy = Self::copy(This(slf), it)?;
        if let Some(copy_d) = Py::<Deque>::from_value(it, &copy) {
            Self::repeat(&copy_d, it, n)?;
        }
        Ok(copy)
    }

    #[proto(imul)]
    fn __imul__(slf: This<Py<Self>>, it: &mut Interp, n: &Value) -> R<Value> {
        let slf = slf.0;
        if !it.has_index(n) {
            return Ok(Value::NotImplemented);
        }
        let n = it.index_of(n)?;
        Self::repeat(&slf, it, n)?;
        Ok(slf.into_value())
    }

    #[proto(reduce)]
    #[method(hint(py(text_signature = "")))]
    fn __reduce__(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let slf = slf.0;
        let maxlen = slf.borrow(it)?.maxlen;
        let ty = Value::Obj(it.type_of(slf.value()));
        let args = match maxlen {
            Some(m) => Value::tuple(vec![Value::list(Vec::new()), Value::Int(m as i64)]),
            None => Value::tuple(Vec::new()),
        };
        let items = it.get_iter(slf.value())?;
        Ok(Value::tuple(vec![ty, args, Value::None, items]))
    }

    #[proto(sizeof)]
    #[method(hint(py(text_signature = "")))]
    fn __sizeof__(&self) -> usize {
        64 + 8 * self.items.len()
    }
}

/// A forward iterator over a deque (the reverse one wraps the same state).
#[lumen_bind::class(name = "_deque_iterator", module = "_collections")]
pub struct DequeIter {
    deque: Py<Deque>,
    idx: usize,
    state: u64,
    remaining: usize,
}

impl DequeIter {
    fn over(deque: Py<Deque>, it: &mut Interp) -> R<DequeIter> {
        let (state, remaining) = {
            let d = deque.borrow(it)?;
            (d.state, d.items.len())
        };
        Ok(DequeIter { deque, idx: 0, state, remaining })
    }

    fn step(&mut self, it: &mut Interp, reverse: bool) -> R<Option<Value>> {
        let d = self.deque.borrow(it)?;
        if d.state != self.state {
            drop(d);
            self.remaining = 0;
            return Err(it.new_exc_str("RuntimeError", MUTATED));
        }
        if self.remaining == 0 {
            return Ok(None);
        }
        let n = d.items.len();
        let pos = if reverse { n.checked_sub(1 + self.idx) } else { Some(self.idx) };
        let v = pos.and_then(|p| d.items.get(p).cloned());
        self.idx += 1;
        self.remaining -= 1;
        Ok(v)
    }
}

#[lumen_bind::methods]
impl DequeIter {
    #[constructor(hint(py(text_signature = "")))]
    fn new(it: &mut Interp, deque: Py<Deque>, #[default(0)] index: usize) -> R<DequeIter> {
        let mut d = DequeIter::over(deque, it)?;
        for _ in 0..index.min(d.remaining) {
            d.step(it, false)?;
        }
        Ok(d)
    }

    #[proto(iter)]
    fn __iter__(slf: This<Py<Self>>) -> Py<Self> {
        slf.0
    }

    #[proto(next)]
    fn __next__(&mut self, it: &mut Interp) -> R<Option<Value>> {
        self.step(it, false)
    }

    fn __length_hint__(&self) -> usize {
        self.remaining
    }
}

#[lumen_bind::class(name = "_deque_reverse_iterator", module = "_collections")]
pub struct DequeRevIter(DequeIter);

#[lumen_bind::methods]
impl DequeRevIter {
    #[proto(iter)]
    fn __iter__(slf: This<Py<Self>>) -> Py<Self> {
        slf.0
    }

    #[proto(next)]
    fn __next__(&mut self, it: &mut Interp) -> R<Option<Value>> {
        self.0.step(it, true)
    }

    fn __length_hint__(&self) -> usize {
        self.0.remaining
    }
}

// ---- defaultdict --------------------------------------------------------------------------------

fn factory_of(it: &mut Interp, o: &Obj) -> Value {
    let d = it.instance_dict(o);
    dict_get_str(&d, "default_factory").unwrap_or(Value::None)
}

fn is_dict(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Dict(_)))
}

fn dd_copy(it: &mut Interp, o: &Obj) -> R<Value> {
    let f = factory_of(it, o);
    let ty = Value::Obj(it.type_of_obj(o));
    it.call(&ty, vec![f, Value::Obj(o.clone())], Vec::new())
}

type Dd<'a> = Inst<'a, DefaultDict>;

/// defaultdict(default_factory=None, /, [...]) --> dict with default factory
///
/// The default factory is called without arguments to produce
/// a new value when a key is not present, in __getitem__ only.
/// A defaultdict compares equal to a dict with the same items.
/// All remaining arguments are treated the same as if they were
/// passed to the dict constructor, including keyword arguments.
#[lumen_bind::class(name = "defaultdict", module = "collections")]
pub struct DefaultDict;

#[lumen_bind::methods]
impl DefaultDict {
    /// Initialize self.  See help(type(self)) for accurate signature.
    #[proto(init)]
    fn init(slf: This<Dd<'_>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
        let factory = match args.first() {
            Some(f) => {
                if !f.is_none() && !it.is_callable(f) {
                    return Err(it.type_error("first argument must be callable or None"));
                }
                f.clone()
            }
            None => Value::None,
        };
        let o = slf.0 .0;
        let d = it.instance_dict(o);
        dict_set_str(&d, "default_factory", factory);
        let dict_ty = it.types.dict.clone();
        let init = it.lookup_mro(&dict_ty, "__init__").ok_or_else(|| it.type_error("dict has no __init__"))?;
        let mut rest = vec![Value::Obj(o.clone())];
        rest.extend(args.iter().skip(1).cloned());
        it.call(&init, rest, kwargs.to_vec())?;
        Ok(())
    }

    /// __missing__(key) # Called by __getitem__ for missing key; pseudo-code:
    ///   if self.default_factory is None: raise KeyError((key,))
    ///   self[key] = value = self.default_factory()
    ///   return value
    #[method(name = "__missing__", hint(py(text_signature = "")))]
    fn missing(slf: This<Dd<'_>>, it: &mut Interp, key: &Value) -> R<Value> {
        let o = slf.0 .0;
        let f = factory_of(it, o);
        if f.is_none() {
            return Err(it.new_exc_val("KeyError", key.clone()));
        }
        let v = it.call(&f, Vec::new(), Vec::new())?;
        it.setitem(&Value::Obj(o.clone()), key.clone(), v.clone())?;
        Ok(v)
    }

    /// Return repr(self).
    #[proto(repr)]
    fn repr(slf: This<Dd<'_>>, it: &mut Interp) -> R<String> {
        let o = slf.0 .0;
        let ty = it.type_of_obj(o);
        let name = it.type_name(&ty);
        let f = factory_of(it, o);
        let d = it.native_repr(&Value::Obj(o.clone()))?;
        let fr = if f.is_none() {
            "None".to_string()
        } else if it.repr_enter(o) {
            "...".to_string()
        } else {
            let r = it.repr_of(&f);
            it.repr_leave();
            r?
        };
        Ok(format!("{}({}, {})", name, fr, d))
    }

    /// D.copy() -> a shallow copy of D.
    #[method(hint(py(text_signature = "", aliases = "__copy__")))]
    fn copy(slf: This<Dd<'_>>, it: &mut Interp) -> R<Value> {
        dd_copy(it, slf.0 .0)
    }

    /// Return state information for pickling.
    #[method(name = "__reduce__", hint(py(text_signature = "")))]
    fn reduce(slf: This<Dd<'_>>, it: &mut Interp) -> R<Value> {
        let o = slf.0 .0;
        let f = factory_of(it, o);
        let ty = Value::Obj(it.type_of_obj(o));
        let args = if f.is_none() { Value::tuple(Vec::new()) } else { Value::tuple(vec![f]) };
        let items = it.call_method(&Value::Obj(o.clone()), "items", Vec::new())?;
        let items = it.get_iter(&items)?;
        Ok(Value::tuple(vec![ty, args, Value::None, Value::None, items]))
    }

    /// Return self|value.
    #[proto(or)]
    fn or(slf: This<Dd<'_>>, it: &mut Interp, other: &Value) -> R<Value> {
        if !is_dict(other) {
            return Ok(Value::NotImplemented);
        }
        let new = dd_copy(it, slf.0 .0)?;
        let upd = it.get_attr_str(&new, "update")?;
        it.call(&upd, vec![other.clone()], Vec::new())?;
        Ok(new)
    }

    /// Return value|self.
    #[proto(ror)]
    fn ror(slf: This<Dd<'_>>, it: &mut Interp, other: &Value) -> R<Value> {
        if !is_dict(other) {
            return Ok(Value::NotImplemented);
        }
        let o = slf.0 .0;
        let f = factory_of(it, o);
        let ty = Value::Obj(it.type_of_obj(o));
        let new = it.call(&ty, vec![f, other.clone()], Vec::new())?;
        let upd = it.get_attr_str(&new, "update")?;
        it.call(&upd, vec![Value::Obj(o.clone())], Vec::new())?;
        Ok(new)
    }

    /// See PEP 585
    #[classmethod(name = "__class_getitem__", hint(py(text_signature = "")))]
    fn class_getitem(cls: This<Value>, it: &mut Interp, item: &Value) -> Value {
        it.make_alias(cls.0, item)
    }
}

// ---- helpers ------------------------------------------------------------------------------------

fn tuple_getter_get(it: &mut Interp, getter: &Value, obj: &Value) -> R<Value> {
    let Some(index) = with_opaque::<TupleGetter, _>(getter, |g| g.index) else { return Err(it.self_state_err("_tuplegetter")) };
    match obj.tuple_items() {
        Some(items) => match usize::try_from(index).ok().and_then(|i| items.get(i)) {
            Some(v) => Ok(v.clone()),
            None => Err(it.new_exc_str("IndexError", "tuple index out of range")),
        },
        None => {
            let t = it.type_name_of(obj);
            Err(it.type_error(&format!("descriptor for index '{}' for tuple subclasses doesn't apply to '{}' object", index, t)))
        }
    }
}

/// High performance data structures.
/// - deque:        ordered collection accessible from endpoints only
/// - defaultdict:  dict subclass with a default value factory
#[lumen_bind::module(name = "_collections")]
pub mod _collections {
    use super::*;

    /// Count elements in the iterable, updating the mapping
    #[op]
    fn _count_elements(it: &mut Interp, mapping: &Value, iterable: &Value) -> R<()> {
        let get = it.get_attr_str(mapping, "get")?;
        let src = it.get_iter(iterable)?;
        while let Some(elem) = it.iter_next(&src)? {
            let old = it.call(&get, vec![elem.clone(), Value::Int(0)], Vec::new())?;
            let new = match &old {
                Value::Int(n) => match n.checked_add(1) {
                    Some(s) => Value::Int(s),
                    None => it.binary_op(BinOp::Add, &old, &Value::Int(1))?,
                },
                _ => it.binary_op(BinOp::Add, &old, &Value::Int(1))?,
            };
            it.setitem(mapping, elem, new)?;
        }
        Ok(())
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let deque = type_object::<Deque>(it);
        set_type(&d, "deque", &deque);
        let ty = type_object::<DequeIter>(it);
        set_type(&d, "_deque_iterator", &ty);
        let ty = type_object::<DequeRevIter>(it);
        set_type(&d, "_deque_reverse_iterator", &ty);
        let dict_ty = it.types.dict.clone();
        let dd = new_type(it, "collections", "defaultdict", Some(&dict_ty), Layout::Dict);
        crate::bind::extend_type::<DefaultDict>(it, &dd);
        if let Some(td) = dd.dict.borrow().as_ref() {
            dict_set_str(td, "__doc__", <DefaultDict as lumen_bind::Class>::DESC.doc.map_or(Value::None, Value::str));
        }
        set_type(&d, "defaultdict", &dd);
        let tg = type_object::<TupleGetter>(it);
        set_type(&d, "_tuplegetter", &tg);
    }
}
