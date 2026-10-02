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
