//! `xxlimited` and `xxlimited_35`: the template extension modules built with the limited C API
//! (3.12 and 3.5 flavours): an `Xxo` type that keeps its attributes in a dictionary of its
//! own, `Str`, an error type and `foo`/`new`; `xxlimited` adds the buffer interface and
//! `xxlimited_35` adds `Null` and `roj`.

use super::testextm::{del_attr, demo_optional, get_attr, set_attr, str_subclass, AttrDict, Example, HasAttrs};
use crate::bind::{type_object, This, KwArgs};
use crate::builtins::memview::view_of_store;
use crate::builtins::native::{new_type, with_opaque};
use crate::object::*;
use crate::vm::{dict_set_str, Interp};
use lumen_common::buffer::ByteStore;
use std::rc::Rc;

const BUFSIZE: usize = 10;

/// `xxlimited.Xxo`: attributes in a dictionary of its own and a 10-byte buffer.
#[lumen_bind::class(module = "xxlimited", name = "Xxo", hint(py(final)))]
pub struct Xxo {
    attrs: AttrDict,
    buffer: Rc<ByteStore>,
}

impl HasAttrs for Xxo {
    fn attrs(&mut self) -> &mut AttrDict {
        &mut self.attrs
    }
}

#[lumen_bind::methods]
impl Xxo {
    #[constructor]
    fn new() -> Xxo {
        Xxo { attrs: AttrDict::default(), buffer: Rc::new(ByteStore::zeroed(BUFSIZE)) }
    }

    #[method(hint(py(arg_style = "parse", arg_name = "demo", text_signature = "", doc = "demo(o) -> o")))]
    fn demo(slf: This<&Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<Value> {
        if !kwargs.is_empty() {
            return Err(it.type_error("demo() takes no keyword arguments"));
        }
        let [o] = args else {
            return Err(it.type_error("demo() takes exactly 1 argument"));
        };
        let cls = it.type_of(&slf);
        if o.as_str().is_some() || it.isinstance_value(o, &Value::Obj(cls))? {
            Ok(o.clone())
        } else {
            Ok(Value::None)
        }
    }

    /// Return the number of times an internal buffer is exported.
    #[getter(name = "x_exports")]
    fn x_exports(&self) -> i64 {
        self.buffer.pins() as i64
    }

    #[method(name = "__buffer__")]
    fn buffer(slf: This<&Value>, it: &mut Interp, flags: &Value) -> R<Value> {
        let _ = flags;
        let store = with_opaque::<Xxo, _>(&slf, |x| x.buffer.clone());
        match store {
            Some(store) => view_of_store(it, slf.0.clone(), &store),
            None => Err(it.type_error("descriptor requires an 'xxlimited.Xxo' object")),
        }
    }

    #[method(name = "__release_buffer__")]
    fn release_buffer(&self, view: &Value) {
        let _ = view;
    }

    #[proto(getattribute)]
    fn getattribute(slf: This<&Value>, it: &mut Interp, name: &Value) -> R<Value> {
        get_attr::<Xxo>(&slf, it, name)
    }

    #[proto(setattr)]
    fn setattr(slf: This<&Value>, it: &mut Interp, name: &Value, value: &Value) -> R<()> {
        set_attr::<Xxo>(&slf, it, name, value)
    }

    #[proto(delattr)]
    fn delattr(slf: This<&Value>, it: &mut Interp, name: &Value) -> R<()> {
        del_attr::<Xxo>(&slf, it, name)
    }
}

fn new_instance(it: &mut Interp, module: &str) -> R<Value> {
    let m = it.import_module(module)?;
    let ty = it.get_attr_str(&Value::Obj(m), "Xxo")?;
    it.call(&ty, Vec::new(), Vec::new())
}

#[lumen_bind::module(name = "xxlimited")]
pub mod xxlimited {
    use super::*;

    #[op(hint(py(arg_style = "parse", arg_name = "foo", text_signature = "", doc = "foo(i,j)\n\nReturn the sum of i and j.")))]
    fn foo(i: i64, j: i64) -> i64 {
        i.wrapping_add(j)
    }

    #[op(hint(py(text_signature = "", doc = "new() -> new Xx object")))]
    fn new(it: &mut Interp) -> R<Value> {
        new_instance(it, "xxlimited")
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) -> R<()> {
        let Value::Obj(m) = m else { return Ok(()) };
        let d = it.module_dict(m);
        dict_set_str(&d, "__doc__", Value::str("This is a template module just for instruction."));
        let base = it.exc_type("Exception");
        let error = new_type(it, "xxlimited", "Error", Some(&base), Layout::Exception);
        dict_set_str(&d, "Error", Value::Obj(error));
        let xxo = type_object::<Xxo>(it);
        if let Some(td) = xxo.dict.borrow().as_ref() {
            dict_set_str(td, "__doc__", Value::str("A class that explicitly stores attributes in an internal dict"));
        }
        dict_set_str(&d, "Xxo", Value::Obj(xxo));
        let str_type = str_subclass(it, "xxlimited")?;
        dict_set_str(&d, "Str", str_type);
        Ok(())
    }
}

#[lumen_bind::module(name = "xxlimited_35")]
pub mod xxlimited_35 {
    use super::*;

    #[op(hint(py(arg_style = "parse", arg_name = "foo", text_signature = "", doc = "foo(i,j)\n\nReturn the sum of i and j.")))]
    fn foo(i: i64, j: i64) -> i64 {
        i.wrapping_add(j)
    }

    #[op(hint(py(arg_style = "parse", arg_name = "new", text_signature = "", doc = "new() -> new Xx object")))]
    fn new(it: &mut Interp) -> R<Value> {
        new_instance(it, "xxlimited_35")
    }

    #[op(hint(py(arg_style = "parse", arg_name = "roj", text_signature = "", doc = "roj(a,b) -> None")))]
    fn roj(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = args;
        Err(it.new_exc_str("SystemError", "PY_SSIZE_T_CLEAN macro must be defined for '#' formats"))
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) -> R<()> {
        let Value::Obj(m) = m else { return Ok(()) };
        let d = it.module_dict(m);
        dict_set_str(&d, "__doc__", Value::str("This is a module for testing limited API from Python 3.5."));
        let base = it.exc_type("Exception");
        let error = new_type(it, "xxlimited_35", "error", Some(&base), Layout::Exception);
        dict_set_str(&d, "error", Value::Obj(error));
        let xxo = new_type(it, "xxlimited_35", "Xxo", None, Layout::Other);
        crate::bind::install_all::<Example>(&xxo);
        if let Some(td) = xxo.dict.borrow().as_ref() {
            dict_set_str(td, "__doc__", Value::str("The Xxo type"));
        }
        dict_set_str(&d, "Xxo", Value::Obj(xxo));
        let str_type = str_subclass(it, "xxlimited_35")?;
        dict_set_str(&d, "Str", str_type);
        let attrs = it.new_dict();
        dict_set_str(&attrs, "__module__", Value::str("xxlimited_35"));
        let type_fn = crate::vm::dict_get_str(&it.builtins, "type").unwrap_or(Value::None);
        let object = Value::Obj(it.types.object.clone());
        let null = it.call(&type_fn, vec![Value::str("Null"), Value::tuple(vec![object]), Value::Obj(attrs)], Vec::new())?;
        dict_set_str(&d, "Null", null);
        Ok(())
    }
}
