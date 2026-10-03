//! `PyErr_WriteUnraisable`: an exception that cannot propagate is handed to `sys.unraisablehook`.

use crate::builtins::sysextra::{new_structseq_type, structseq};
use crate::object::*;
use crate::vm::*;

#[derive(Default)]
struct HookArgs {
    ty: Option<Obj>,
}

impl Interp {
    /// Reports `exc` through `sys.unraisablehook`; `err_msg` and `object` say where it happened.
    pub fn write_unraisable(&mut self, exc: &Obj, err_msg: Option<&str>, object: Option<&Value>) {
        let sys = self.sys_module.clone().map(|m| self.module_dict(&m));
        let hook = sys.as_ref().and_then(|d| dict_get_str(d, "unraisablehook"));
        let default = sys.as_ref().and_then(|d| dict_get_str(d, "__unraisablehook__"));
        let custom = match (&hook, &default) {
            (Some(h), Some(d)) if !h.is(d) => hook,
            (Some(h), None) => Some(h.clone()),
            _ => None,
        };
        let err_msg = err_msg.map(Value::str);
        if let Some(h) = custom {
            let args = self.unraisable_args(exc, err_msg.clone(), object.cloned());
            if let Err(e) = self.call(&h, vec![args], Vec::new()) {
                self.flush_out();
                let repr = self.repr_of(&h).unwrap_or_default();
                self.write_stderr(&format!("Exception ignored in sys.unraisablehook: {repr}\n"));
                let text = self.format_exception(&e);
                self.write_stderr(&text);
                self.default_unraisable(exc, err_msg.as_ref(), object);
            }
            return;
        }
        self.default_unraisable(exc, err_msg.as_ref(), object);
    }

    fn unraisable_args(&mut self, exc: &Obj, err_msg: Option<Value>, object: Option<Value>) -> Value {
        let ty = match self.native_state::<HookArgs>().ty.clone() {
            Some(t) => t,
            None => {
                let t = new_structseq_type(self, "sys", "UnraisableHookArgs", &["exc_type", "exc_value", "exc_traceback", "err_msg", "object"]);
                self.native_state::<HookArgs>().ty = Some(t.clone());
                t
            }
        };
        let tb = match &exc.kind {
            Kind::Exception(d) => {
                let entries = d.borrow().tb.clone();
                self.make_tb(&entries)
            }
            _ => Value::None,
        };
        let cls = self.type_of_obj(exc);
        structseq(&ty, vec![Value::Obj(cls), Value::Obj(exc.clone()), tb, err_msg.unwrap_or(Value::None), object.unwrap_or(Value::None)])
    }

    /// The default hook: the report on stderr.
    pub fn default_unraisable(&mut self, exc: &Obj, err_msg: Option<&Value>, object: Option<&Value>) {
        self.flush_out();
        let object = object.filter(|o| !o.is_none());
        let msg = err_msg.filter(|m| !m.is_none()).and_then(|m| self.str_of(m).ok());
        let mut head = String::new();
        match (object, msg) {
            (Some(o), Some(m)) => head = format!("{m}: {}\n", self.repr_of(o).unwrap_or_default()),
            (Some(o), None) => head = format!("Exception ignored in: {}\n", self.repr_of(o).unwrap_or_default()),
            (None, Some(m)) => head = format!("{m}:\n"),
            (None, None) => {}
        }
        head.push_str(&self.format_exception(exc));
        self.write_stderr(&head);
    }
}
