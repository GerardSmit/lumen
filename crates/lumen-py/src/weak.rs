//! Weak reference bookkeeping. Objects are reference counted, so a referent dies exactly when its
//! last strong reference goes away; the registry lets that moment find the weak references (and
//! their callbacks) that pointed at it.

use crate::object::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

#[lumen_bind::class(name = "ReferenceType", module = "weakref")]
pub struct WeakRefData {
    pub target: Weak<Object>,
    pub callback: Value,
    pub hash: Option<i64>,
}

#[lumen_bind::class(name = "ProxyType", module = "weakref", hint(py(unhashable)))]
pub struct ProxyData {
    pub target: Weak<Object>,
    pub callback: Value,
}

thread_local! {
    static REGISTRY: RefCell<HashMap<u32, Vec<Weak<Object>>>> = RefCell::new(HashMap::new());
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
    static DEAD: RefCell<Vec<Obj>> = const { RefCell::new(Vec::new()) };
    static PENDING: Cell<bool> = const { Cell::new(false) };
}

/// The weak-reference bookkeeping of the interpreter, which follows it from thread to thread as
/// the GIL changes hands (see `threads`).
pub(crate) struct WeakState {
    registry: HashMap<u32, Vec<Weak<Object>>>,
    active: bool,
    dead: Vec<Obj>,
    pending: bool,
}

pub(crate) fn state_take() -> WeakState {
    WeakState {
        registry: REGISTRY.with(|r| std::mem::take(&mut *r.borrow_mut())),
        active: ACTIVE.with(|a| a.replace(false)),
        dead: DEAD.with(|d| std::mem::take(&mut *d.borrow_mut())),
        pending: PENDING.with(|p| p.replace(false)),
    }
}

pub(crate) fn state_put(state: WeakState) {
    let old = REGISTRY.with(|r| std::mem::replace(&mut *r.borrow_mut(), state.registry));
    ACTIVE.with(|a| a.set(state.active));
    let dead = DEAD.with(|d| std::mem::replace(&mut *d.borrow_mut(), state.dead));
    PENDING.with(|p| p.set(state.pending));
    drop(old);
    drop(dead);
}

pub fn register(target: &Obj, weakref: &Obj) {
    ACTIVE.with(|a| a.set(true));
    let id = target.identity();
    REGISTRY.with(|r| r.borrow_mut().entry(id).or_default().push(Rc::downgrade(weakref)));
}

pub fn live_refs(target: &Obj) -> Vec<Obj> {
    let id = target.id.get();
    if id == 0 {
        return Vec::new();
    }
    REGISTRY.with(|r| {
        let mut r = r.borrow_mut();
        let Some(list) = r.get_mut(&id) else { return Vec::new() };
        list.retain(|w| w.strong_count() > 0);
        list.iter().filter_map(|w| w.upgrade()).collect()
    })
}

fn callback_of(o: &Obj) -> Option<Value> {
    let Kind::Opaque(cell) = &o.kind else { return None };
    let b = cell.try_borrow().ok()?;
    if let Some(w) = b.downcast_ref::<WeakRefData>() {
        return Some(w.callback.clone()).filter(|c| !c.is_none());
    }
    if let Some(p) = b.downcast_ref::<ProxyData>() {
        return Some(p.callback.clone()).filter(|c| !c.is_none());
    }
    None
}

/// Called from `Object::drop` for an object that had an identity assigned.
pub fn on_object_drop(id: u32) {
    if !ACTIVE.try_with(|a| a.get()).unwrap_or(false) {
        return;
    }
    let refs = REGISTRY.try_with(|r| r.try_borrow_mut().ok().and_then(|mut r| r.remove(&id))).ok().flatten();
    let Some(refs) = refs else { return };
    for w in refs {
        if let Some(o) = w.upgrade() {
            if callback_of(&o).is_some() {
                let _ = DEAD.try_with(|d| d.borrow_mut().push(o));
                let _ = PENDING.try_with(|p| p.set(true));
            }
        }
    }
}

#[inline]
pub fn has_pending() -> bool {
    PENDING.with(|p| p.get())
}

pub fn take_pending() -> Vec<Obj> {
    PENDING.with(|p| p.set(false));
    DEAD.with(|d| std::mem::take(&mut *d.borrow_mut()))
}

/// Removes and returns the callback so it fires at most once.
pub fn take_callback(o: &Obj) -> Option<Value> {
    let Kind::Opaque(cell) = &o.kind else { return None };
    let mut b = cell.try_borrow_mut().ok()?;
    if let Some(w) = b.downcast_mut::<WeakRefData>() {
        return Some(std::mem::replace(&mut w.callback, Value::None)).filter(|c| !c.is_none());
    }
    if let Some(p) = b.downcast_mut::<ProxyData>() {
        return Some(std::mem::replace(&mut p.callback, Value::None)).filter(|c| !c.is_none());
    }
    None
}

fn with_target<X>(o: &Obj, f: impl FnOnce(&mut Weak<Object>) -> X) -> Option<X> {
    let Kind::Opaque(cell) = &o.kind else { return None };
    let mut b = cell.try_borrow_mut().ok()?;
    if let Some(w) = b.downcast_mut::<WeakRefData>() {
        return Some(f(&mut w.target));
    }
    b.downcast_mut::<ProxyData>().map(|p| f(&mut p.target))
}

/// Makes the weak references of the object with identity `id` follow it to `new` (an object
/// that was moved into a fresh allocation to be finalized).
pub fn retarget(id: u32, new: &Obj) {
    let refs: Vec<Obj> = REGISTRY
        .try_with(|r| match r.try_borrow() {
            Ok(r) => r.get(&id).map(|l| l.iter().filter_map(|w| w.upgrade()).collect::<Vec<Obj>>()).unwrap_or_default(),
            Err(_) => Vec::new(),
        })
        .unwrap_or_default();
    for r in &refs {
        with_target(r, |t| *t = Rc::downgrade(new));
    }
}

/// Clears every weak reference to the object with identity `id`, which the cycle collector found
/// unreachable, and returns them (their callbacks are the collector's to call).
pub fn clear_refs(id: u32) -> Vec<Obj> {
    let list = REGISTRY.try_with(|r| r.try_borrow_mut().ok().and_then(|mut r| r.remove(&id))).ok().flatten().unwrap_or_default();
    let mut out = Vec::new();
    for w in list {
        if let Some(o) = w.upgrade() {
            with_target(&o, |t| *t = Weak::new());
            out.push(o);
        }
    }
    out
}

/// The weak reference objects (`ref` and proxies) currently pointing at the object with
/// identity `id`.
pub fn refs_of(id: u32) -> Vec<Obj> {
    REGISTRY
        .try_with(|r| match r.try_borrow() {
            Ok(r) => r.get(&id).map(|l| l.iter().filter_map(|w| w.upgrade()).collect::<Vec<Obj>>()).unwrap_or_default(),
            Err(_) => Vec::new(),
        })
        .unwrap_or_default()
}

/// Wakes the poll sites: something (a finalizer to run, a callback to call) is queued.
pub fn mark_pending() {
    let _ = PENDING.try_with(|p| p.set(true));
}

/// Whether the weak reference `o` has a callback left to call.
pub fn has_callback(o: &Obj) -> bool {
    callback_of(o).is_some()
}
