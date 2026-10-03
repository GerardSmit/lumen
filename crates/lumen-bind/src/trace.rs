//! Garbage-collection support for `#[class]` structs: which references an instance's native
//! state holds, so a host's cycle collector can see through it and break the cycles it is part of.
//!
//! `#[class]` implements [`Class::gc_trace`](crate::Class::gc_trace) and
//! [`Class::gc_clear`](crate::Class::gc_clear) from the struct's fields: every field whose type
//! implements [`Trace`] is reported / cleared, the rest (numbers, strings, closures, weak
//! handles) are skipped. A host implements [`Trace`] for its own value types and a [`Visit`]or
//! that picks its references out of the `&dyn Any` edges; the language-neutral containers below
//! and `Rc<T>` (reported as an edge, for hosts whose objects are `Rc`s) are provided here.

use std::any::Any;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::rc::Rc;

/// Receives the references a value holds. The target is the host's own handle type (a
/// `Value`, an `Rc<Object>`, ...); a host's visitor downcasts the ones it knows and ignores the
/// rest.
pub trait Visit {
    fn edge(&mut self, target: &dyn Any);
}

/// A value that can report and drop the references it holds.
pub trait Trace {
    /// Reports every reference this value holds (once per reference).
    fn trace(&self, v: &mut dyn Visit);

    /// Drops the references this value holds, as far as it can (it stays usable).
    fn clear(&mut self) {}
}

impl<T: Trace + ?Sized> Trace for Box<T> {
    fn trace(&self, v: &mut dyn Visit) {
        (**self).trace(v)
    }
    fn clear(&mut self) {
        (**self).clear()
    }
}

impl<T: Trace> Trace for Option<T> {
    fn trace(&self, v: &mut dyn Visit) {
        if let Some(x) = self {
            x.trace(v);
        }
    }
    fn clear(&mut self) {
        *self = None;
    }
}

impl<T: Trace> Trace for Vec<T> {
    fn trace(&self, v: &mut dyn Visit) {
        for x in self {
            x.trace(v);
        }
    }
    fn clear(&mut self) {
        Vec::clear(self);
    }
}

impl<T: Trace> Trace for VecDeque<T> {
    fn trace(&self, v: &mut dyn Visit) {
        for x in self {
            x.trace(v);
        }
    }
    fn clear(&mut self) {
        VecDeque::clear(self);
    }
}

impl<T: Trace, const N: usize> Trace for [T; N] {
    fn trace(&self, v: &mut dyn Visit) {
        for x in self {
            x.trace(v);
        }
    }
    fn clear(&mut self) {
        for x in self {
            x.clear();
        }
    }
}

impl<T: Trace> Trace for RefCell<T> {
    fn trace(&self, v: &mut dyn Visit) {
        if let Ok(b) = self.try_borrow() {
            b.trace(v);
        }
    }
    fn clear(&mut self) {
        self.get_mut().clear();
    }
}

impl<K: Trace, V: Trace, S> Trace for HashMap<K, V, S> {
    fn trace(&self, v: &mut dyn Visit) {
        for (k, x) in self {
            k.trace(v);
            x.trace(v);
        }
    }
    fn clear(&mut self) {
        HashMap::clear(self);
    }
}

impl<K: Trace, V: Trace> Trace for BTreeMap<K, V> {
    fn trace(&self, v: &mut dyn Visit) {
        for (k, x) in self {
            k.trace(v);
            x.trace(v);
        }
    }
    fn clear(&mut self) {
        BTreeMap::clear(self);
    }
}

/// A shared handle is one edge: the host decides whether it is one of its objects.
impl<T: ?Sized + 'static> Trace for Rc<T> {
    fn trace(&self, v: &mut dyn Visit) {
        v.edge(self);
    }
}

macro_rules! tuple_trace {
    ($(($($n:ident . $i:tt),+)),+) => {$(
        impl<$($n: Trace),+> Trace for ($($n,)+) {
            fn trace(&self, v: &mut dyn Visit) {
                $(self.$i.trace(v);)+
            }
            fn clear(&mut self) {
                $(self.$i.clear();)+
            }
        }
    )+};
}

tuple_trace!((A.0, B.1), (A.0, B.1, C.2), (A.0, B.1, C.2, D.3));
