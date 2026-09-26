//! Guarded fast paths for hot language operations: pristine Array iteration (for-of, array
//! destructuring, spread), for-in key snapshots, and object spread.
//!
//! Every guard is built on [`SlotCache`]: a shape id pins its object's *named* key list (shared
//! shapes are immutable; an owned shape is re-id'd on every key-list change), so "key `k` lives in
//! slot `s`" — or "is absent" — holds for every object of that shape. Values and attributes are
//! not part of a shape, so each use re-reads the slot and re-checks identity/attributes.
use crate::fasthash::FastMap;
use crate::interpreter::Interp;
use crate::lstr::LStr;
use crate::value::{Exotic, Gc, Object, Property, Props, Value};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// No shape recorded yet (never issued: owned ids count down from `u32::MAX - 1`).
const NO_SHAPE: u32 = u32::MAX;
const ABSENT: u32 = u32::MAX;

/// The array-destructuring scalar replacement's remembered intrinsics (see
/// `bytecode::array_destructure`), shared here: `Array.prototype.values` and
/// `%ArrayIteratorPrototype%.next` as the realm created them.
const VALUES: &str = "%DestructureArrayValuesIntrinsic%";
const NEXT: &str = "%DestructureArrayNextIntrinsic%";
const AIP: &str = "%ArrayIteratorPrototype%";

/// A one-entry `(shape → slot | absent)` memo for one named (non-index) key.
pub(crate) struct SlotCache {
    shape: Cell<u32>,
    slot: Cell<u32>,
}

impl Default for SlotCache {
    fn default() -> SlotCache {
        SlotCache {
            shape: Cell::new(NO_SHAPE),
            slot: Cell::new(ABSENT),
        }
    }
}

impl SlotCache {
    /// The own property `key` (a named, non-index key) of `props`, or `None` when absent.
    #[inline]
    pub(crate) fn get<'a>(&self, props: &'a Props, key: &str) -> Option<&'a Property> {
        let sh = props.shape();
        if self.shape.get() == sh {
            let s = self.slot.get();
            return if s == ABSENT {
                None
            } else {
                props.entry_at(s as usize)
            };
        }
        let slot = props.slot_of(key);
        self.shape.set(sh);
        self.slot.set(slot.map_or(ABSENT, |s| s as u32));
        slot.and_then(|s| props.entry_at(s))
    }

    /// The slot of the own property `key` (a named, non-index key) of `props`.
    #[inline]
    pub(crate) fn slot(&self, props: &Props, key: &str) -> Option<usize> {
        let sh = props.shape();
        if self.shape.get() != sh {
            self.shape.set(sh);
            self.slot
                .set(props.slot_of(key).map_or(ABSENT, |s| s as u32));
        }
        let s = self.slot.get();
        (s != ABSENT).then_some(s as usize)
    }
}

/// Whether `p` is a data property holding exactly the object at address `expected`.
#[inline]
fn holds(p: Option<&Property>, expected: usize) -> bool {
    match p {
        Some(p) if !p.accessor() => {
            matches!(p.value(), Value::Obj(f) if Gc::as_ptr(&f) as usize == expected)
        }
        _ => false,
    }
}

/// The realm's iteration intrinsics, memoized by its `Array.prototype` (the held clones keep
/// every compared address alive, so a pointer can't be reused while cached).
struct Intrinsics {
    array_proto: Gc,
    values: Gc,
    next: Gc,
    aip: Gc,
    /// `%ArrayIteratorStepIntrinsic%` is installed (the bytecode step elision is enabled).
    step_enabled: bool,
}

/// Per-interpreter memo state for the fast paths (see the module docs).
#[derive(Default)]
pub(crate) struct LangCaches {
    /// The for-of / spread / destructuring protectors (see `bytecode::iter_fast`).
    pub(crate) iter_proof: crate::bytecode::iter_fast::IterProof,
    /// See [`Intrinsics`]; `extra_protos` lookups hash their string keys.
    intrinsics: RefCell<Option<Intrinsics>>,
    /// `(SymbolData ptr, property key)` of the realm's `Symbol.iterator`.
    iter_key: RefCell<Option<(usize, Rc<str>)>>,
    /// `@@iterator` on array instances (expected absent).
    own_iter: SlotCache,
    /// `@@iterator` on `Array.prototype`.
    proto_iter: SlotCache,
    /// `next` on `%ArrayIteratorPrototype%`.
    aip_next: SlotCache,
    /// `return` along `%ArrayIteratorPrototype%`'s prototype chain, one memo per level.
    ret: [SlotCache; 4],
    /// `(proto epoch, Array.prototype address)` at which [`Interp::array_iter_return_absent`]
    /// last held; the chain's objects were marked as prototypes then, so any structural change
    /// on them (a new `return`), a prototype swap or a `defineProperty` moves the epoch.
    ret_absent: std::cell::Cell<(u32, usize)>,
    /// A fresh Array Iterator's property map (`__ai_target`/`__ai_index`/`__ai_kind`), and the
    /// slots of those three keys under its shape.
    iter_template: RefCell<Option<(Props, [u32; 3])>>,
    /// `__ai_target` / `__ai_index` / `__ai_kind` on a stepped iterator (any shape).
    ai_slots: [SlotCache; 3],
    /// For-in own-key lists per shape: `(slot, key)` of every string-keyed named property.
    for_in: RefCell<FastMap<u32, Rc<[(u32, Value)]>>>,
    /// A fresh RegExp object's property map (own `lastIndex` = 0).
    regexp_template: RefCell<Option<Props>>,
    /// Compiled programs by literal site: keyed by the chunk's source/flags name pointers
    /// (pinned by the stored clones, so a pointer can't be reused while cached).
    regexp_sites: RefCell<FastMap<(usize, usize), (Rc<str>, Rc<str>, Rc<crate::regex::Regex>)>>,
    /// Weak handles of RegExp objects with a `regexps` entry (see [`Interp::regexp_weak_pin`]),
    /// and the length at which the dead ones are pruned next.
    regexp_weak: RefCell<Vec<crate::value::WeakGc>>,
    regexp_weak_next: std::cell::Cell<usize>,
    /// Promise side-table ownership and resolving-function parts (see `promise_fast`).
    pub(crate) promise: super::promise_fast::PromiseCaches,
    /// Inlined array callback guard memos (see `bytecode::inline_callback`).
    pub(crate) array_cb: crate::bytecode::inline_callback::CbCache,
}


impl Interp {
    /// The property key of the realm's `Symbol.iterator`.
    fn iter_sym_key(&self) -> Option<Rc<str>> {
        let sym = self.iterator_sym.as_ref()?;
        let ptr = Rc::as_ptr(sym) as usize;
        let mut c = self.lang.iter_key.borrow_mut();
        if let Some((p, k)) = &*c {
            if *p == ptr {
                return Some(k.clone());
            }
        }
        let k: Rc<str> = Interp::sym_key(sym).into();
        *c = Some((ptr, k.clone()));
        Some(k)
    }

    /// `(values, next, aip)` addresses of the current realm's iteration intrinsics.
    #[inline]
    fn iter_intrinsics(&self) -> Option<(usize, usize, *const Gc)> {
        let addr = |g: &Gc| Gc::as_ptr(g) as usize;
        {
            let c = self.lang.intrinsics.borrow();
            if let Some(c) = &*c {
                if Gc::ptr_eq(&c.array_proto, &self.array_proto) {
                    return Some((addr(&c.values), addr(&c.next), &c.aip as *const Gc));
                }
            }
        }
        let values = self.extra_protos.get(VALUES)?.clone();
        let next = self.extra_protos.get(NEXT)?.clone();
        let aip = self.extra_protos.get(AIP)?.clone();
        let mut c = self.lang.intrinsics.borrow_mut();
        *c = Some(Intrinsics {
            array_proto: self.array_proto.clone(),
            values,
            next,
            aip,
            step_enabled: self.extra_protos.contains_key("%ArrayIteratorStepIntrinsic%"),
        });
        let c = c.as_ref()?;
        Some((addr(&c.values), addr(&c.next), &c.aip as *const Gc))
    }

    /// Whether iterating `arr` (GetIterator + IteratorStep) runs no user code and is exactly the
    /// intrinsic Array Iterator over it: `arr` is an ordinary Array of this realm without an own
    /// `@@iterator`, `Array.prototype[@@iterator]` is the intrinsic `values`, and
    /// `%ArrayIteratorPrototype%.next` is the intrinsic `next` (a fresh iterator has no own
    /// `next`). Pure: reads only, never calls out.
    pub(crate) fn pristine_array_iteration(&self, arr: &Gc) -> bool {
        self.pristine_iteration(arr, false)
    }

    /// [`Interp::pristine_array_iteration`] for a lazy split view (see `crate::split_view`):
    /// iterating it yields exactly its pieces, in order, running no user code.
    pub(crate) fn pristine_view_iteration(&self, arr: &Gc) -> bool {
        self.pristine_iteration(arr, true)
    }

    fn pristine_iteration(&self, arr: &Gc, view: bool) -> bool {
        let Some((values, next, aip)) = self.iter_intrinsics() else {
            return false;
        };
        let Some(key) = self.iter_sym_key() else {
            return false;
        };
        // SAFETY (all unguarded borrows here): pure reads; nothing below can borrow mutably.
        let b = match unsafe { arr.try_borrow_unguarded() } {
            Ok(b) => b,
            Err(_) => return false,
        };
        let kind_ok = if view {
            b.exotic == Exotic::SplitView
        } else {
            b.ic_plain.get() && matches!(b.exotic, Exotic::Array)
        };
        if !kind_ok
            || !matches!(&b.proto, Some(p) if Gc::ptr_eq(p, &self.array_proto))
            || self.lang.own_iter.get(&b.props, &key).is_some()
        {
            return false;
        }
        let Ok(pb) = (unsafe { self.array_proto.try_borrow_unguarded() }) else {
            return false;
        };
        if !pb.ic_plain.get() || !holds(self.lang.proto_iter.get(&pb.props, &key), values) {
            return false;
        }
        // SAFETY: `aip` points into the memo, which is not replaced during this call.
        let Ok(ab) = (unsafe { (*aip).try_borrow_unguarded() }) else {
            return false;
        };
        ab.ic_plain.get()
            && matches!(ab.exotic, Exotic::None)
            && holds(self.lang.aip_next.get(&ab.props, "next"), next)
    }

    /// Whether an Array Iterator fresh from [`Interp::new_array_iterator`] (no own `return`)
    /// has no `return` method anywhere on its prototype chain — IteratorClose is then a no-op.
    pub(crate) fn array_iter_return_absent(&self) -> bool {
        let epoch = crate::value::proto_epoch();
        // The realm (`iter_intrinsics` keys its memo by `Array.prototype` too).
        let realm = Gc::as_ptr(&self.array_proto) as usize;
        if epoch != u32::MAX && self.lang.ret_absent.get() == (epoch, realm) {
            return true;
        }
        let Some((_, _, aip)) = self.iter_intrinsics() else {
            return false;
        };
        // SAFETY: pure reads through unguarded borrows; `aip` points into the memo.
        let mut cur: Option<&Gc> = Some(unsafe { &*aip });
        let mut depth = 0;
        while let Some(o) = cur {
            if depth >= self.lang.ret.len() {
                return false;
            }
            let Ok(b) = (unsafe { o.try_borrow_unguarded() }) else {
                return false;
            };
            if !b.ic_plain.get() || !matches!(b.exotic, Exotic::None) {
                return false;
            }
            if self.lang.ret[depth].get(&b.props, "return").is_some() {
                return false;
            }
            b.props.mark_proto();
            cur = b.proto.as_ref();
            depth += 1;
        }
        self.lang.ret_absent.set((epoch, realm));
        true
    }

    /// The intrinsic `%ArrayIteratorPrototype%.next`, when remembered.
    pub(crate) fn array_iter_next_intrinsic(&self) -> Option<Gc> {
        self.iter_intrinsics()?;
        self.lang.intrinsics.borrow().as_ref().map(|c| c.next.clone())
    }

    /// Whether `f` is the intrinsic `next` and the bytecode step elision is enabled.
    #[inline]
    pub(crate) fn is_elidable_array_iter_next(&self, f: &Gc) -> bool {
        self.is_array_iter_next(f)
            && self.lang.intrinsics.borrow().as_ref().is_some_and(|c| c.step_enabled)
    }

    /// Whether `f` is the intrinsic `%ArrayIteratorPrototype%.next`.
    #[inline]
    pub(crate) fn is_array_iter_next(&self, f: &Gc) -> bool {
        self.iter_intrinsics()
            .is_some_and(|(_, next, _)| Gc::as_ptr(f) as usize == next)
    }

    /// `CreateArrayIterator(arr, value)` for a [`pristine_array_iteration`] array: the same
    /// object `Array.prototype.values` builds, stamped from a cached property map instead of three
    /// shape-transitioning inserts.
    ///
    /// [`pristine_array_iteration`]: Interp::pristine_array_iteration
    pub(crate) fn new_array_iterator(&self, arr: &Value) -> Value {
        let aip = self.iter_intrinsics().map(|(_, _, aip)| {
            // SAFETY: `aip` points into the memo, which nothing below replaces.
            unsafe { (*aip).clone() }
        });
        {
            let t = self.lang.iter_template.borrow();
            if let Some((props, slots)) = &*t {
                let mut props = props.clone();
                if let Some(p) = props.entry_at_mut(slots[0] as usize) {
                    p.set_value(arr.clone());
                }
                return Value::Obj(Object::new_with_parts(aip, props, Exotic::None));
            }
        }
        let obj = crate::builtins::make_array_iterator_pub(self, arr.clone(), 0);
        self.remember_iter_template(&obj);
        obj
    }

    /// Record `obj` (a fresh `values` Array Iterator) as the template for
    /// [`Interp::new_array_iterator`].
    fn remember_iter_template(&self, obj: &Value) {
        let Value::Obj(o) = obj else {
            return;
        };
        let b = o.borrow();
        let slot = |k| b.props.slot_of(k).map(|s| s as u32);
        if let (Some(t), Some(i), Some(k)) =
            (slot("__ai_target"), slot("__ai_index"), slot("__ai_kind"))
        {
            let mut props = b.props.clone();
            if let Some(p) = props.entry_at_mut(t as usize) {
                p.set_value(Value::Undefined);
            }
            *self.lang.iter_template.borrow_mut() = Some((props, [t, i, k]));
        }
    }

    /// One step of the intrinsic `next` on an ordinary object whose own data `__ai_*` state says
    /// `values` kind over an ordinary Array (the state `next` itself reads), with no user code: `Some(Some(v))` yielded `v`,
    /// `Some(None)` = done (the iterator is now exhausted), `None` = not applicable and nothing
    /// was touched (the caller runs the protocol). A hole, an accessor element, or any other
    /// irregularity is "not applicable".
    pub(crate) fn array_iter_step_fast(&self, iter: &Value) -> Option<Option<Value>> {
        let Value::Obj(it) = iter else {
            return None;
        };
        let mut b = it.try_borrow_mut().ok()?;
        if !b.ic_plain.get() || !matches!(b.exotic, Exotic::None) {
            return None;
        }
        let [tc, ic, kc] = &self.lang.ai_slots;
        let ts = tc.slot(&b.props, "__ai_target")?;
        let is = ic.slot(&b.props, "__ai_index")?;
        let ks = kc.slot(&b.props, "__ai_kind")?;
        let kind = b.props.entry_at(ks)?;
        if kind.accessor() || !matches!(kind.value(), Value::Num(k) if k == 0.0) {
            return None;
        }
        let tp = b.props.entry_at(ts)?;
        if tp.accessor() {
            return None;
        }
        let target = match tp.value() {
            Value::Undefined => return Some(None),
            Value::Obj(o) => o,
            _ => return None,
        };
        let ip = b.props.entry_at(is)?;
        if ip.accessor() || !ip.writable() {
            return None;
        }
        let Value::Num(index) = ip.value() else {
            return None;
        };
        if !(0.0..4294967295.0).contains(&index) || index.fract() != 0.0 {
            return None;
        }
        let idx = index as u32;
        let step = {
            let tb = target.try_borrow().ok()?;
            if !tb.ic_plain.get() || !matches!(tb.exotic, Exotic::Array) {
                return None;
            }
            let len = tb.props.length_property()?;
            if len.accessor() {
                return None;
            }
            let Value::Num(len) = len.value() else {
                return None;
            };
            if f64::from(idx) >= len {
                None
            } else {
                let e = tb.props.get_index(idx)?;
                if e.accessor() {
                    return None;
                }
                let v = e.value();
                if matches!(v, Value::Empty) {
                    return None;
                }
                Some(v)
            }
        };
        match step {
            Some(v) => {
                b.props
                    .entry_at_mut(is)?
                    .set_value(Value::Num(f64::from(idx) + 1.0));
                Some(Some(v))
            }
            None => {
                // Exhausted: clear the target so the iterator stays done (as `next` does).
                b.props.entry_at_mut(ts)?.set_value(Value::Undefined);
                Some(None)
            }
        }
    }

    /// Whether IteratorClose on `iter` is a no-op: a template-shaped Array Iterator (no own
    /// `return`) with `%ArrayIteratorPrototype%` as prototype and no `return` on the chain.
    pub(crate) fn array_iter_close_is_noop(&self, iter: &Value) -> bool {
        let Value::Obj(it) = iter else {
            return false;
        };
        {
            let t = self.lang.iter_template.borrow();
            let Some((tmpl, _)) = t.as_ref() else {
                return false;
            };
            let b = it.borrow();
            if b.props.shape() != tmpl.shape()
                || !b.ic_plain.get()
                || !matches!(b.exotic, Exotic::None)
                || !matches!((&b.proto, self.iter_intrinsics()), (Some(p), Some((_, _, a)))
                        if Gc::as_ptr(p) == Gc::as_ptr(unsafe { &*a }))
            {
                return false;
            }
        }
        self.array_iter_return_absent()
    }

    /// The elements of a [`pristine_array_iteration`] array as the Array Iterator would yield
    /// them, when every index below `length` is an own data element (no holes, no accessors —
    /// reading those could run user code or consult the prototype chain). `None`: iterate
    /// through the protocol.
    ///
    /// [`pristine_array_iteration`]: Interp::pristine_array_iteration
    pub(crate) fn dense_array_values(&self, arr: &Gc) -> Option<Vec<Value>> {
        let b = arr.borrow();
        let len = b.props.length_property()?;
        if len.accessor() {
            return None;
        }
        let Value::Num(len) = len.value() else {
            return None;
        };
        if !(0.0..=u32::MAX as f64).contains(&len) {
            return None;
        }
        let len = len as u32;
        let mut out = Vec::with_capacity(len as usize);
        if b.props.copy_dense_run(0, len, &mut out) != len {
            return None;
        }
        Some(out)
    }

    /// For-in keys of an ordinary object whose prototype chain contributes no enumerable string
    /// keys (the overwhelmingly common case: only builtin prototypes, whose methods are all
    /// non-enumerable). The result is then just the object's own enumerable string keys in
    /// [[OwnPropertyKeys]] order — no dedupe set, no prototype key lists. `None` = generic path.
    pub(crate) fn for_in_keys_fast(&self, o: &Gc) -> Option<Vec<Value>> {
        let b = o.borrow();
        if !b.ic_plain.get() || !matches!(b.exotic, Exotic::None | Exotic::Array) {
            return None;
        }
        let mut cur = b.proto.clone();
        let mut depth = 0;
        while let Some(p) = cur {
            depth += 1;
            if depth > 16 || Gc::ptr_eq(&p, o) {
                return None;
            }
            let pb = p.try_borrow().ok()?;
            if !pb.ic_plain.get() || !matches!(pb.exotic, Exotic::None | Exotic::Array) {
                return None;
            }
            if has_elements(&pb.props) {
                return None;
            }
            for (k, prop) in pb.props.iter_named() {
                if prop.enumerable() && !Interp::is_sym_key(k) && !Interp::is_private_key(k) {
                    return None;
                }
            }
            cur = pb.proto.clone();
        }
        if has_elements(&b.props) {
            // Index keys first (ascending), then named keys: the reflection order, filtered.
            let mut out = Vec::new();
            for k in b.props.ordered_keys() {
                if Interp::is_sym_key(&k) || Interp::is_private_key(&k) {
                    continue;
                }
                if b.props.get(&k).is_some_and(|p| p.enumerable()) {
                    out.push(Value::Str(LStr::from(&*k)));
                }
            }
            return Some(out);
        }
        self.named_enum_keys(&b)
    }

    /// `Object.keys(o)` of a plain ordinary object without elements: its own enumerable string
    /// keys from the per-shape key list (no key strings built). `None` = generic path.
    pub(crate) fn object_keys_fast(&self, o: &Gc) -> Option<Vec<Value>> {
        let b = o.try_borrow().ok()?;
        if !b.ic_plain.get() || !matches!(b.exotic, Exotic::None) || has_elements(&b.props) {
            return None;
        }
        self.named_enum_keys(&b)
    }

    /// The enumerable string-keyed named properties of `b` (no elements), in order, from the
    /// per-shape `(slot, key)` list. `None` when a named key is an integer (it sorts first).
    fn named_enum_keys(&self, b: &crate::value::Object) -> Option<Vec<Value>> {
        let shape = b.props.shape();
        let list = {
            let hit = self.lang.for_in.borrow().get(&shape).cloned();
            match hit {
                Some(l) => l,
                None => {
                    let mut l: Vec<(u32, Value)> = Vec::new();
                    for (slot, (k, _)) in b.props.iter_named().enumerate() {
                        if Interp::is_sym_key(k) || Interp::is_private_key(k) {
                            continue;
                        }
                        if crate::value::canonical_index(k).is_some() {
                            return None; // named integer keys sort first: generic order
                        }
                        l.push((slot as u32, Value::Str(LStr::from(&**k))));
                    }
                    let l: Rc<[(u32, Value)]> = l.into();
                    let mut m = self.lang.for_in.borrow_mut();
                    if m.len() >= 1024 {
                        m.clear();
                    }
                    m.insert(shape, l.clone());
                    l
                }
            }
        };
        let mut out = Vec::with_capacity(list.len());
        for (slot, key) in list.iter() {
            if b.props.entry_at(*slot as usize)?.enumerable() {
                out.push(key.clone());
            }
        }
        Some(out)
    }

    /// CopyDataProperties(target, source, []) into a still-empty ordinary `target` from an
    /// ordinary `source` whose own properties are all plain enumerable data properties under
    /// named keys: the copy is exactly `source`'s shape with its values (the same keys, order and
    /// `{writable, enumerable, configurable}` attributes CreateDataProperty gives). Returns
    /// `false` (nothing touched) when the gates don't hold.
    pub(crate) fn copy_data_props_fast(&self, target: &Gc, source: &Gc) -> bool {
        if Gc::ptr_eq(target, source) {
            return false;
        }
        let Ok(sb) = source.try_borrow() else {
            return false;
        };
        if !sb.ic_plain.get() || !matches!(sb.exotic, Exotic::None) {
            return false;
        }
        let Ok(mut tb) = target.try_borrow_mut() else {
            return false;
        };
        if !tb.ic_plain.get()
            || !tb.extensible
            || !matches!(tb.exotic, Exotic::None)
            || tb.props.shape() != 0
            || tb.props.values().next().is_some()
            || !tb.props.can_instantiate_inline()
        {
            return false;
        }
        if !sb.props.can_instantiate_inline() || has_elements(&sb.props) {
            return false;
        }
        let mut n = 0usize;
        for (k, p) in sb.props.iter_named() {
            if p.accessor()
                || !p.enumerable()
                || !p.writable()
                || !p.configurable()
                || Interp::is_private_key(k)
                || crate::value::canonical_index(k).is_some()
            {
                return false;
            }
            n += 1;
        }
        let values: Vec<Value> = sb.props.iter_named().map(|(_, p)| p.value()).collect();
        if values.len() != n {
            return false;
        }
        tb.props = sb.props.instantiate_plain(values.into_iter());
        true
    }

    /// `Object.assign(target, source)` for an empty `target` as [`copy_data_props_fast`] does it,
    /// when every [[Set]] would just create an own data property: no key of `source` is an
    /// accessor or read-only anywhere on `target`'s (ordinary) prototype chain.
    ///
    /// [`copy_data_props_fast`]: Interp::copy_data_props_fast
    pub(crate) fn assign_props_fast(&self, target: &Gc, source: &Gc) -> bool {
        {
            let (Ok(tb), Ok(sb)) = (target.try_borrow(), source.try_borrow()) else {
                return false;
            };
            let mut cur = tb.proto.clone();
            let mut depth = 0;
            while let Some(p) = cur {
                depth += 1;
                if depth > 8 || Gc::ptr_eq(&p, source) || Gc::ptr_eq(&p, target) {
                    return false;
                }
                let Ok(pb) = p.try_borrow() else {
                    return false;
                };
                if !pb.ic_plain.get() || !matches!(pb.exotic, Exotic::None) {
                    return false;
                }
                for (k, _) in sb.props.iter_named() {
                    if let Some(pp) = pb.props.get(k) {
                        if pp.accessor() || !pp.writable() {
                            return false;
                        }
                    }
                }
                cur = pb.proto.clone();
            }
        }
        self.copy_data_props_fast(target, source)
    }

    /// A fresh RegExp object for the compiled `re`: its only own property is `lastIndex` (0,
    /// writable, non-enumerable, non-configurable), stamped from a cached property map.
    pub(crate) fn regexp_props(&self) -> Props {
        if let Some(p) = &*self.lang.regexp_template.borrow() {
            return p.clone();
        }
        let mut props = Props::new();
        props.insert(
            "lastIndex",
            Property::data(Value::Num(0.0), true, false, false),
        );
        *self.lang.regexp_template.borrow_mut() = Some(props.clone());
        props
    }

    /// A fresh RegExp object: its `lastIndex` template copied into the box's inline slots.
    pub(crate) fn new_regexp_object(&self, proto: Option<Gc>) -> Gc {
        if self.lang.regexp_template.borrow().is_none() {
            drop(self.regexp_props());
        }
        let template = self.lang.regexp_template.borrow();
        Object::new_inline_copy(proto, template.as_ref().expect("initialized above"))
    }

    /// Tie `obj`'s `regexps` entry to it without keeping it alive (unlike [`Interp::gc_pin`]):
    /// the weak handle reserves the object's heap slot, so no later object can reuse the address
    /// and inherit the entry, while the object itself dies by plain refcounting instead of
    /// waiting for a collection (which pinning every regex literal made frequent). Entries of
    /// dead objects are pruned in amortized batches.
    pub(crate) fn regexp_weak_pin(&mut self, obj: &Gc) {
        let mut pins = self.lang.regexp_weak.borrow_mut();
        if pins.len() >= self.lang.regexp_weak_next.get() {
            let regexps = &mut self.regexps;
            pins.retain(|w| {
                if w.strong_count() > 0 {
                    return true;
                }
                regexps.remove(&(w.as_ptr() as usize));
                false
            });
            self.lang.regexp_weak_next.set((pins.len() * 2).max(1024));
        }
        pins.push(Gc::downgrade(obj));
    }

    /// The compiled program for a regex literal site (`source`/`flags` are the chunk's name
    /// entries — pointer-stable while the chunk lives).
    pub(crate) fn regexp_site_program(
        &mut self,
        source: &Rc<str>,
        flags: &Rc<str>,
    ) -> Result<Rc<crate::regex::Regex>, crate::interpreter::Abrupt> {
        let key = (Rc::as_ptr(source) as *const u8 as usize, Rc::as_ptr(flags) as *const u8 as usize);
        if let Some((s, f, re)) = self.lang.regexp_sites.borrow().get(&key) {
            if Rc::ptr_eq(s, source) && Rc::ptr_eq(f, flags) {
                return Ok(re.clone());
            }
        }
        let re = self.compiled_regexp(source, flags)?;
        let mut m = self.lang.regexp_sites.borrow_mut();
        if m.len() >= 512 {
            m.clear();
        }
        m.insert(key, (source.clone(), flags.clone(), re.clone()));
        Ok(re)
    }
}

/// Whether `props` holds any element (packed or dense-region entry).
fn has_elements(props: &Props) -> bool {
    props.get_index(0).is_some() || props.values().count() != props.iter_named().count()
}
