//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;
use crate::value::WeakGc;

// WeakRef / FinalizationRegistry.
//
// Targets are held through `WeakGc`s in an interpreter side table ([`WeakState`]), so they die
// with the rest of the heap: by refcount, or in a cycle collection. Creating a WeakRef and
// `deref` keep the target alive until the end of the current job (AddToKeptObjects), which lumen
// ends at the next microtask checkpoint (ClearKeptObjects). After a collection, that checkpoint
// also finds the registrations whose targets died and queues one job per dead cell calling the
// registry's cleanup callback with the held value (the host-defined timing of
// HostEnqueueFinalizationRegistryCleanupJob — like Node, callbacks follow a GC; a throwing
// callback surfaces as an unhandled rejection). Symbols, the only other weakly holdable values,
// are never collected by lumen and are simply held strongly.

/// A weakly held value.
enum Weak {
    Obj(WeakGc),
    Sym(Value),
}

impl Weak {
    fn new(v: &Value) -> Weak {
        match v {
            Value::Obj(o) => Weak::Obj(Gc::downgrade(o)),
            other => Weak::Sym(other.clone()),
        }
    }
    fn get(&self) -> Option<Value> {
        match self {
            Weak::Obj(w) => w.upgrade().map(Value::Obj),
            Weak::Sym(v) => Some(v.clone()),
        }
    }
    fn alive(&self) -> bool {
        match self {
            Weak::Obj(w) => w.strong_count() > 0,
            Weak::Sym(_) => true,
        }
    }
    /// SameValue with `v` (a held `WeakGc` keeps its address from being reused).
    fn is(&self, v: &Value) -> bool {
        match (self, v) {
            (Weak::Obj(w), Value::Obj(o)) => w.as_ptr() == Gc::as_ptr(o),
            (Weak::Sym(s), v) => same_value(s, v),
            _ => false,
        }
    }
}

/// A FinalizationRegistry Cell record.
struct FrCell {
    target: Weak,
    held: Value,
    token: Option<Weak>,
}

struct Registry {
    this: WeakGc,
    cells: Vec<FrCell>,
}

/// The interpreter's weak-reference state (see the comment above).
#[derive(Default)]
pub(crate) struct WeakState {
    /// WeakRef object ptr → (the WeakRef, its [[WeakRefTarget]]).
    refs: crate::fasthash::FastMap<usize, (WeakGc, Weak)>,
    /// FinalizationRegistry ptr → its [[Cells]].
    registries: crate::fasthash::FastMap<usize, Registry>,
    /// [[KeptAlive]]: objects kept until the end of the current job, by address.
    kept: crate::fasthash::FastMap<usize, Gc>,
    /// A collection ran since the last scan for dead targets.
    scan_due: bool,
}

impl Interp {
    /// AddToKeptObjects.
    fn keep_during_job(&mut self, v: &Value) {
        if let Value::Obj(o) = v {
            self.weak
                .kept
                .entry(Gc::as_ptr(o) as usize)
                .or_insert_with(|| o.clone());
        }
    }

    /// The collector ran: registered targets may have died.
    pub(crate) fn weak_note_collection(&mut self) {
        if !self.weak.registries.is_empty() || !self.weak.refs.is_empty() {
            self.weak.scan_due = true;
        }
    }

    /// The end of a job (a microtask checkpoint reaching quiescence): ClearKeptObjects, then —
    /// after a collection — queue the cleanup jobs of registrations whose targets died. Returns
    /// whether any job was queued.
    #[inline]
    pub(crate) fn weak_checkpoint(&mut self) -> bool {
        if !self.weak.kept.is_empty() {
            self.weak.kept.clear();
        }
        if !self.weak.scan_due {
            return false;
        }
        self.weak_scan()
    }

    #[inline(never)]
    fn weak_scan(&mut self) -> bool {
        self.weak.scan_due = false;
        // A WeakRef whose target (or itself) is gone needs no entry: `deref` then reads undefined.
        self.weak
            .refs
            .retain(|_, (r, t)| r.strong_count() > 0 && t.alive());
        let mut due: Vec<(Value, Value)> = Vec::new();
        self.weak.registries.retain(|_, reg| {
            // A registry that was itself collected runs no cleanup.
            let Some(r) = reg.this.upgrade() else {
                return false;
            };
            let callback = r.borrow().props.get("\u{0}fr").map(|p| p.value());
            reg.cells.retain(|c| {
                if c.target.alive() {
                    return true;
                }
                if let Some(cb) = &callback {
                    due.push((cb.clone(), c.held.clone()));
                }
                false
            });
            !reg.cells.is_empty()
        });
        let mut queued = false;
        for (handler, held) in due {
            if !handler.is_callable() {
                continue;
            }
            let result = self.new_promise();
            self.microtasks.push_back(crate::interpreter::Job {
                handler,
                result,
                value: held,
                fulfilled: true,
                kind: 0,
                idx: 0,
                context: Value::Undefined,
            });
            queued = true;
        }
        queued
    }
}

pub(super) fn install_weak_refs(it: &mut Interp) {
    let wr_proto = Object::new(Some(it.object_proto.clone()));
    it.def_method(&wr_proto, "deref", 0, |i, this, _| {
        if !matches!(&this, Value::Obj(o) if o.borrow().props.contains("\u{0}weakref-target")) {
            return Err(i.make_error("TypeError", "deref called on a non-WeakRef"));
        }
        let target = this
            .as_obj()
            .and_then(|o| i.weak.refs.get(&(Gc::as_ptr(o) as usize)))
            .and_then(|(_, t)| t.get());
        match target {
            Some(t) => {
                i.keep_during_job(&t);
                Ok(t)
            }
            None => Ok(Value::Undefined),
        }
    });
    let wr_ctor = it.make_native("WeakRef", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "WeakRef requires 'new'"));
        }
        let target = arg(a, 0);
        if !can_be_held_weakly(i, &target) {
            return Err(i.make_error("TypeError", "WeakRef target must be an object or symbol"));
        }
        let obj = new_from_ctor(i, "WeakRef")?;
        // The brand key is \0-prefixed so it is invisible to every own-property enumeration
        // path; the target itself is held weakly in the side table.
        set_internal(&obj, "\u{0}weakref-target", Value::Bool(true));
        i.keep_during_job(&target);
        i.weak.refs.insert(
            Gc::as_ptr(&obj) as usize,
            (Gc::downgrade(&obj), Weak::new(&target)),
        );
        Ok(Value::Obj(obj))
    });
    it.extra_protos.insert("WeakRef", wr_proto.clone());
    wr_ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(wr_proto.clone()), false, false, false),
    );
    wr_proto.borrow_mut().props.insert(
        "constructor",
        Property::builtin(Value::Obj(wr_ctor.clone())),
    );
    if let Some(key) = well_known_key(it, "toStringTag") {
        wr_proto.borrow_mut().props.insert(
            key,
            Property::data(Value::str("WeakRef"), false, false, true),
        );
    }
    set_builtin(&it.global, "WeakRef", Value::Obj(wr_ctor));

    let fr_proto = Object::new(Some(it.object_proto.clone()));
    it.def_method(&fr_proto, "register", 2, |i, this, a| {
        // Brand check, then: target must be registerable, distinct from its held value, and any
        // unregister token must itself be registerable.
        if !matches!(&this, Value::Obj(o) if o.borrow().props.contains("\u{0}fr")) {
            return Err(i.make_error("TypeError", "register called on a non-FinalizationRegistry"));
        }
        let target = arg(a, 0);
        if !can_be_held_weakly(i, &target) {
            return Err(i.make_error("TypeError", "target cannot be held weakly"));
        }
        if same_value(&target, &arg(a, 1)) {
            return Err(i.make_error("TypeError", "target and held value must not be the same"));
        }
        let token = arg(a, 2);
        if !matches!(token, Value::Undefined) && !can_be_held_weakly(i, &token) {
            return Err(i.make_error("TypeError", "unregister token cannot be held weakly"));
        }
        if let Value::Obj(o) = &this {
            let cell = FrCell {
                target: Weak::new(&target),
                held: arg(a, 1),
                token: (!matches!(token, Value::Undefined)).then(|| Weak::new(&token)),
            };
            i.weak
                .registries
                .entry(Gc::as_ptr(o) as usize)
                .or_insert_with(|| Registry {
                    this: Gc::downgrade(o),
                    cells: Vec::new(),
                })
                .cells
                .push(cell);
        }
        Ok(Value::Undefined)
    });
    it.def_method(&fr_proto, "unregister", 1, |i, this, a| {
        if !matches!(&this, Value::Obj(o) if o.borrow().props.contains("\u{0}fr")) {
            return Err(i.make_error(
                "TypeError",
                "unregister called on a non-FinalizationRegistry",
            ));
        }
        let token = arg(a, 0);
        if !can_be_held_weakly(i, &token) {
            return Err(i.make_error("TypeError", "unregister token cannot be held weakly"));
        }
        let mut removed = false;
        if let Value::Obj(o) = &this {
            if let Some(reg) = i.weak.registries.get_mut(&(Gc::as_ptr(o) as usize)) {
                let before = reg.cells.len();
                reg.cells
                    .retain(|c| !c.token.as_ref().is_some_and(|t| t.is(&token)));
                removed = reg.cells.len() != before;
            }
        }
        Ok(Value::Bool(removed))
    });
    let fr_ctor = it.make_native("FinalizationRegistry", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "FinalizationRegistry requires 'new'"));
        }
        if !arg(a, 0).is_callable() {
            return Err(i.make_error("TypeError", "cleanup callback must be callable"));
        }
        let obj = new_from_ctor(i, "FinalizationRegistry")?;
        // [[CleanupCallback]] (and the brand).
        set_internal(&obj, "\u{0}fr", arg(a, 0));
        Ok(Value::Obj(obj))
    });
    it.extra_protos
        .insert("FinalizationRegistry", fr_proto.clone());
    fr_ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(fr_proto.clone()), false, false, false),
    );
    fr_proto.borrow_mut().props.insert(
        "constructor",
        Property::builtin(Value::Obj(fr_ctor.clone())),
    );
    if let Some(key) = well_known_key(it, "toStringTag") {
        fr_proto.borrow_mut().props.insert(
            key,
            Property::data(Value::str("FinalizationRegistry"), false, false, true),
        );
    }
    set_builtin(&it.global, "FinalizationRegistry", Value::Obj(fr_ctor));
}
