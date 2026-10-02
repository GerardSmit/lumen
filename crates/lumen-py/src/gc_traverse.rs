//! What every object references (`traverse`, CPython's `tp_traverse`) and how to drop those
//! references (`clear`, `tp_clear`). The cycle collector in [`crate::gc`] is built on these two.
//!
//! `traverse` reports each strong reference exactly once and never reports a reference that is not
//! a real strong one: an unreported reference only makes its target look externally held (a leak
//! at worst), a spurious one could free a live object.

use crate::bind::Py;
use crate::dict::PyDict;
use crate::object::*;
use crate::vm::Frame;
use lumen_bind::{Class, Trace, Visit};
use std::any::{Any, TypeId};
use std::cell::RefCell;
use std::collections::HashMap;

impl Trace for Value {
    fn trace(&self, v: &mut dyn Visit) {
        if let Value::Obj(o) = self {
            v.edge(o);
        }
    }

    fn clear(&mut self) {
        *self = Value::None;
    }
}

impl<T: Class> Trace for Py<T> {
    fn trace(&self, v: &mut dyn Visit) {
        self.value().trace(v);
    }

    fn clear(&mut self) {
        self.release();
    }
}

impl Trace for PyDict {
    fn trace(&self, v: &mut dyn Visit) {
        for e in self.iter() {
            e.key.trace(v);
            e.val.trace(v);
        }
    }

    fn clear(&mut self) {
        PyDict::clear(self);
    }
}

/// A [`Visit`] that hands the objects among the edges to a closure.
struct Sink<'a>(&'a mut dyn FnMut(&Obj));

impl Visit for Sink<'_> {
    fn edge(&mut self, target: &dyn Any) {
        if let Some(o) = target.downcast_ref::<Obj>() {
            (self.0)(o);
        }
    }
}

#[derive(Clone, Copy)]
struct Hooks {
    trace: fn(&dyn Any, &mut dyn Visit),
    clear: fn(&mut dyn Any),
}

/// The hooks are plain function pointers keyed by type, shared by every thread.
static HOOKS: std::sync::Mutex<Option<HashMap<TypeId, Hooks>>> = std::sync::Mutex::new(None);

fn hooks_table() -> std::sync::MutexGuard<'static, Option<HashMap<TypeId, Hooks>>> {
    HOOKS.lock().unwrap_or_else(|e| e.into_inner())
}

fn trace_hook<T: Class>(state: &dyn Any, v: &mut dyn Visit) {
    if let Some(t) = state.downcast_ref::<T>() {
        t.gc_trace(v);
    }
}

fn clear_hook<T: Class>(state: &mut dyn Any) {
    if let Some(t) = state.downcast_mut::<T>() {
        t.gc_clear();
    }
}

/// Makes the native state of class `T` visible to the collector (called when `T`'s type object is
/// created).
pub fn register<T: Class>() {
    hooks_table().get_or_insert_with(HashMap::new).entry(TypeId::of::<T>()).or_insert(Hooks { trace: trace_hook::<T>, clear: clear_hook::<T> });
}

fn hooks_for(state: &dyn Any) -> Option<Hooks> {
    let id = state.type_id();
    hooks_table().as_ref().and_then(|h| h.get(&id).copied())
}

fn val(v: &Value, visit: &mut dyn FnMut(&Obj)) {
    if let Value::Obj(o) = v {
        visit(o);
    }
}

fn frame(fr: &Frame, visit: &mut dyn FnMut(&Obj)) {
    for v in &fr.stack {
        val(v, visit);
    }
    for v in fr.locals.iter().flatten() {
        val(v, visit);
    }
    for c in &fr.cells {
        visit(c);
    }
    visit(&fr.globals);
    if let Some(n) = &fr.names {
        visit(n);
    }
    if let Some(f) = &fr.func {
        visit(f);
    }
}

fn iter_state(st: &IterState, visit: &mut dyn FnMut(&Obj)) {
    match st {
        IterState::List { list: o, .. }
        | IterState::Tuple { tup: o, .. }
        | IterState::Str { s: o, .. }
        | IterState::Bytes { b: o, .. }
        | IterState::Dict { dict: o, .. }
        | IterState::Set { set: o, .. } => visit(o),
        IterState::Seq { obj: v, .. } | IterState::Reversed { seq: v, .. } | IterState::Enumerate { it: v, .. } => val(v, visit),
        IterState::CallIter { f, sentinel, .. } => {
            val(f, visit);
            val(sentinel, visit);
        }
        IterState::Zip { its, .. } => {
            for v in its {
                val(v, visit);
            }
        }
        IterState::Map { f, its, .. } => {
            val(f, visit);
            for v in its {
                val(v, visit);
            }
        }
        IterState::Filter { f, it } => {
            val(f, visit);
            val(it, visit);
        }
        IterState::Range { .. } | IterState::Native(_) | IterState::Running | IterState::Empty => {}
    }
}

/// Calls `visit` once for every strong reference `o` holds to an object.
pub fn traverse(o: &Object, visit: &mut dyn FnMut(&Obj)) {
    if let Some(c) = &o.cls {
        visit(c);
    }
    if let Ok(d) = o.dict.try_borrow() {
        if let Some(d) = &*d {
            visit(d);
        }
    }
    match &o.kind {
        Kind::Tuple(items) => {
            for v in items {
                val(v, visit);
            }
        }
        Kind::List(l) => {
            if let Ok(l) = l.try_borrow() {
                for v in l.iter() {
                    val(v, visit);
                }
            }
        }
        Kind::Dict(d) | Kind::Set(d) | Kind::FrozenSet(d) => {
            if let Ok(d) = d.try_borrow() {
                for e in d.iter() {
                    val(&e.key, visit);
                    val(&e.val, visit);
                }
            }
        }
        Kind::Type(td) => {
            if let Ok(b) = td.bases.try_borrow() {
                for c in b.iter() {
                    visit(c);
                }
            }
            if let Ok(m) = td.mro.try_borrow() {
                for c in m.iter() {
                    visit(c);
                }
            }
        }
        Kind::Function(f) => {
            visit(&f.globals);
            if let Ok(d) = f.defaults.try_borrow() {
                for v in d.iter() {
                    val(v, visit);
                }
            }
            if let Ok(d) = f.kwdefaults.try_borrow() {
                for (k, v) in d.iter() {
                    visit(k);
                    val(v, visit);
                }
            }
            for c in &f.closure {
                visit(c);
            }
            if let Ok(a) = f.annotations.try_borrow() {
                if let Some(a) = &*a {
                    visit(a);
                }
            }
            if let Ok(t) = f.type_params.try_borrow() {
                if let Some(t) = &*t {
                    val(t, visit);
                }
            }
        }
        Kind::Method(a, b) => {
            val(a, visit);
            val(b, visit);
        }
        Kind::Native(nd) => {
            if let Some(NativeOwner::Class(c)) = &nd.owner {
                visit(c);
            }
        }
        Kind::Cell(c) => {
            if let Ok(c) = c.try_borrow() {
                if let Some(v) = &*c {
                    val(v, visit);
                }
            }
        }
        Kind::Generator(gd) => {
            if let Ok(s) = gd.state.try_borrow() {
                if let GenState::Created(fr) | GenState::Suspended(fr) = &*s {
                    frame(fr, visit);
                }
            }
        }
        Kind::Exception(e) => {
            if let Ok(e) = e.try_borrow() {
                val(&e.args, visit);
                if let Some(c) = &e.cause {
                    visit(c);
                }
                if let Some(c) = &e.context {
                    visit(c);
                }
                for t in &e.tb {
                    visit(&t.globals);
                }
            }
        }
        Kind::Slice(a, b, c) | Kind::Super(a, b, c) => {
            val(a, visit);
            val(b, visit);
            val(c, visit);
        }
        Kind::Iter(st) => {
            if let Ok(st) = st.try_borrow() {
                iter_state(&st, visit);
            }
        }
        Kind::Property(p) => {
            val(&p.fget, visit);
            val(&p.fset, visit);
            val(&p.fdel, visit);
            val(&p.doc, visit);
        }
        Kind::StaticMethod(v) | Kind::ClassMethod(v) | Kind::AsyncGenValue(v) => val(v, visit),
        Kind::DictView(d, _) => visit(d),
        Kind::Opaque(cell) => {
            if let Ok(b) = cell.try_borrow() {
                let state: &dyn Any = &**b;
                if let Some(h) = hooks_for(state) {
                    (h.trace)(state, &mut Sink(&mut *visit));
                }
            }
        }
        Kind::Instance
        | Kind::Str(_)
        | Kind::Int(_)
        | Kind::Float(_)
        | Kind::Complex(..)
        | Kind::Bytes(_)
        | Kind::ByteArray(_)
        | Kind::Module
        | Kind::Code(_)
        | Kind::Range(_)
        | Kind::BigRange(_)
        | Kind::Frame => {}
    }
}

/// The objects `o` refers to, one entry per reference.
pub fn referents(o: &Object) -> Vec<Obj> {
    let mut out = Vec::new();
    traverse(o, &mut |t| out.push(t.clone()));
    out
}

/// Drops the references `o` holds that can be dropped (CPython's `tp_clear`), breaking the cycles
/// it is part of. What is taken out is released only after every borrow of `o`'s state ended.
pub fn clear(o: &Object) {
    let dict = o.dict.try_borrow_mut().ok().and_then(|mut d| d.take());
    match &o.kind {
        Kind::List(l) => {
            let old = l.try_borrow_mut().map(|mut b| std::mem::take(&mut *b)).unwrap_or_default();
            drop(old);
        }
        Kind::Dict(d) | Kind::Set(d) | Kind::FrozenSet(d) => {
            let old = d.try_borrow_mut().map(|mut b| b.take_all()).ok();
            drop(old);
        }
        Kind::Type(td) => {
            let bases = td.bases.try_borrow_mut().map(|mut b| std::mem::take(&mut *b)).unwrap_or_default();
            let mro = td.mro.try_borrow_mut().map(|mut b| std::mem::take(&mut *b)).unwrap_or_default();
            drop((bases, mro));
        }
        Kind::Function(f) => {
            let defaults = f.defaults.try_borrow_mut().map(|mut b| std::mem::take(&mut *b)).unwrap_or_default();
            let kwdefaults = f.kwdefaults.try_borrow_mut().map(|mut b| std::mem::take(&mut *b)).unwrap_or_default();
            let ann = f.annotations.try_borrow_mut().ok().and_then(|mut b| b.take());
            let params = f.type_params.try_borrow_mut().ok().and_then(|mut b| b.take());
            drop((defaults, kwdefaults, ann, params));
        }
        Kind::Cell(c) => {
            let old = c.try_borrow_mut().ok().and_then(|mut b| b.take());
            drop(old);
        }
        Kind::Generator(gd) => {
            let old = match gd.state.try_borrow_mut() {
                Ok(mut s) if matches!(&*s, GenState::Created(_) | GenState::Suspended(_)) => Some(std::mem::replace(&mut *s, GenState::Done)),
                _ => None,
            };
            drop(old);
        }
        Kind::Exception(e) => {
            let old = e.try_borrow_mut().ok().map(|mut e| {
                let args = std::mem::replace(&mut e.args, Value::tuple(Vec::new()));
                (args, e.cause.take(), e.context.take(), std::mem::take(&mut e.tb))
            });
            drop(old);
        }
        Kind::Iter(st) => {
            let old = match st.try_borrow_mut() {
                Ok(mut s) if !matches!(&*s, IterState::Running) => Some(std::mem::replace(&mut *s, IterState::Empty)),
                _ => None,
            };
            drop(old);
        }
        Kind::Opaque(cell) => {
            if let Ok(mut b) = cell.try_borrow_mut() {
                let state: &mut dyn Any = &mut **b;
                if let Some(h) = hooks_for(&*state) {
                    (h.clear)(state);
                }
            }
        }
        _ => {}
    }
    drop(dict);
}
