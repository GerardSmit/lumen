//! Protocol-free iteration of pristine Arrays, Maps and Sets for `for…of`, array
//! destructuring, spread arguments/elements and `Array.from`.
//!
//! **Protectors.** [`array_ok`] / [`coll_ok`] prove that GetIterator on an object would run no
//! user code and produce exactly the intrinsic iterator: the object is an ordinary Array (Map,
//! Set) of this realm with no own `@@iterator`, `%Array.prototype%[@@iterator]` is the intrinsic
//! `values` (`entries` / `values` for Map / Set), and the iterator prototype's `next` is the
//! intrinsic one. The proof is memoized per realm on the prototypes' slots, keyed by the
//! prototype epoch (every structural change to a marked prototype bumps it — both prototypes
//! are marked when the proof is built); the two values are re-read from their slots on every
//! check (an ordinary assignment `Array.prototype[Symbol.iterator] = f` is not structural), and
//! a subclass instance, an own `@@iterator` (a different shape) or a swapped prototype fails
//! the per-object part.
//!
//! **Encoded for-of state.** A `GetIter` whose operand passes the protector allocates no
//! iterator: the iterator slot holds `Num(index + kind * STRIDE)` and the `next` slot holds the
//! iterated object itself (a real iterator is always an object, so a Number in the iterator
//! slot is unambiguous). [`step`] then runs `%ArrayIteratorPrototype%.next`'s algorithm on the
//! virtual iterator — the `next` GetIterator captured is the intrinsic one, so later patches to
//! the prototype are correctly unobservable — re-reading `length` each step (mutation during the
//! loop is visible) and reading holes through the prototype chain. Map/Set states walk the live
//! backing list by position (appended entries are visited, tombstoned ones skipped). Anything
//! irregular (an accessor element, a hole under an indexed prototype property, a non-plain
//! array) *materializes* the state: a real iterator object with the same position is built, the
//! slots are rewritten to it and the captured intrinsic `next`, and the ordinary protocol takes
//! over. IteratorClose of an encoded state is a no-op while no `return` is reachable from the
//! iterator prototype; otherwise the state is materialized first so `return` sees a real
//! iterator object.
use std::cell::{Cell, RefCell};

use crate::builtins::collection_data::CollectionKind;
use crate::interpreter::{Abrupt, Interp};
use crate::value::{proto_epoch, Exotic, Gc, Value};

/// Iteration kinds (the `__ai_kind` / `__ci_kind` codes).
const VALUES: u8 = 0;
const ENTRIES: u8 = 2;
/// The kind's weight in an encoded state (indices stay far below it).
const STRIDE: f64 = 4_398_046_511_104.0; // 2^42
/// An exhausted encoded state (stays done even if the target grows).
const DONE: f64 = f64::INFINITY;

const MAP_ITER_FN: &str = "%MapIteratorFnIntrinsic%";
const SET_ITER_FN: &str = "%SetIteratorFnIntrinsic%";
const MAP_NEXT_FN: &str = "%MapIteratorNextIntrinsic%";
const SET_NEXT_FN: &str = "%SetIteratorNextIntrinsic%";

/// One memoized protector proof: `proto[slot]` holds `iter_fn` and `ip[ip_slot]` holds `next`.
struct Proof {
    epoch: u32,
    /// The realm prototype the proof is for (`Array.prototype`, `Map.prototype`, …).
    proto: Gc,
    /// The iterator prototype (`%ArrayIteratorPrototype%`, …).
    ip: Gc,
    proto_slot: usize,
    ip_slot: usize,
    iter_fn: Gc,
    next: Gc,
    /// An instance shape verified to have no own `@@iterator`.
    shape: Cell<u32>,
}

impl Proof {
    #[inline]
    fn holds(&self, proto: &Gc, shape: u32) -> bool {
        self.epoch == proto_epoch()
            && Gc::ptr_eq(&self.proto, proto)
            && self.shape.get() == shape
            && holds_at(&self.proto, self.proto_slot, &self.iter_fn)
            && holds_at(&self.ip, self.ip_slot, &self.next)
    }
}

/// Per-interpreter protector memos (see the module docs).
#[derive(Default)]
pub(crate) struct IterProof {
    arr: RefCell<Option<Proof>>,
    /// Map, Set.
    coll: [RefCell<Option<Proof>>; 2],
    /// `Array.prototype.push` (see [`array_push_ok`]).
    push: RefCell<Option<Proof>>,
}

/// Whether `o` is a data property slot `slot` of `holder` holding exactly `f`.
#[inline]
fn holds_at(holder: &Gc, slot: usize, f: &Gc) -> bool {
    // SAFETY: a pure read; nothing below can borrow mutably.
    let Ok(b) = (unsafe { holder.try_borrow_unguarded() }) else {
        return false;
    };
    b.ic_plain.get()
        && b.props.entry_at(slot).is_some_and(|p| p.holds_obj(f))
}

/// The protector for Arrays: iterating `o` is exactly the intrinsic Array Iterator, with no
/// user code to find or create it (see the module docs).
#[inline]
pub(crate) fn array_ok(i: &Interp, o: &Gc) -> bool {
    // SAFETY: a pure read; nothing below can borrow mutably.
    let shape = {
        let Ok(b) = (unsafe { o.try_borrow_unguarded() }) else {
            return false;
        };
        if !matches!(b.exotic, Exotic::Array)
            || !b.ic_plain.get()
            || !matches!(&b.proto, Some(p) if Gc::ptr_eq(p, &i.array_proto))
        {
            return false;
        }
        b.props.shape()
    };
    if let Some(p) = &*i.lang.iter_proof.arr.borrow() {
        if p.holds(&i.array_proto, shape) {
            return true;
        }
    }
    array_ok_slow(i, o, shape)
}

#[cold]
fn array_ok_slow(i: &Interp, o: &Gc, shape: u32) -> bool {
    if !i.pristine_array_iteration(o) {
        return false;
    }
    // Proven for this array; memoize the prototype half and this shape.
    let (Some(key), Some(aip), Some(next)) = (
        iter_key(i),
        i.extra_protos.get("%ArrayIteratorPrototype%").cloned(),
        i.array_iter_next_intrinsic(),
    ) else {
        return true;
    };
    let ap = i.array_proto.clone();
    let values = match ap.borrow().props.get(&key).map(|p| p.value()) {
        Some(Value::Obj(f)) => f,
        _ => return true,
    };
    remember(&i.lang.iter_proof.arr, ap, aip, &key, "next", values, next, shape);
    true
}

/// Whether `o.push` reads the intrinsic `Array.prototype.push`: `o` is a plain Array whose
/// prototype is `Array.prototype`, it has no own `push`, and `Array.prototype`'s own data
/// property `push` holds `push`. The proof is memoized like the iteration protectors (its
/// `ip`/`next` half repeats the `push` slot, since there is no second object to watch).
#[inline]
pub(crate) fn array_push_ok(i: &Interp, o: &Gc, push: crate::value::NativeFn) -> bool {
    // SAFETY: a pure read; nothing below can borrow mutably.
    let shape = {
        let Ok(b) = (unsafe { o.try_borrow_unguarded() }) else {
            return false;
        };
        if !matches!(b.exotic, Exotic::Array)
            || !b.ic_plain.get()
            || !matches!(&b.proto, Some(p) if Gc::ptr_eq(p, &i.array_proto))
        {
            return false;
        }
        b.props.shape()
    };
    if let Some(p) = &*i.lang.iter_proof.push.borrow() {
        if p.holds(&i.array_proto, shape) {
            return true;
        }
    }
    array_push_ok_slow(i, o, shape, push)
}

#[cold]
fn array_push_ok_slow(i: &Interp, o: &Gc, shape: u32, push: crate::value::NativeFn) -> bool {
    if o.borrow().props.slot_of("push").is_some() {
        return false;
    }
    let ap = i.array_proto.clone();
    let f = match ap.borrow().props.get("push") {
        Some(p) if !p.accessor() => match p.value() {
            Value::Obj(f) => f,
            _ => return false,
        },
        _ => return false,
    };
    let native = matches!(f.borrow().call, crate::value::Callable::Native(fp) if fp as usize == push as usize);
    if native {
        remember(&i.lang.iter_proof.push, ap.clone(), ap, "push", "push", f.clone(), f, shape);
    }
    native
}

fn iter_key(i: &Interp) -> Option<String> {
    i.iterator_sym.as_ref().map(|s| Interp::sym_key(s))
}

/// Mark both prototypes (so structural changes bump the epoch), then record the proof.
fn remember(
    cell: &RefCell<Option<Proof>>,
    proto: Gc,
    ip: Gc,
    key: &str,
    ip_key: &str,
    iter_fn: Gc,
    next: Gc,
    shape: u32,
) {
    proto.borrow().props.mark_proto();
    ip.borrow().props.mark_proto();
    let epoch = proto_epoch();
    if epoch == u32::MAX {
        return;
    }
    let (Some(proto_slot), Some(ip_slot)) =
        (proto.borrow().props.slot_of(key), ip.borrow().props.slot_of(ip_key))
    else {
        return;
    };
    let p = Proof { epoch, proto, ip, proto_slot, ip_slot, iter_fn, next, shape: Cell::new(shape) };
    if !(p.holds(&p.proto.clone(), shape)) {
        return;
    }
    *cell.borrow_mut() = Some(p);
}

/// The protector for Maps (`set == false`) and Sets: `o` is a genuine Map/Set of this realm on
/// the intrinsic prototype, whose `@@iterator` and iterator `next` are the intrinsic ones.
pub(crate) fn coll_ok(i: &Interp, o: &Gc) -> Option<CollectionKind> {
    let ptr = Gc::as_ptr(o) as usize;
    let kind = i.map_data.get(&ptr)?.kind();
    let (idx, proto_key) = match kind {
        CollectionKind::Map => (0, "Map"),
        CollectionKind::Set => (1, "Set"),
        _ => return None,
    };
    // SAFETY: a pure read; nothing below can borrow mutably.
    let (shape, proto) = {
        let b = unsafe { o.try_borrow_unguarded() }.ok()?;
        if !matches!(b.exotic, Exotic::None) || !b.ic_plain.get() {
            return None;
        }
        (b.props.shape(), b.proto.clone()?)
    };
    if let Some(p) = &*i.lang.iter_proof.coll[idx].borrow() {
        if Gc::ptr_eq(&p.proto, &proto) && p.holds(&proto, shape) {
            return Some(kind);
        }
    }
    coll_ok_slow(i, o, idx, proto_key, &proto, shape).then_some(kind)
}

#[cold]
fn coll_ok_slow(i: &Interp, o: &Gc, idx: usize, proto_key: &str, proto: &Gc, shape: u32) -> bool {
    let (ip_key, fn_key, next_key) = if idx == 0 {
        ("%MapIteratorPrototype%", MAP_ITER_FN, MAP_NEXT_FN)
    } else {
        ("%SetIteratorPrototype%", SET_ITER_FN, SET_NEXT_FN)
    };
    let (Some(realm_proto), Some(ip), Some(iter_fn), Some(next), Some(key)) = (
        i.extra_protos.get(proto_key),
        i.extra_protos.get(ip_key),
        i.extra_protos.get(fn_key),
        i.extra_protos.get(next_key),
        iter_key(i),
    ) else {
        return false;
    };
    if !Gc::ptr_eq(realm_proto, proto) {
        return false;
    }
    // No own `@@iterator` on the instance, and the intrinsic data properties in place.
    let plain = |g: &Gc| {
        let b = g.borrow();
        b.ic_plain.get() && matches!(b.exotic, Exotic::None)
    };
    if !plain(proto) || !plain(ip) || o.borrow().props.get(&key).is_some() {
        return false;
    }
    let data_is = |g: &Gc, k: &str, f: &Gc| {
        matches!(g.borrow().props.get(k), Some(p) if !p.accessor()
            && matches!(p.value(), Value::Obj(v) if Gc::ptr_eq(&v, f)))
    };
    if !data_is(proto, &key, iter_fn) || !data_is(ip, "next", next) {
        return false;
    }
    remember(
        &i.lang.iter_proof.coll[idx],
        proto.clone(),
        ip.clone(),
        &key,
        "next",
        iter_fn.clone(),
        next.clone(),
        shape,
    );
    true
}

/// Realm setup: remember a Map/Set `@@iterator` function (`set`) and iterator `next`.
pub(crate) fn remember_collection_intrinsics(i: &mut Interp, set: bool, iter_fn: &Value) {
    if let Value::Obj(f) = iter_fn {
        i.extra_protos.insert(if set { SET_ITER_FN } else { MAP_ITER_FN }, f.clone());
    }
}

/// Realm setup: remember the `next` of `%MapIteratorPrototype%` / `%SetIteratorPrototype%`.
pub(crate) fn remember_collection_next(i: &mut Interp, set: bool, proto: &Gc) {
    if let Some(Value::Obj(f)) = proto.borrow().props.get("next").map(|p| p.value()) {
        i.extra_protos.insert(if set { SET_NEXT_FN } else { MAP_NEXT_FN }, f);
    }
}

/// GetIterator for `for…of` / destructuring: the encoded initial state when `v` passes a
/// protector (see the module docs), to be stored in the iterator slot with `v` in the `next` slot.
#[inline]
pub(crate) fn open(i: &Interp, v: &Value) -> Option<f64> {
    if disabled() {
        return None;
    }
    let Value::Obj(o) = v else {
        return None;
    };
    if array_ok(i, o) {
        return Some(0.0);
    }
    match coll_ok(i, o)? {
        CollectionKind::Map => Some(f64::from(ENTRIES) * STRIDE),
        _ => Some(f64::from(VALUES) * STRIDE),
    }
}

/// GetIterator on a fresh Map/Set iterator (`m.keys()`, `m.values()`, `m.entries()`, `s.values()`
/// …) that nothing else references: the encoded state at the iterator's position plus the
/// iterated collection (for the `next` slot). The iterator object is consumed unobservably —
/// the operand holds its only reference — so the loop may run on the encoded state instead,
/// provided GetIterator would run no user code: the object carries only its three internal
/// slots, its prototype is the realm's Map/Set iterator prototype with the intrinsic `next`,
/// and `@@iterator` resolves to `%IteratorPrototype%`'s intrinsic (returns `this`).
pub(crate) fn open_coll_iter(i: &Interp, v: &Value) -> Option<(f64, Value)> {
    if disabled() {
        return None;
    }
    let Value::Obj(o) = v else {
        return None;
    };
    if Gc::strong_count(o) != 1 {
        return None;
    }
    let b = o.try_borrow().ok()?;
    if !matches!(b.exotic, Exotic::None) || !b.ic_plain.get() {
        return None;
    }
    let coll = match b.props.get("__ci_coll")?.value() {
        Value::Obj(c) => c,
        _ => return None,
    };
    let idx = match b.props.get("__ci_index")?.value() {
        Value::Num(n) if n >= 0.0 && n < STRIDE => n,
        _ => return None,
    };
    let kind = match b.props.get("__ci_kind")?.value() {
        Value::Num(k) if k == 0.0 || k == 1.0 || k == 2.0 => k as u8,
        _ => return None,
    };
    if b.props.get("__ci_done").is_some() || b.props.keys().len() != 3 {
        return None;
    }
    let ip_key = match i.map_data.get(&(Gc::as_ptr(&coll) as usize))?.kind() {
        CollectionKind::Map => "%MapIteratorPrototype%",
        CollectionKind::Set => "%SetIteratorPrototype%",
        _ => return None,
    };
    let proto = b.proto.as_ref()?;
    if !Gc::ptr_eq(proto, i.extra_protos.get(ip_key)?) {
        return None;
    }
    let key = iter_key(i)?;
    let native_is = |p: Option<&crate::value::Property>, f: crate::value::NativeFn| -> bool {
        let Some(p) = p else { return false };
        if p.accessor() {
            return false;
        }
        match p.value() {
            Value::Obj(g) => matches!(
                g.try_borrow().map(|gb| match gb.call {
                    crate::value::Callable::Native(h) => h as usize == f as usize,
                    _ => false,
                }),
                Ok(true)
            ),
            _ => false,
        }
    };
    let pb = proto.try_borrow().ok()?;
    if !matches!(pb.exotic, Exotic::None)
        || !pb.ic_plain.get()
        || pb.props.get(&key).is_some()
        || !native_is(
            pb.props.get("next"),
            crate::builtins::collections::map_set_iter_next,
        )
    {
        return None;
    }
    let root = pb.proto.as_ref()?;
    if !Gc::ptr_eq(root, i.extra_protos.get("%IteratorPrototype%")?) {
        return None;
    }
    let rb = root.try_borrow().ok()?;
    if !matches!(rb.exotic, Exotic::None)
        || !rb.ic_plain.get()
        || !native_is(rb.props.get(&key), crate::builtins::return_this)
    {
        return None;
    }
    Some((idx + f64::from(kind) * STRIDE, Value::Obj(coll)))
}

fn disabled() -> bool {
    thread_local! {
        static OFF: bool = std::env::var_os("LUMEN_NO_ITER_FAST").is_some();
    }
    OFF.with(|o| *o)
}

#[inline]
fn decode(f: f64) -> (f64, u8) {
    if f < STRIDE {
        (f, VALUES)
    } else if f < 2.0 * STRIDE {
        (f - STRIDE, 1)
    } else {
        (f - 2.0 * STRIDE, ENTRIES)
    }
}

/// One step of the state in the iterator/next slots: `Some(Some(v))` yielded `v`, `Some(None)`
/// = done, `None` = not an encoded state — or one that just materialized — so the caller runs
/// the ordinary protocol on the (possibly rewritten) slots.
#[inline]
pub(crate) fn step(i: &Interp, it: &mut Value, nx: &mut Value) -> Option<Option<Value>> {
    let Value::Num(f) = *it else {
        return None;
    };
    if f == DONE {
        return Some(None);
    }
    let (idx, kind) = decode(f);
    let r = match nx {
        Value::Obj(target) => step_encoded(i, target, idx, kind),
        _ => None,
    };
    match r {
        Some(Some((v, next_idx))) => {
            *it = Value::Num(next_idx + f64::from(kind) * STRIDE);
            Some(Some(v))
        }
        Some(None) => {
            *it = Value::Num(DONE);
            Some(None)
        }
        None => {
            materialize(i, it, nx);
            None
        }
    }
}

/// `Some(Some((value, next index)))`, `Some(None)` = exhausted, `None` = irregular (materialize).
#[inline]
fn step_encoded(i: &Interp, target: &Gc, idx: f64, kind: u8) -> Option<Option<(Value, f64)>> {
    // SAFETY: a pure read; `array_append_unshadowed` below only takes shared borrows.
    let b = unsafe { target.try_borrow_unguarded() }.ok()?;
    if matches!(b.exotic, Exotic::Array) {
        if !b.ic_plain.get() {
            return None;
        }
        let len = match b.props.length_property() {
            Some(p) if !p.accessor() => match p.value() {
                Value::Num(n) => n,
                _ => return None,
            },
            _ => return None,
        };
        if idx >= len {
            return Some(None);
        }
        let k = idx as u32;
        let v = match b.props.get_index(k) {
            Some(p) if !p.accessor() => p.value(),
            Some(_) => return None,
            None => {
                // A hole (or a map-only element): an own data property is read as such; an
                // absent one is `undefined` while the prototypes hold no indexed properties.
                match b.props.get(&k.to_string()) {
                    Some(p) if !p.accessor() => p.value(),
                    Some(_) => return None,
                    None if i.array_append_unshadowed(target) => Value::Undefined,
                    None => return None,
                }
            }
        };
        let v = match kind {
            VALUES => v,
            1 => Value::Num(idx),
            _ => i.make_array(vec![Value::Num(idx), v]),
        };
        #[cfg(test)]
        super::array_iterator_step::note_success();
        return Some(Some((v, idx + 1.0)));
    }
    let data = i.map_data.get(&(Gc::as_ptr(target) as usize))?;
    let mut cur = idx as usize;
    match data.next(&mut cur) {
        Some((k, v)) => {
            let v = match kind {
                VALUES => v.clone(),
                1 => k.clone(),
                _ => i.make_array(vec![k.clone(), v.clone()]),
            };
            Some(Some((v, cur as f64)))
        }
        None => Some(None),
    }
}

/// Rewrite an encoded state into a real iterator object at the same position plus the
/// intrinsic `next` GetIterator captured. A no-op for a non-encoded state.
#[cold]
pub(crate) fn materialize(i: &Interp, it: &mut Value, nx: &mut Value) {
    let Value::Num(f) = *it else {
        return;
    };
    let Value::Obj(target) = nx.clone() else {
        return;
    };
    let is_array = matches!(target.borrow().exotic, Exotic::Array);
    if is_array {
        let (idx, kind) = if f == DONE { (0.0, VALUES) } else { decode(f) };
        let obj = crate::builtins::make_array_iterator_pub(
            i,
            if f == DONE { Value::Undefined } else { Value::Obj(target.clone()) },
            kind,
        );
        if let Value::Obj(o) = &obj {
            if let Some(p) = o.borrow_mut().props.get_mut("__ai_index") {
                p.set_value(Value::Num(idx));
            }
        }
        let next = i.array_iter_next_intrinsic();
        *it = obj;
        *nx = next.map_or(Value::Undefined, Value::Obj);
        return;
    }
    let set = i
        .map_data
        .get(&(Gc::as_ptr(&target) as usize))
        .is_some_and(|d| d.kind() == CollectionKind::Set);
    let (idx, kind) = if f == DONE { (0.0, VALUES) } else { decode(f) };
    let obj = crate::builtins::make_collection_iterator(
        i,
        Value::Obj(target.clone()),
        kind,
        idx,
        f == DONE,
    );
    let next = i.extra_protos.get(if set { SET_NEXT_FN } else { MAP_NEXT_FN }).cloned();
    *it = obj;
    *nx = next.map_or(Value::Undefined, Value::Obj);
}

/// Materialize the state in `slots[it]` / `slots[it + 1]` (see [`materialize`]).
pub(crate) fn materialize_slots(i: &Interp, slots: &mut [Value], it: u16) {
    let it = it as usize;
    if it + 1 < slots.len() && matches!(slots[it], Value::Num(_)) {
        let (a, b) = slots.split_at_mut(it + 1);
        materialize(i, &mut a[it], &mut b[0]);
    }
}

/// [`step`] on `slots[is]` / `slots[ns]`.
#[inline]
pub(crate) fn step_slots(i: &Interp, slots: &mut [Value], is: u16, ns: u16) -> Option<Option<Value>> {
    let (is, ns) = (is as usize, ns as usize);
    if !matches!(slots.get(is), Some(Value::Num(_))) || is == ns || ns >= slots.len() {
        return None;
    }
    let (it, nx) = if is < ns {
        let (a, b) = slots.split_at_mut(ns);
        (&mut a[is], &mut b[0])
    } else {
        let (a, b) = slots.split_at_mut(is);
        (&mut b[0], &mut a[ns])
    };
    step(i, it, nx)
}

/// IteratorClose on the (iterator, next) pair in `slots[s]` / `slots[s + 1]` (see
/// [`close_pair_is_noop`]).
pub(crate) fn close_is_noop(i: &Interp, slots: &mut [Value], s: u16) -> bool {
    let s = s as usize;
    if s + 1 >= slots.len() || !matches!(slots[s], Value::Num(_)) {
        return false;
    }
    let (a, b) = slots.split_at_mut(s + 1);
    close_pair_is_noop(i, &mut a[s], &mut b[0])
}

/// IteratorClose on an (iterator, next) pair: `true` when it is an encoded state whose close is
/// a no-op (no `return` reachable, or already done); otherwise an encoded state is materialized
/// (so the caller's ordinary close sees a real iterator) and `false` is returned.
pub(crate) fn close_pair_is_noop(i: &Interp, it: &mut Value, nx: &mut Value) -> bool {
    let Value::Num(f) = *it else {
        return false;
    };
    if f == DONE {
        return true;
    }
    let absent = match &*nx {
        Value::Obj(t) if matches!(t.borrow().exotic, Exotic::Array) => i.array_iter_return_absent(),
        Value::Obj(t) => {
            let set = i
                .map_data
                .get(&(Gc::as_ptr(t) as usize))
                .is_some_and(|d| d.kind() == CollectionKind::Set);
            let ip = i.extra_protos.get(if set {
                "%SetIteratorPrototype%"
            } else {
                "%MapIteratorPrototype%"
            });
            ip.is_some_and(return_absent_from)
        }
        _ => false,
    };
    if absent {
        return true;
    }
    materialize(i, it, nx);
    false
}

/// No `return` (own or inherited, data or accessor) along `proto`'s chain, and nothing exotic
/// that could intercept the lookup.
fn return_absent_from(proto: &Gc) -> bool {
    let mut cur = Some(proto.clone());
    let mut depth = 0;
    while let Some(o) = cur {
        if depth > 8 {
            return false;
        }
        let b = o.borrow();
        if !b.ic_plain.get() || !matches!(b.exotic, Exotic::None) || b.props.get("return").is_some() {
            return false;
        }
        cur = b.proto.clone();
        depth += 1;
    }
    true
}

/// Every value the (iterator, next) pair yields from its current position (IterRestL): an
/// encoded state runs natively until it would materialize, then the ordinary protocol.
pub(crate) fn drain(i: &mut Interp, it: &Value, nx: &Value) -> Result<Vec<Value>, Abrupt> {
    let (mut it, mut nx) = (it.clone(), nx.clone());
    let mut out = Vec::new();
    loop {
        match step(i, &mut it, &mut nx) {
            Some(Some(v)) => out.push(v),
            Some(None) => return Ok(out),
            None => break,
        }
    }
    while let Some(v) = i.iterator_step(&it, &nx)? {
        out.push(v);
    }
    Ok(out)
}

/// The values iterating the pristine Array `v` yields (see [`array_ok`]), read without any
/// user code: `None` when `v` fails the protector or an element can't be read natively (an
/// accessor, or a hole under indexed prototype properties).
pub(crate) fn array_values(i: &Interp, v: &Value) -> Option<Vec<Value>> {
    if let Some((o, len)) = pristine_len(i, v) {
        // SAFETY: a pure read.
        let b = unsafe { o.try_borrow_unguarded() }.ok()?;
        let mut out = Vec::with_capacity(len as usize);
        if b.props.copy_dense_run(0, len, &mut out) == len {
            return Some(out);
        }
    }
    let mut out = Vec::new();
    array_elems(i, v, |x| out.push(x)).then_some(out)
}

/// A pristine Array (see [`array_ok`]) and its `length`.
#[inline]
fn pristine_len<'a>(i: &Interp, v: &'a Value) -> Option<(&'a Gc, u32)> {
    if disabled() {
        return None;
    }
    let Value::Obj(o) = v else {
        return None;
    };
    if !array_ok(i, o) {
        return None;
    }
    // SAFETY: a pure read.
    let b = unsafe { o.try_borrow_unguarded() }.ok()?;
    match b.props.length_property() {
        Some(p) if !p.accessor() => match p.value() {
            Value::Num(n) if (0.0..=67_108_864.0).contains(&n) => Some((o, n as u32)),
            _ => None,
        },
        _ => None,
    }
}

/// [`array_elems`] for a pristine Array of at most `max` packed plain data elements (no holes,
/// no accessors); `false` = nothing was pushed.
#[inline]
pub(crate) fn array_packed(i: &Interp, v: &Value, max: usize, mut push: impl FnMut(Value)) -> bool {
    let Some((o, len)) = pristine_len(i, v) else {
        return false;
    };
    if len as usize > max {
        return false;
    }
    // SAFETY: a pure read.
    let Ok(b) = (unsafe { o.try_borrow_unguarded() }) else {
        return false;
    };
    match b.props.packed_elements().and_then(|e| e.get(..len as usize)) {
        Some(run) if run.iter().all(|p| p.is_plain_element()) => {
            for p in run {
                push(p.value());
            }
            true
        }
        _ => false,
    }
}

/// [`array_values`] into `push`: all-or-nothing (`false` = nothing was pushed).
pub(crate) fn array_elems(i: &Interp, v: &Value, mut push: impl FnMut(Value)) -> bool {
    let Some((o, len)) = pristine_len(i, v) else {
        return false;
    };
    // SAFETY: a pure read; `array_append_unshadowed` only takes shared borrows.
    let Ok(b) = (unsafe { o.try_borrow_unguarded() }) else {
        return false;
    };
    // Pass 1: every element readable without user code.
    let mut holes = false;
    for k in 0..len {
        match b.props.get_index(k) {
            Some(p) if !p.accessor() => {}
            Some(_) => return false,
            None => match b.props.get(&k.to_string()) {
                Some(p) if !p.accessor() => {}
                Some(_) => return false,
                None => holes = true,
            },
        }
    }
    if holes && !i.array_append_unshadowed(o) {
        return false;
    }
    for k in 0..len {
        push(match b.props.get_index(k) {
            Some(p) => p.value(),
            None => b.props.get(&k.to_string()).map_or(Value::Undefined, |p| p.value()),
        });
    }
    true
}

/// The values iterating a pristine Map (its `[key, value]` entries) or Set yields, read from
/// the backing list without user code (see [`coll_ok`]).
pub(crate) fn coll_values(i: &Interp, v: &Value) -> Option<Vec<Value>> {
    if disabled() {
        return None;
    }
    let Value::Obj(o) = v else {
        return None;
    };
    let kind = coll_ok(i, o)?;
    let data = i.map_data.get(&(Gc::as_ptr(o) as usize))?;
    let mut out = Vec::with_capacity(data.len());
    for (k, v) in data.iter() {
        out.push(match kind {
            CollectionKind::Map => i.make_array(vec![k.clone(), v.clone()]),
            _ => v.clone(),
        });
    }
    Some(out)
}

/// [`array_values`], else [`coll_values`].
pub(crate) fn values(i: &Interp, v: &Value) -> Option<Vec<Value>> {
    array_values(i, v).or_else(|| coll_values(i, v))
}
