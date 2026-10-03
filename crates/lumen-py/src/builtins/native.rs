//! Helpers shared by the native extension modules (`itertools`, `_collections`, `_weakref`, ...).

use crate::object::*;
use crate::vm::*;
use std::any::Any;
use std::cell::RefCell;

pub type Kw<'a> = &'a [(Obj, Value)];

/// A new builtin type whose own special methods are dispatched like those of a Python class.
pub fn new_type(it: &mut Interp, module: &str, name: &str, base: Option<&Obj>, layout: Layout) -> Obj {
    let ty = new_type_raw(name, layout);
    if let Kind::Type(td) = &ty.kind {
        td.flags.set(TF_DISPATCH);
    }
    if let Some(d) = ty.dict.borrow().as_ref() {
        dict_set_str(d, "__module__", Value::str(module));
    }
    let base = base.cloned().unwrap_or_else(|| it.types.object.clone());
    it.set_bases(&ty, vec![base]);
    ty
}

pub fn new_opaque(cls: &Obj, data: impl Any) -> Value {
    Value::Obj(Object::with_cls(cls.clone(), Kind::Opaque(RefCell::new(Box::new(data)))))
}

/// Runs `f` on the native state of `v` if it is an opaque object holding a `T`.
pub fn with_opaque<T: Any, X>(v: &Value, f: impl FnOnce(&mut T) -> X) -> Option<X> {
    let Value::Obj(o) = v else { return None };
    let Kind::Opaque(cell) = &o.kind else { return None };
    let mut b = cell.try_borrow_mut().ok()?;
    b.downcast_mut::<T>().map(f)
}

pub fn set_type(d: &Obj, name: &str, ty: &Obj) {
    dict_set_str(d, name, Value::Obj(ty.clone()));
}

impl Interp {
    pub fn self_state_err(&mut self, ty: &str) -> Obj {
        self.type_error(&format!("descriptor requires a '{}' object", ty))
    }

    pub fn is_callable(&mut self, v: &Value) -> bool {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Function(_) | Kind::Native(_) | Kind::Method(..) | Kind::Type(_) => true,
                _ => {
                    let cls = self.type_of_obj(o);
                    self.lookup_mro(&cls, "__call__").is_some()
                }
            },
            _ => false,
        }
    }
}
