//! Dense-array fast paths for the `Array.prototype` methods.
//!
//! Every helper here performs exactly ONE spec step — HasProperty+Get, Get, Set(…, true),
//! DeletePropertyOrThrow, Set(O, "length", n, true) — and takes its fast route only when a guard
//! proves, at that very moment, that the step reduces to a plain slot operation with no
//! observable side effects (own plain data element on an ordinary object: no getter, no proxy
//! trap, no typed-array / arguments / module-namespace semantics). Otherwise the generic step
//! runs. Because the guard is re-evaluated per step, callbacks that mutate the receiver mid-walk
//! (adding accessors, punching holes, freezing, …) are handled exactly: the next step simply
//! takes the generic route.
//!
//! Result arrays (map/filter/slice/splice/concat/flat/Array.from) are accumulated in a `Vec`
//! while the result is provably unobservable — ArraySpeciesCreate took the default route, so
//! the fresh array is not reachable from JS until returned — and materialized in one go.

use super::*;
use crate::bytecode::PreparedCall;

/// The receiver's elements live in `props` with ordinary semantics.
#[inline(always)]
fn plain_elems(b: &Object) -> bool {
    b.ic_plain.get() && matches!(b.exotic, Exotic::Array | Exotic::None)
}

/// Own plain data element `k` of an ordinary object, read by index (no key string, no chain
/// walk). `None` = not provably such an element: the caller runs the generic step.
#[inline]
pub(super) fn own_elem(o: &Gc, k: usize) -> Option<Value> {
    let n = u32::try_from(k).ok()?;
    let b = o.borrow();
    if !plain_elems(&b) {
        return None;
    }
    let p = b.props.get_index(n)?;
    if p.accessor() {
        None
    } else {
        Some(p.value())
    }
}

/// Scan the packed plain elements `from..len` of `o` under one borrow for the first `hit`:
/// `Some(found)` when the whole range was read natively (no holes or accessors, which need the
/// generic per-index steps), `None` to fall back. Reading them runs no user code.
fn dense_find(o: &Gc, from: usize, len: usize, mut hit: impl FnMut(&Value) -> bool) -> Option<Option<usize>> {
    let b = o.borrow();
    if !plain_elems(&b) {
        return None;
    }
    let packed = b.props.packed_elements()?;
    if packed.len() < len {
        return None;
    }
    for (k, p) in packed.get(from..len)?.iter().enumerate() {
        if p.accessor() {
            return None;
        }
        let v = p.value();
        if matches!(v, Value::Empty) {
            return None;
        }
        if hit(&v) {
            return Some(Some(from + k));
        }
    }
    Some(None)
}

/// HasProperty(O, k) and, when present, Get(O, k).
#[inline]
pub(super) fn has_get(
    i: &mut Interp,
    o: &Gc,
    ov: &Value,
    k: usize,
) -> Result<Option<Value>, Value> {
    if let Some(v) = own_elem(o, k) {
        return Ok(Some(v));
    }
    let key = k.to_string();
    if !ab(i.js_has_property(ov, &key))? {
        return Ok(None);
    }
    Ok(Some(ab(i.get_member(ov, &key))?))
}

/// Get(O, k).
#[inline]
pub(super) fn get_elem(i: &mut Interp, o: &Gc, ov: &Value, k: usize) -> Result<Value, Value> {
    if let Some(v) = own_elem(o, k) {
        return Ok(v);
    }
    ab(i.get_member(ov, &k.to_string()))
}

/// Set(O, k, v, true). The fast route overwrites an existing own writable data element — the
/// OrdinarySet outcome whatever the prototype chain holds.
#[inline]
pub(super) fn set_elem(
    i: &mut Interp,
    o: &Gc,
    ov: &Value,
    k: usize,
    v: Value,
) -> Result<(), Value> {
    let v = match u32::try_from(k) {
        Ok(n) => {
            let mut b = o.borrow_mut();
            if plain_elems(&b) {
                match b.props.set_index_value(n, v) {
                    Ok(()) => return Ok(()),
                    Err(back) => back,
                }
            } else {
                v
            }
        }
        Err(_) => v,
    };
    set_throw(i, ov, &k.to_string(), v)
}

/// DeletePropertyOrThrow(O, k). The fast route pops a configurable tail element of an ordinary
/// Array (which is exactly what [[Delete]] does to it; `length` is untouched).
#[inline]
pub(super) fn delete_elem(i: &mut Interp, o: &Gc, ov: &Value, k: usize) -> Result<(), Value> {
    if let Ok(n) = u32::try_from(k) {
        let popped = {
            let mut b = o.borrow_mut();
            if b.ic_plain.get() && matches!(b.exotic, Exotic::Array) {
                b.props.pop_last_element(n)
            } else {
                None
            }
        };
        if popped.is_some() {
            return Ok(());
        }
    }
    delete_or_throw(i, ov, &k.to_string())
}

/// LengthOfArrayLike(O) (ToLength of `length`). An ordinary Array's own `length` is always a
/// valid uint32 data property, so no coercion can be observed.
#[inline]
pub(super) fn len_of(i: &mut Interp, o: &Gc) -> Result<usize, Value> {
    {
        let b = o.borrow();
        if b.ic_plain.get() && matches!(b.exotic, Exotic::Array) {
            if let Some(p) = b.props.length_property() {
                if !p.accessor() {
                    if let Value::Num(n) = p.value() {
                        return Ok(n as usize);
                    }
                }
            }
        }
    }
    ab(i.to_length(o))
}

/// [`len_of`] bounded by the engine's materialization cap (as `Interp::checked_array_len`).
#[inline]
pub(super) fn checked_len_of(i: &mut Interp, o: &Gc) -> Result<usize, Value> {
    let len = len_of(i, o)?;
    if len > MAX_ARRAY_OP_LEN {
        return Err(i.make_error("RangeError", "array length exceeds engine limit"));
    }
    Ok(len)
}

/// Set(O, "length", n, true). The fast route covers an ordinary Array with a writable `length`
/// when no element at or past `n` exists (growing, or shrinking over an already-emptied tail):
/// ArraySetLength then deletes nothing and just stores the value.
pub(super) fn set_len(i: &mut Interp, o: &Gc, ov: &Value, n: f64) -> Result<(), Value> {
    if n.fract() == 0.0 && (0.0..=4294967295.0).contains(&n) {
        let mut b = o.borrow_mut();
        if b.ic_plain.get() && matches!(b.exotic, Exotic::Array) {
            let cur = match b.props.length_property() {
                Some(p) if !p.accessor() && p.writable() => match p.value() {
                    Value::Num(c) => Some(c),
                    _ => None,
                },
                _ => None,
            };
            if let Some(cur) = cur {
                let tail_empty = n >= cur
                    || (cur - n <= 8.0
                        && (n as u64..cur as u64).all(|k| b.props.get(&k.to_string()).is_none()));
                if tail_empty {
                    if let Some(slot) = b.props.slot_of("length") {
                        if let Some(p) = b.props.entry_at_mut(slot) {
                            p.set_value(Value::Num(n));
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
    set_throw(i, ov, "length", Value::Num(n))
}

/// Whether `ctor` is an unmodified realm `Array` constructor whose `@@species` is still the
/// intrinsic getter (so `Get(C, @@species)` returns `C` with no observable effect).
fn species_is_default_array(i: &Interp, ctor: &Gc) -> bool {
    let array_ctor = match i.global.borrow().props.get("Array").map(|p| p.value()) {
        Some(Value::Obj(ac)) => ac,
        _ => return false,
    };
    if !Gc::ptr_eq(&array_ctor, ctor) {
        return false;
    }
    let Some(key) = well_known_key(i, "species") else {
        return false;
    };
    let b = ctor.borrow();
    if !b.ic_plain.get() {
        return false;
    }
    let Some(p) = b.props.get(&key) else {
        return false;
    };
    if !p.accessor() {
        return false;
    }
    let Some(Value::Obj(g)) = p.getter() else {
        return false;
    };
    let gb = g.borrow();
    matches!(gb.call, Callable::Native(f) if f as usize == nf_species_getter as NativeFn as usize)
}

/// ArraySpeciesCreate(original, len), except that the default outcome (a fresh ordinary Array,
/// unreachable from JS until the caller returns it) is reported as `None` so the caller can
/// accumulate elements natively. `Some(a)` is the (possibly species-constructed) result the
/// caller must fill with the generic CreateDataPropertyOrThrow steps.
pub(super) fn species(i: &mut Interp, original: &Value, len: usize) -> Result<Option<Value>, Value> {
    // Fast proof that the default route would be taken without running any user code: an
    // ordinary Array with no own `constructor`, whose prototype is the realm's Array.prototype
    // holding the unmodified Array constructor as a data property.
    if let Value::Obj(o) = original {
        let fast = {
            let b = o.borrow();
            b.ic_plain.get()
                && matches!(b.exotic, Exotic::Array)
                && b.proto.as_ref().is_some_and(|p| Gc::ptr_eq(p, &i.array_proto))
                && b.props.get("constructor").is_none()
        };
        if fast {
            let c = {
                let ap = i.array_proto.borrow();
                match ap.props.get("constructor") {
                    Some(p) if !p.accessor() => match p.value() {
                        Value::Obj(c) => Some(c),
                        _ => None,
                    },
                    _ => None,
                }
            };
            if let Some(c) = c {
                if species_is_default_array(i, &c) {
                    if len as u64 > 4294967295 {
                        return Err(i.make_error("RangeError", "invalid array length"));
                    }
                    return Ok(None);
                }
            }
        }
    }
    let (v, default) = array_species_create_ex(i, original, len)?;
    Ok(if default { None } else { Some(v) })
}

/// A result array under construction (see the module docs).
pub(super) struct Out {
    fast: Vec<Value>,
    /// The materialized result (species-constructed, or after a hole forced the sparse form).
    slow: Option<Value>,
    /// The length ArraySpeciesCreate gave the result.
    init_len: usize,
}

impl Out {
    /// For a [`species`] outcome.
    pub(super) fn new(custom: Option<Value>, init_len: usize) -> Out {
        Out {
            fast: Vec::with_capacity(init_len.min(4096)),
            slow: custom,
            init_len,
        }
    }

    /// CreateDataPropertyOrThrow(A, k, v).
    #[inline]
    pub(super) fn put(&mut self, i: &mut Interp, k: usize, v: Value) -> Result<(), Value> {
        if self.slow.is_none() && k == self.fast.len() {
            crate::value::push_value(&mut self.fast, v);
            return Ok(());
        }
        if self.slow.is_none() {
            // A gap: materialize the array built so far, then continue generically.
            let r = make_sparse_array(i, self.init_len)?;
            for (j, x) in std::mem::take(&mut self.fast).into_iter().enumerate() {
                cdp_or_throw(i, &r, &j.to_string(), x)?;
            }
            self.slow = Some(r);
        }
        let r = self.slow.clone().unwrap();
        cdp_or_throw(i, &r, &k.to_string(), v)
    }

    /// Bulk CreateDataPropertyOrThrow(A, k.., source[from..end]) for the leading run of the
    /// source's own plain data elements (see `Props::copy_dense_run`), while the result is
    /// still the unobservable fast vector and `k` its frontier. Returns how many elements were
    /// copied; the caller continues per index from there.
    #[inline]
    pub(super) fn put_run(&mut self, k: usize, o: &Gc, from: usize, end: usize) -> usize {
        if self.slow.is_some() || k != self.fast.len() {
            return 0;
        }
        let (Ok(from), Ok(end)) = (u32::try_from(from), u32::try_from(end)) else {
            return 0;
        };
        let b = o.borrow();
        if !plain_elems(&b) {
            return 0;
        }
        b.props.copy_dense_run(from, end, &mut self.fast) as usize
    }

    /// The finished array; `explicit_len` is a trailing Set(A, "length", n, true).
    pub(super) fn finish(self, i: &mut Interp, explicit_len: Option<usize>) -> Result<Value, Value> {
        match self.slow {
            Some(r) => {
                if let Some(n) = explicit_len {
                    set_length_throw(i, &r, n as f64)?;
                }
                Ok(r)
            }
            None => {
                let n = explicit_len.unwrap_or(self.init_len.max(self.fast.len()));
                let count = self.fast.len();
                let arr = i.make_array(self.fast);
                if n != count {
                    set_length_throw(i, &arr, n as f64)?;
                }
                Ok(arr)
            }
        }
    }
}

fn callback(i: &mut Interp, args: &[Value], what: &str) -> Result<(Value, Value), Value> {
    let cb = arg(args, 0);
    if !cb.is_callable() {
        return Err(i.make_error("TypeError", format!("{what} callback is not callable")));
    }
    Ok((cb, arg(args, 1)))
}

pub(super) fn array_for_each(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = len_of(i, &o)?;
    let (cb, cb_this) = callback(i, args, "Array.prototype.forEach")?;
    let ov = Value::Obj(o.clone());
    let mut f = PreparedCall::new(i, cb, cb_this);
    for k in 0..len {
        if let Some(v) = has_get(i, &o, &ov, k)? {
            f.call3(i, v, Value::Num(k as f64), &ov)?;
        }
    }
    f.finish(i);
    Ok(Value::Undefined)
}

pub(super) fn array_map(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = len_of(i, &o)?;
    let (cb, cb_this) = callback(i, args, "Array.prototype.map")?;
    let ov = Value::Obj(o.clone());
    let custom = species(i, &this, len)?;
    let mut out = Out::new(custom, len);
    let mut f = PreparedCall::new(i, cb, cb_this);
    for k in 0..len {
        if let Some(v) = has_get(i, &o, &ov, k)? {
            let mapped = f.call3(i, v, Value::Num(k as f64), &ov)?;
            out.put(i, k, mapped)?;
        }
    }
    f.finish(i);
    out.finish(i, None)
}

pub(super) fn array_filter(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = len_of(i, &o)?;
    let (cb, cb_this) = callback(i, args, "Array.prototype.filter")?;
    let ov = Value::Obj(o.clone());
    let custom = species(i, &this, 0)?;
    let mut out = Out::new(custom, 0);
    let mut to = 0usize;
    let mut f = PreparedCall::new(i, cb, cb_this);
    for k in 0..len {
        if let Some(v) = has_get(i, &o, &ov, k)? {
            let keep = f.call3(i, v.clone(), Value::Num(k as f64), &ov)?;
            if i.to_boolean(&keep) {
                out.put(i, to, v)?;
                to += 1;
            }
        }
    }
    f.finish(i);
    out.finish(i, None)
}

pub(super) fn array_reduce(
    i: &mut Interp,
    this: Value,
    args: &[Value],
    right: bool,
) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = len_of(i, &o)?;
    let cb = arg(args, 0);
    if !cb.is_callable() {
        let name = if right { "reduceRight" } else { "reduce" };
        return Err(i.make_error(
            "TypeError",
            format!("Array.prototype.{name} callback is not callable"),
        ));
    }
    let ov = Value::Obj(o.clone());
    // Visit order as indices; `step` walks it in the method's direction.
    let at = |s: usize| if right { len - 1 - s } else { s };
    let mut s = 0usize;
    let mut acc = if args.len() >= 2 {
        arg(args, 1)
    } else {
        // Seed with the first present element in visit order (holes are skipped).
        loop {
            if s >= len {
                return Err(i.make_error("TypeError", "Reduce of empty array with no initial value"));
            }
            let k = at(s);
            s += 1;
            if let Some(v) = has_get(i, &o, &ov, k)? {
                break v;
            }
        }
    };
    let mut f = PreparedCall::new(i, cb, Value::Undefined);
    while s < len {
        let k = at(s);
        s += 1;
        if let Some(v) = has_get(i, &o, &ov, k)? {
            acc = ab(f.call(i, &mut [acc, v, Value::Num(k as f64), ov.clone()]))?;
        }
    }
    f.finish(i);
    Ok(acc)
}

pub(super) fn array_find_impl(
    i: &mut Interp,
    this: Value,
    args: &[Value],
    want_value: bool,
    from_last: bool,
) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let ov = Value::Obj(o.clone());
    let len = len_of(i, &o)?;
    let cb = arg(args, 0);
    if !cb.is_callable() {
        return Err(i.make_error("TypeError", "predicate is not callable"));
    }
    let cb_this = arg(args, 1);
    let mut f = PreparedCall::new(i, cb, cb_this);
    for step in 0..len {
        let k = if from_last { len - 1 - step } else { step };
        let v = get_elem(i, &o, &ov, k)?;
        let r = f.call3(i, v.clone(), Value::Num(k as f64), &ov)?;
        if i.to_boolean(&r) {
            f.finish(i);
            return Ok(if want_value { v } else { Value::Num(k as f64) });
        }
    }
    f.finish(i);
    Ok(if want_value {
        Value::Undefined
    } else {
        Value::Num(-1.0)
    })
}

pub(super) fn array_some_every_impl(
    i: &mut Interp,
    this: Value,
    args: &[Value],
    every: bool,
) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = len_of(i, &o)?;
    let cb = arg(args, 0);
    if !cb.is_callable() {
        return Err(i.make_error("TypeError", "predicate is not callable"));
    }
    let cb_this = arg(args, 1);
    let ov = Value::Obj(o.clone());
    let mut f = PreparedCall::new(i, cb, cb_this);
    for k in 0..len {
        let Some(v) = has_get(i, &o, &ov, k)? else {
            continue;
        };
        let r = f.call3(i, v, Value::Num(k as f64), &ov)?;
        let b = i.to_boolean(&r);
        if every != b {
            f.finish(i);
            return Ok(Value::Bool(b));
        }
    }
    f.finish(i);
    Ok(Value::Bool(every))
}

/// Strict equality with no side effects (no coercion exists for `===`).
#[inline]
fn strict_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Num(x), Value::Num(y)) => x == y,
        (Value::Str(x), Value::Str(y)) => crate::lstr::LStr::ptr_eq(x, y) || x.as_str() == y.as_str(),
        (Value::Obj(x), Value::Obj(y)) => Gc::ptr_eq(x, y),
        (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        _ => false,
    }
}

pub(super) fn array_index_of(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = len_of(i, &o)?;
    if len == 0 {
        // The length check precedes the fromIndex coercion.
        return Ok(Value::Num(-1.0));
    }
    let target = arg(args, 0);
    let from = match arg(args, 1) {
        Value::Undefined => 0usize,
        v => {
            let n = ab(i.to_number(&v))?;
            if n == f64::INFINITY {
                return Ok(Value::Num(-1.0));
            }
            let n = if n.is_nan() { 0.0 } else { n.trunc() };
            if n >= 0.0 {
                n.min(len as f64) as usize
            } else {
                (len as f64 + n).max(0.0) as usize
            }
        }
    };
    let simple = matches!(
        target,
        Value::Num(_) | Value::Str(_) | Value::Obj(_) | Value::Undefined | Value::Null | Value::Bool(_)
    );
    if simple {
        if let Some(r) = dense_find(&o, from, len, |v| strict_eq(v, &target)) {
            return Ok(Value::Num(r.map_or(-1.0, |k| k as f64)));
        }
    }
    let ov = Value::Obj(o.clone());
    for k in from..len {
        let v = match own_elem(&o, k) {
            Some(v) => v,
            None => match has_get(i, &o, &ov, k)? {
                Some(v) => v,
                None => continue, // indexOf skips holes
            },
        };
        if if simple { strict_eq(&v, &target) } else { i.strict_equals(&v, &target) } {
            return Ok(Value::Num(k as f64));
        }
    }
    Ok(Value::Num(-1.0))
}

pub(super) fn array_last_index_of(
    i: &mut Interp,
    this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = len_of(i, &o)? as i64;
    if len == 0 {
        return Ok(Value::Num(-1.0));
    }
    let target = arg(args, 0);
    // fromIndex (default len-1): the highest index to search from, going backward.
    let mut k = if args.len() > 1 {
        let n = ab(i.to_number(&arg(args, 1)))?;
        if n == f64::NEG_INFINITY {
            return Ok(Value::Num(-1.0));
        }
        let n = if n.is_nan() { 0 } else { n.trunc() as i64 };
        if n >= 0 {
            n.min(len - 1)
        } else {
            len + n
        }
    } else {
        len - 1
    };
    let ov = Value::Obj(o.clone());
    while k >= 0 {
        if let Some(v) = has_get(i, &o, &ov, k as usize)? {
            if i.strict_equals(&v, &target) {
                return Ok(Value::Num(k as f64));
            }
        }
        k -= 1;
    }
    Ok(Value::Num(-1.0))
}

pub(super) fn array_includes(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = len_of(i, &o)? as i64;
    if len == 0 {
        // The length check precedes the fromIndex coercion.
        return Ok(Value::Bool(false));
    }
    let target = arg(args, 0);
    let mut k = match arg(args, 1) {
        Value::Undefined => 0i64,
        v => {
            let n = ab(i.to_number(&v))?;
            if n == f64::INFINITY {
                return Ok(Value::Bool(false));
            }
            if n == f64::NEG_INFINITY || n.is_nan() {
                0
            } else if n >= 0.0 {
                n.trunc().min(len as f64) as i64
            } else {
                (len + n.trunc() as i64).max(0)
            }
        }
    };
    if let Some(r) = dense_find(&o, k as usize, len as usize, |v| same_value_zero(v, &target)) {
        return Ok(Value::Bool(r.is_some()));
    }
    let ov = Value::Obj(o.clone());
    while k < len {
        let v = get_elem(i, &o, &ov, k as usize)?;
        if same_value_zero(&v, &target) {
            return Ok(Value::Bool(true));
        }
        k += 1;
    }
    Ok(Value::Bool(false))
}

/// Append ToString(v) of a join element (undefined/null contribute nothing).
fn push_join_part(i: &mut Interp, out: &mut String, v: &Value) -> Result<(), Value> {
    match v {
        Value::Undefined | Value::Null => {}
        Value::Str(s) => out.push_str(s),
        Value::Num(n) => {
            let n = *n;
            if n.trunc() == n && n.abs() < 1e15 {
                // Integers (-0 prints "0"): no shortest-float machinery.
                use std::fmt::Write;
                let _ = write!(out, "{}", n as i64);
            } else {
                out.push_str(&i.num_to_str(n));
            }
        }
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        other => out.push_str(&ab(i.to_string(other))?),
    }
    Ok(())
}

pub(super) fn array_join(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let ov = Value::Obj(o.clone());
    let len = checked_len_of(i, &o)?;
    let sep = match arg(args, 0) {
        Value::Undefined => crate::lstr::LStr::from(","),
        v => ab(i.to_string(&v))?,
    };
    let mut out = String::new();
    for k in 0..len {
        if k > 0 {
            out.push_str(&sep);
        }
        let v = get_elem(i, &o, &ov, k)?;
        push_join_part(i, &mut out, &v)?;
        if out.len() > MAX_STR_LEN {
            return Err(i.make_error("RangeError", "Invalid string length"));
        }
    }
    if !out.is_ascii() {
        if let Some(fixed) = crate::jstr::canonicalize(&out) {
            out = fixed;
        }
    }
    Ok(Value::from_string(out))
}

/// Short slices go through the generic `Out` path (its small-array allocation is already one
/// block); longer ones clone the packed run directly.
const INLINE_SLICE_MAX: usize = 16;

pub(super) fn array_slice(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    // The total length may be near 2^53 for an array-like; only the copied span (end-start) is
    // bounded by the engine's materialization cap.
    let len = len_of(i, &o)? as i64;
    let start = norm_index(ab(i.to_number(&arg(args, 0)))?, len);
    let end = match arg(args, 1) {
        Value::Undefined => len,
        v => norm_index(ab(i.to_number(&v))?, len),
    };
    let count = (end - start).max(0) as usize;
    let custom = species(i, &this, count)?;
    // Dense fast path: clone the packed run property by property into the result's storage.
    if custom.is_none() && count > INLINE_SLICE_MAX {
        let copy = {
            let b = o.borrow();
            plain_elems(&b)
                .then(|| crate::value::Object::slice_packed(&b, start as usize, end as usize, i.array_proto.clone()))
                .flatten()
        };
        if let Some(a) = copy {
            return Ok(Value::Obj(a));
        }
    }
    let mut out = Out::new(custom, count);
    let ov = Value::Obj(o.clone());
    let mut k = start;
    let mut to = 0usize;
    while k < end {
        let run = out.put_run(to, &o, k as usize, end as usize);
        if run != 0 {
            k += run as i64;
            to += run;
            continue;
        }
        // Preserve holes: only copy indices the source actually has (HasProperty).
        if let Some(v) = has_get(i, &o, &ov, k as usize)? {
            out.put(i, to, v)?;
        }
        k += 1;
        to += 1;
    }
    out.finish(i, Some(to))
}

pub(super) fn array_concat(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    // ToObject(this): a primitive receiver is boxed (and spread/appended as its wrapper).
    let recv = Value::Obj(arr_to_object(i, &this)?);
    let custom = species(i, &recv, 0)?;
    let mut out = Out::new(custom, 0);
    let mut n = 0u64;
    let spread_key = well_known_key(i, "isConcatSpreadable");
    for v in std::iter::once(&recv).chain(args.iter()) {
        // IsConcatSpreadable: @@isConcatSpreadable if defined, else IsArray.
        let spreadable = if let Value::Obj(_) = v {
            let flag = match &spread_key {
                Some(k) => ab(i.get_member(v, k))?,
                None => Value::Undefined,
            };
            match flag {
                Value::Undefined => json_is_array(i, v)?,
                other => i.to_boolean(&other),
            }
        } else {
            false
        };
        if spreadable {
            let Value::Obj(eo) = v else { unreachable!() };
            let len = len_of(i, eo)? as u64;
            if n + len > 9007199254740991 {
                return Err(i.make_error("TypeError", "concat result is too long"));
            }
            let mut k = 0;
            while k < len {
                let run = out.put_run((n + k) as usize, eo, k as usize, len as usize);
                if run != 0 {
                    k += run as u64;
                    continue;
                }
                if let Some(elem) = has_get(i, eo, v, k as usize)? {
                    out.put(i, (n + k) as usize, elem)?;
                }
                k += 1;
            }
            n += len; // holes keep their positions
        } else {
            if n >= 9007199254740991 {
                return Err(i.make_error("TypeError", "concat result is too long"));
            }
            out.put(i, n as usize, v.clone())?;
            n += 1;
        }
    }
    out.finish(i, Some(n as usize))
}

pub(super) fn array_splice_impl(
    i: &mut Interp,
    this: Value,
    args: &[Value],
) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let ov = Value::Obj(o.clone());
    let len = len_of(i, &o)? as i64;
    let start = norm_index(ab(i.to_number(&arg(args, 0)))?, len);
    let delete_count = if args.is_empty() {
        0
    } else if args.len() < 2 {
        len - start
    } else {
        let d = ab(i.to_number(&arg(args, 1)))?;
        let d = if d.is_nan() { 0.0 } else { d.trunc() };
        (d.max(0.0).min((len - start) as f64)) as i64
    };
    let items: &[Value] = if args.len() > 2 { &args[2..] } else { &[] };
    // A result length past 2^53-1 is a TypeError (before any mutation or species construction).
    if (len - delete_count + items.len() as i64) as u64 > 9007199254740991 {
        return Err(i.make_error("TypeError", "splice result is too long"));
    }
    // The removed array (ArraySpeciesCreate) preserves holes via HasProperty.
    let custom = species(i, &this, delete_count.max(0) as usize)?;
    // Dense fast path: a packed array of plain elements splices its buffer directly (when it
    // grows, the new indices must not reach a prototype setter).
    let item_count = items.len() as i64;
    if custom.is_none()
        && matches!(o.borrow().exotic, Exotic::Array)
        && i.ordinary_get_ptr(Gc::as_ptr(&o) as usize)
        && (item_count <= delete_count || (o.borrow().extensible && i.array_append_unshadowed(&o)))
    {
        let mut b = o.borrow_mut();
        let new_len = len - delete_count + item_count;
        if plain_len(&b) == Some(len as u32) && new_len <= u32::MAX as i64 {
            if let Some(removed) =
                b.props.splice_packed(len as u32, start as usize, delete_count as usize, items)
            {
                store_len(&mut b, new_len as u32);
                drop(b);
                return Ok(i.make_array(removed));
            }
        }
    }
    let mut removed = Out::new(custom, delete_count.max(0) as usize);
    for k in 0..delete_count {
        if let Some(v) = has_get(i, &o, &ov, (start + k) as usize)? {
            removed.put(i, k as usize, v)?;
        }
    }
    let removed = removed.finish(i, Some(delete_count as usize))?;
    // Shift the trailing elements (preserving holes) to open or close the gap.
    if item_count < delete_count {
        for k in start..(len - delete_count) {
            let from = (k + delete_count) as usize;
            let to = (k + item_count) as usize;
            match has_get(i, &o, &ov, from)? {
                Some(v) => set_elem(i, &o, &ov, to, v)?,
                None => delete_elem(i, &o, &ov, to)?,
            }
        }
        for k in ((len - delete_count + item_count)..len).rev() {
            delete_elem(i, &o, &ov, k as usize)?;
        }
    } else if item_count > delete_count {
        for k in ((start + 1)..=(len - delete_count)).rev() {
            let from = (k + delete_count - 1) as usize;
            let to = (k + item_count - 1) as usize;
            match has_get(i, &o, &ov, from)? {
                Some(v) => set_elem(i, &o, &ov, to, v)?,
                None => delete_elem(i, &o, &ov, to)?,
            }
        }
    }
    for (off, v) in items.iter().enumerate() {
        set_elem(i, &o, &ov, (start + off as i64) as usize, v.clone())?;
    }
    set_len(i, &o, &ov, (len - delete_count + item_count) as f64)?;
    Ok(removed)
}

/// The array's own writable data `length` as a `u32`, for the dense fast paths.
fn plain_len(o: &crate::value::Object) -> Option<u32> {
    match o.props.length_property() {
        Some(p) if !p.accessor() && p.writable() => match p.value() {
            Value::Num(n) if n.trunc() == n && (0.0..=u32::MAX as f64).contains(&n) => Some(n as u32),
            _ => None,
        },
        _ => None,
    }
}

fn store_len(o: &mut crate::value::Object, n: u32) {
    let s = o.props.slot_of("length").unwrap();
    o.props.entry_at_mut(s).unwrap().set_value(Value::Num(n as f64));
}

pub(super) fn array_shift(i: &mut Interp, this: Value, _args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    // Dense fast path: a packed array of plain elements drops its first slot in O(1).
    if matches!(o.borrow().exotic, Exotic::Array) && i.ordinary_get_ptr(Gc::as_ptr(&o) as usize) {
        let mut b = o.borrow_mut();
        if let Some(len) = plain_len(&b).filter(|&n| n > 0) {
            if let Some(v) = b.props.shift_packed(len) {
                store_len(&mut b, len - 1);
                return Ok(v);
            }
        }
    }
    let ov = Value::Obj(o.clone());
    let len = checked_len_of(i, &o)?;
    if len == 0 {
        set_len(i, &o, &ov, 0.0)?;
        return Ok(Value::Undefined);
    }
    let first = get_elem(i, &o, &ov, 0)?;
    for k in 1..len {
        match has_get(i, &o, &ov, k)? {
            Some(v) => set_elem(i, &o, &ov, k - 1, v)?,
            None => delete_elem(i, &o, &ov, k - 1)?,
        }
    }
    delete_elem(i, &o, &ov, len - 1)?;
    set_len(i, &o, &ov, (len - 1) as f64)?;
    Ok(first)
}

pub(super) fn array_unshift(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    // Dense fast path: the new indices can't reach a setter (no elements on the array
    // prototypes), so prepending into front slack is the whole effect.
    if !args.is_empty()
        && matches!(o.borrow().exotic, Exotic::Array)
        && o.borrow().extensible
        && i.ordinary_get_ptr(Gc::as_ptr(&o) as usize)
        && i.array_append_unshadowed(&o)
    {
        let mut b = o.borrow_mut();
        if let Some(len) = plain_len(&b) {
            let n = len as u64 + args.len() as u64;
            if n <= u32::MAX as u64 && b.props.unshift_packed(len, args) {
                store_len(&mut b, n as u32);
                return Ok(Value::Num(n as f64));
            }
        }
    }
    let ov = Value::Obj(o.clone());
    let len = len_of(i, &o)? as u64;
    let n = args.len() as u64;
    if n > 0 {
        if len + n > 9007199254740991 {
            return Err(i.make_error("TypeError", "unshift result is too long"));
        }
        for k in (0..len).rev() {
            let (from, to) = (k as usize, (k + n) as usize);
            match has_get(i, &o, &ov, from)? {
                Some(v) => set_elem(i, &o, &ov, to, v)?,
                None => delete_elem(i, &o, &ov, to)?,
            }
        }
        for (idx, a) in args.iter().enumerate() {
            set_elem(i, &o, &ov, idx, a.clone())?;
        }
    }
    set_len(i, &o, &ov, (len + n) as f64)?;
    Ok(Value::Num((len + n) as f64))
}

pub(super) fn array_reverse(i: &mut Interp, this: Value, _args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let ov = Value::Obj(o.clone());
    let len = len_of(i, &o)?;
    for lower in 0..len / 2 {
        let upper = len - 1 - lower;
        // HasProperty/Get the two ends, then swap — preserving holes (a hole moves as a
        // DeletePropertyOrThrow).
        let lower_val = has_get(i, &o, &ov, lower)?;
        let upper_val = has_get(i, &o, &ov, upper)?;
        match (lower_val, upper_val) {
            (Some(lv), Some(uv)) => {
                set_elem(i, &o, &ov, lower, uv)?;
                set_elem(i, &o, &ov, upper, lv)?;
            }
            (None, Some(uv)) => {
                set_elem(i, &o, &ov, lower, uv)?;
                delete_elem(i, &o, &ov, upper)?;
            }
            (Some(lv), None) => {
                delete_elem(i, &o, &ov, lower)?;
                set_elem(i, &o, &ov, upper, lv)?;
            }
            (None, None) => {}
        }
    }
    Ok(ov)
}

pub(super) fn array_fill(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = len_of(i, &o)? as i64;
    let v = arg(args, 0);
    let start = norm_index(ab(i.to_number(&arg(args, 1)))?, len);
    let end = match arg(args, 2) {
        Value::Undefined => len,
        x => norm_index(ab(i.to_number(&x))?, len),
    };
    // A real Array's filled span is bounded by the engine cap (it materializes one property
    // per index); a generic array-like iterates lazily — its accessors typically throw or
    // the per-op caps stop runaway growth.
    if matches!(o.borrow().exotic, Exotic::Array) && (end - start).max(0) as usize > MAX_ARRAY_OP_LEN
    {
        return Err(i.make_error("RangeError", "array length exceeds engine limit"));
    }
    let ov = Value::Obj(o.clone());
    for k in start..end {
        set_elem(i, &o, &ov, k as usize, v.clone())?;
    }
    Ok(ov)
}

pub(super) fn array_at(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = checked_len_of(i, &o)? as i64;
    let rel = ab(i.to_number(&arg(args, 0)))?;
    let rel = if rel.is_nan() { 0.0 } else { rel.trunc() };
    let idx = if rel < 0.0 { len as f64 + rel } else { rel };
    if idx < 0.0 || idx >= len as f64 {
        return Ok(Value::Undefined);
    }
    let ov = Value::Obj(o.clone());
    get_elem(i, &o, &ov, idx as usize)
}

pub(super) fn array_sort(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let cmp = arg(args, 0);
    if !matches!(cmp, Value::Undefined) && !cmp.is_callable() {
        return Err(i.make_error("TypeError", "the comparator must be a function or undefined"));
    }
    let o = arr_to_object(i, &this)?;
    let ov = Value::Obj(o.clone());
    let len = checked_len_of(i, &o)?;
    // SortIndexedProperties: read only the present indices (holes are skipped, not read).
    let mut items = Vec::with_capacity(len);
    for k in 0..len {
        if let Some(v) = has_get(i, &o, &ov, k)? {
            items.push(v);
        }
    }
    let item_count = items.len();
    sort_values(i, &mut items, &cmp)?;
    // Set(O, k, v, true): a failed write (non-writable element) always throws.
    for (k, v) in items.into_iter().enumerate() {
        set_elem(i, &o, &ov, k, v)?;
    }
    // Vacated trailing indices (originally holes, or beyond the present count) are deleted
    // in ascending order through [[Delete]] (a proxy's deleteProperty trap observes each).
    for k in item_count..len {
        delete_or_throw(i, &ov, &k.to_string())?;
    }
    Ok(ov)
}

/// The spec's code-unit order of two strings.
#[inline]
pub(super) fn cmp_str_units(a: &crate::lstr::LStr, b: &crate::lstr::LStr) -> Ordering {
    // UTF-8 byte order equals UTF-16 unit order unless a supplementary-plane character (or a
    // smuggled surrogate) meets U+E000..U+FFFF; all-ASCII strings can't contain either.
    if a.ascii_hint() && b.ascii_hint() {
        return a.as_bytes().cmp(b.as_bytes());
    }
    crate::jstr::cmp_units(a, b)
}

/// SortIndexedProperties' sort: stable, `undefined`s last, comparator `cmp` (or the default
/// string order). An abrupt comparator (or ToString) aborts the sort with that error; an
/// inconsistent comparator yields an implementation-defined order (never a panic).
pub(super) fn sort_values(i: &mut Interp, items: &mut Vec<Value>, cmp: &Value) -> Result<(), Value> {
    // `undefined` always sorts to the end, without being passed to the comparator.
    let undefs = items.iter().filter(|v| matches!(v, Value::Undefined)).count();
    if undefs > 0 {
        items.retain(|v| !matches!(v, Value::Undefined));
    }
    if cmp.is_callable() {
        let mut f = PreparedCall::new(i, cmp.clone(), Value::Undefined);
        let mut cmp_fn = |i: &mut Interp, a: &Value, b: &Value| -> Result<bool, Value> {
            // `a` goes after `b` (strictly greater).
            let r = ab(f.call(i, &mut [a.clone(), b.clone()]))?;
            let n = match r {
                Value::Num(n) => n,
                other => ab(i.to_number(&other))?,
            };
            Ok(n > 0.0)
        };
        merge_sort_by(i, items, &mut cmp_fn)?;
    } else if items
        .iter()
        .all(|v| matches!(v, Value::Str(_) | Value::Num(_) | Value::Bool(_) | Value::Null))
    {
        // ToString of these primitives is unobservable: compute each key once.
        let mut keyed: Vec<(crate::lstr::LStr, Value)> = Vec::with_capacity(items.len());
        for v in items.drain(..) {
            let k = match &v {
                Value::Str(s) => s.clone(),
                other => ab(i.to_string(other))?,
            };
            keyed.push((k, v));
        }
        // Total order on keys: std's stable sort is exact here.
        keyed.sort_by(|a, b| cmp_str_units(&a.0, &b.0));
        items.extend(keyed.into_iter().map(|(_, v)| v));
    } else {
        let mut cmp_fn = |i: &mut Interp, a: &Value, b: &Value| -> Result<bool, Value> {
            let sa = ab(i.to_string(a))?;
            let sb = ab(i.to_string(b))?;
            Ok(cmp_str_units(&sa, &sb) == Ordering::Greater)
        };
        merge_sort_by(i, items, &mut cmp_fn)?;
    }
    items.extend(std::iter::repeat_n(Value::Undefined, undefs));
    Ok(())
}

/// Stable merge sort driven by a fallible "a > b" predicate (JS comparators may be
/// inconsistent or throw, so std's sort — which may panic on a non-total order — is unusable).
fn merge_sort_by(
    i: &mut Interp,
    items: &mut [Value],
    gt: &mut dyn FnMut(&mut Interp, &Value, &Value) -> Result<bool, Value>,
) -> Result<(), Value> {
    let n = items.len();
    if n < 2 {
        return Ok(());
    }
    // Binary insertion sort of small runs (fewest comparisons for short runs).
    const RUN: usize = 8;
    let mut start = 0;
    while start < n {
        let end = (start + RUN).min(n);
        for j in start + 1..end {
            // Find the insertion point: after every element <= items[j] (stability).
            let (mut lo, mut hi) = (start, j);
            while lo < hi {
                let mid = (lo + hi) / 2;
                if gt(i, &items[mid], &items[j])? {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            items[lo..=j].rotate_right(1);
        }
        start = end;
    }
    let mut buf: Vec<Value> = Vec::with_capacity(n);
    let mut width = RUN;
    while width < n {
        let mut lo = 0;
        while lo + width < n {
            let mid = lo + width;
            let hi = (lo + 2 * width).min(n);
            // Already ordered across the seam: nothing to merge.
            if gt(i, &items[mid - 1], &items[mid])? {
                buf.clear();
                buf.extend_from_slice(&items[lo..mid]);
                let (mut a, mut b, mut k) = (0usize, mid, lo);
                let res = (|| -> Result<(), Value> {
                    while a < buf.len() && b < hi {
                        if gt(i, &buf[a], &items[b])? {
                            items[k] = items[b].clone();
                            b += 1;
                        } else {
                            items[k] = buf[a].clone();
                            a += 1;
                        }
                        k += 1;
                    }
                    Ok(())
                })();
                // Restore the remaining left run either way (on error the multiset stays
                // intact, as the spec's abrupt sort leaves the receiver untouched anyway).
                while a < buf.len() {
                    items[k] = buf[a].clone();
                    a += 1;
                    k += 1;
                }
                res?;
            }
            lo = hi;
        }
        width *= 2;
    }
    Ok(())
}

/// FlattenIntoArray over [`Out`]: write `source`'s (mapped, one-level-per-depth flattened)
/// elements starting at `start`. Returns the next target index.
#[allow(clippy::too_many_arguments)]
pub(super) fn flatten_into_out(
    i: &mut Interp,
    target: &mut Out,
    source: &Value,
    source_len: usize,
    start: usize,
    depth: i64,
    mut mapper: Option<&mut PreparedCall>,
) -> Result<usize, Value> {
    let mut target_index = start;
    let so = match source {
        Value::Obj(o) => o.clone(),
        _ => return Ok(target_index),
    };
    for k in 0..source_len {
        let Some(mut element) = has_get(i, &so, source, k)? else {
            continue; // FlattenIntoArray skips holes
        };
        if let Some(m) = mapper.as_deref_mut() {
            element = m.call3(i, element, Value::Num(k as f64), source)?;
        }
        if depth > 0 && json_is_array(i, &element)? {
            let el_len = match &element {
                Value::Obj(eo) if !matches!(proxy_pair(i, &element), Some(_)) => len_of(i, eo)?,
                _ => {
                    let len_val = ab(i.get_member(&element, "length"))?;
                    to_length_val(i, &len_val)?
                }
            };
            target_index = flatten_into_out(
                i,
                target,
                &element,
                el_len,
                target_index,
                depth - 1,
                None,
            )?;
        } else {
            // Compared as u64: usize is 32-bit on wasm32, where 2^53 - 1 overflows the type.
            if target_index as u64 >= 9_007_199_254_740_991 {
                return Err(i.make_error("TypeError", "flattened array length exceeds 2^53 - 1"));
            }
            target.put(i, target_index, element)?;
            target_index += 1;
        }
    }
    Ok(target_index)
}

pub(super) fn array_flat(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let source_len = len_of(i, &o)?;
    // ToIntegerOrInfinity(depth); undefined defaults to 1.
    let depth = match arg(args, 0) {
        Value::Undefined => 1i64,
        v => {
            let n = ab(i.to_number(&v))?;
            if n.is_nan() || n <= 0.0 {
                0
            } else if n == f64::INFINITY {
                i64::MAX
            } else {
                n as i64
            }
        }
    };
    let custom = species(i, &this, 0)?;
    let mut out = Out::new(custom, 0);
    let ov = Value::Obj(o.clone());
    flatten_into_out(i, &mut out, &ov, source_len, 0, depth, None)?;
    out.finish(i, None)
}

pub(super) fn array_flat_map(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    let o = arr_to_object(i, &this)?;
    let len = len_of(i, &o)?;
    let cb = arg(args, 0);
    if !cb.is_callable() {
        return Err(i.make_error("TypeError", "Array.prototype.flatMap mapper is not callable"));
    }
    let cb_this = arg(args, 1);
    let custom = species(i, &this, 0)?;
    let mut out = Out::new(custom, 0);
    let ov = Value::Obj(o.clone());
    let mut f = PreparedCall::new(i, cb, cb_this);
    flatten_into_out(i, &mut out, &ov, len, 0, 1, Some(&mut f))?;
    f.finish(i);
    out.finish(i, None)
}
