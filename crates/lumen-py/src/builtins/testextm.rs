//! `_testmultiphase` (an extension module with multi-phase initialisation, PEP 489) and
//! `_testimportmultiple` with the two modules that share its file. The helpers that give an
//! object its own attribute dictionary are shared with `xxlimited` and `xxlimited_35`.

use crate::bind::{type_object, This};
use crate::builtins::native::{new_type, with_opaque};
use crate::object::*;
use crate::vm::{dict_get_str, dict_set_str, Interp};

fn name_of(it: &mut Interp, name: &Value) -> R<String> {
    match name.as_str() {
        Some(s) => Ok(s.to_string()),
        None => {
            let t = it.type_name_of(name);
            Err(it.type_error(&format!("attribute name must be string, not '{t}'")))
        }
    }
}

/// The attributes an object keeps in a dictionary of its own.
#[derive(Default)]
pub struct AttrDict(Vec<(String, Value)>);

/// Native state that carries an [`AttrDict`].
pub trait HasAttrs: 'static {
    fn attrs(&mut self) -> &mut AttrDict;
}

/// `getattr`: the object's own attributes first, then the type's.
pub fn get_attr<T: HasAttrs>(slf: &Value, it: &mut Interp, name: &Value) -> R<Value> {
    let n = name_of(it, name)?;
    if let Some(Some(v)) = with_opaque::<T, _>(slf, |e| e.attrs().0.iter().find(|(k, _)| *k == n).map(|(_, v)| v.clone())) {
        return Ok(v);
    }
    let Value::Obj(name_obj) = name else { unreachable!("name_of accepts strings only") };
    let cls = it.type_of(slf);
    it.generic_getattr(slf, &cls, name_obj)
}

pub fn set_attr<T: HasAttrs>(slf: &Value, it: &mut Interp, name: &Value, value: &Value) -> R<()> {
    let n = name_of(it, name)?;
    with_opaque::<T, _>(slf, |e| {
        let attrs = &mut e.attrs().0;
        match attrs.iter_mut().find(|(k, _)| *k == n) {
            Some(slot) => slot.1 = value.clone(),
            None => attrs.push((n, value.clone())),
        }
    });
    Ok(())
}

/// `delattr`: only the object's own attributes can be deleted.
pub fn del_attr<T: HasAttrs>(slf: &Value, it: &mut Interp, name: &Value) -> R<()> {
    let n = name_of(it, name)?;
    let removed = with_opaque::<T, _>(slf, |e| {
        let attrs = &mut e.attrs().0;
        let before = attrs.len();
        attrs.retain(|(k, _)| *k != n);
        attrs.len() != before
    });
    if removed == Some(true) {
        Ok(())
    } else {
        let t = it.type_name_of(slf);
        Err(it.new_exc_str("AttributeError", &format!("delete non-existing {t} attribute")))
    }
}

/// `demo(o=None)`: returns `o` when it is a string, else None.
pub fn demo_optional(it: &mut Interp, args: &[Value]) -> R<Value> {
    if args.len() > 1 {
        return Err(it.type_error(&format!("demo expected at most 1 argument, got {}", args.len())));
    }
    match args.first() {
        Some(o) if o.as_str().is_some() => Ok(o.clone()),
        _ => Ok(Value::None),
    }
}

/// `module.Str`: a trivial subclass of `str`.
pub fn str_subclass(it: &mut Interp, module: &str) -> R<Value> {
    let attrs = it.new_dict();
    dict_set_str(&attrs, "__module__", Value::str(module));
    let bases = Value::tuple(vec![Value::Obj(it.types.str.clone())]);
    let type_fn = dict_get_str(&it.builtins, "type").unwrap_or(Value::None);
    it.call(&type_fn, vec![Value::str("Str"), bases, Value::Obj(attrs)], Vec::new())
}

/// The type whose instances keep their attributes in a dictionary of their own, which is looked
/// at before the type.
#[lumen_bind::class(module = "_testimportexec", name = "Example", hint(py(final)))]
pub struct Example {
    attrs: AttrDict,
}

impl HasAttrs for Example {
    fn attrs(&mut self) -> &mut AttrDict {
        &mut self.attrs
    }
}

#[lumen_bind::methods]
impl Example {
    #[constructor]
    fn new() -> Example {
        Example { attrs: AttrDict::default() }
    }

    #[method(hint(py(arg_style = "parse", arg_name = "demo", text_signature = "", doc = "demo() -> None")))]
    fn demo(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        demo_optional(it, args)
    }

    #[proto(getattribute)]
    fn getattribute(slf: This<&Value>, it: &mut Interp, name: &Value) -> R<Value> {
        get_attr::<Example>(&slf, it, name)
    }

    #[proto(setattr)]
    fn setattr(slf: This<&Value>, it: &mut Interp, name: &Value, value: &Value) -> R<()> {
        set_attr::<Example>(&slf, it, name, value)
    }

    #[proto(delattr)]
    fn delattr(slf: This<&Value>, it: &mut Interp, name: &Value) -> R<()> {
        del_attr::<Example>(&slf, it, name)
    }
}

#[lumen_bind::module(name = "_testmultiphase")]
pub mod _testmultiphase {
    use super::*;

    #[op(hint(py(arg_style = "parse", arg_name = "foo", text_signature = "", doc = "foo(i,j)\n\nReturn the sum of i and j.")))]
    fn foo(i: i64, j: i64) -> i64 {
        i.wrapping_add(j)
    }

    #[op(hint(py(
        arg_style = "parse",
        arg_name = "call_state_registration_func",
        text_signature = "",
        doc = "register_state(0): call PyState_FindModule()\nregister_state(1): call PyState_AddModule()\nregister_state(2): call PyState_RemoveModule()"
    )))]
    fn call_state_registration_func(it: &mut Interp, which: i64) -> R<Value> {
        match which {
            1 => Err(it.new_exc_str("SystemError", "PyState_AddModule called on module with slots")),
            2 => Err(it.new_exc_str("SystemError", "PyState_RemoveModule called on module with slots")),
            _ => Ok(Value::None),
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) -> R<()> {
        let Value::Obj(m) = m else { return Ok(()) };
        let d = it.module_dict(m);
        dict_set_str(&d, "__doc__", Value::str("Test module main"));
        let example = type_object::<Example>(it);
        dict_set_str(&d, "Example", Value::Obj(example));
        let base = it.exc_type("Exception");
        let error = new_type(it, "_testimportexec", "error", Some(&base), Layout::Exception);
        dict_set_str(&d, "error", Value::Obj(error));
        let str_type = str_subclass(it, "_testimportexec")?;
        dict_set_str(&d, "Str", str_type);
        dict_set_str(&d, "int_const", Value::Int(1969));
        dict_set_str(&d, "str_const", Value::str("something different"));
        Ok(())
    }
}

fn set_doc(it: &mut Interp, m: &Value, doc: &str) {
    let Value::Obj(m) = m else { return };
    let d = it.module_dict(m);
    dict_set_str(&d, "__doc__", Value::str(doc));
}

#[lumen_bind::module(name = "_testimportmultiple")]
pub mod _testimportmultiple {
    #[init]
    fn init(it: &mut crate::vm::Interp, m: &crate::object::Value) {
        super::set_doc(it, m, "_testimportmultiple doc");
    }
}

#[lumen_bind::module(name = "_testimportmultiple_foo")]
pub mod _testimportmultiple_foo {
    #[init]
    fn init(it: &mut crate::vm::Interp, m: &crate::object::Value) {
        super::set_doc(it, m, "_testimportmultiple_foo doc");
    }
}

#[lumen_bind::module(name = "_testimportmultiple_bar")]
pub mod _testimportmultiple_bar {
    #[init]
    fn init(it: &mut crate::vm::Interp, m: &crate::object::Value) {
        super::set_doc(it, m, "_testimportmultiple_bar doc");
    }
}
