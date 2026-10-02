//! `_testcapi.testBuf` (`buffer.c`, PEP 688), `_testcapi.ObjExtraData` (`gc.c`, an object with
//! one hidden slot behind the `extra` attribute) and `unicode_legacy_string`.

use super::name_obj;
use crate::bind::{type_object, Py, This};
use crate::builtins::memview::{view_from_parts, Source};
use crate::builtins::native::with_opaque;
use crate::object::*;
use crate::vm::Interp;
use lumen_common::buffer::ViewDesc;

/// `testBufType`: exports the bytes `b"test"` and counts the exports not yet released.
#[lumen_bind::class(module = "builtins", name = "testBufType")]
pub struct TestBuf {
    obj: Obj,
    references: i64,
}

#[lumen_bind::methods]
impl TestBuf {
    #[constructor]
    fn new() -> TestBuf {
        let Value::Obj(obj) = Value::bytes(b"test".to_vec()) else { unreachable!("bytes are objects") };
        TestBuf { obj, references: 0 }
    }

    #[getter]
    fn references(&self) -> i64 {
        self.references
    }

    #[method(name = "__buffer__")]
    fn buffer(slf: This<Py<Self>>, it: &mut Interp, flags: &Value) -> R<Value> {
        let _ = flags;
        let obj = {
            let mut s = slf.0.borrow_mut(it)?;
            s.references += 1;
            s.obj.clone()
        };
        let len = match &obj.kind {
            Kind::Bytes(b) => b.len(),
            _ => 0,
        };
        Ok(view_from_parts(it, slf.0.value().clone(), Source::Bytes(obj), ViewDesc::bytes(0, len, true), "B".into()))
    }

    #[method(name = "__release_buffer__")]
    fn release_buffer(slf: This<Py<Self>>, it: &mut Interp, view: &Value) -> R<()> {
        it.get_attr_str(view, "nbytes")?;
        slf.0.borrow_mut(it)?.references -= 1;
        it.call_method(view, "release", Vec::new())?;
        Ok(())
    }
}

/// `ObjExtraData`: instances carry one extra object in storage that follows the object itself.
#[lumen_bind::class(module = "builtins", name = "ObjExtraData")]
pub struct ObjExtraData {
    extra: Option<Value>,
}

const EXTRA: &str = "extra";

fn is_extra(name: &Obj) -> bool {
    Value::Obj(name.clone()).as_str() == Some(EXTRA)
}

#[lumen_bind::methods]
impl ObjExtraData {
    #[constructor]
    fn new() -> ObjExtraData {
        ObjExtraData { extra: None }
    }

    #[getter]
    fn extra(&self) -> Value {
        self.extra.clone().unwrap_or(Value::None)
    }

    #[proto(setattr)]
    fn setattr(slf: This<&Value>, it: &mut Interp, name: &Value, value: &Value) -> R<()> {
        let n = name_obj(it, name)?;
        if is_extra(&n) {
            with_opaque::<ObjExtraData, _>(&slf, |s| s.extra = Some(value.clone()));
            return Ok(());
        }
        let cls = it.type_of(&slf);
        it.generic_setattr(&slf, &cls, &n, value.clone())
    }

    #[proto(delattr)]
    fn delattr(slf: This<&Value>, it: &mut Interp, name: &Value) -> R<()> {
        let n = name_obj(it, name)?;
        if is_extra(&n) {
            with_opaque::<ObjExtraData, _>(&slf, |s| s.extra = None);
            return Ok(());
        }
        let cls = it.type_of(&slf);
        it.generic_delattr(&slf, &cls, &n)
    }
}

#[lumen_bind::module(name = "_testcapi")]
pub mod extram {
    use super::*;

    /// unicode_legacy_string(str): a copy of the string in the legacy (not ready) representation.
    #[op]
    fn unicode_legacy_string(it: &mut Interp, s: &Value) -> R<Value> {
        match s.as_str() {
            Some(text) => Ok(Value::string(text.to_string())),
            None => {
                let t = it.type_name_of(s);
                Err(it.type_error(&format!("argument must be str, not {t}")))
            }
        }
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        crate::vm::dict_set_str(&d, "testBuf", Value::Obj(type_object::<TestBuf>(it)));
        crate::vm::dict_set_str(&d, "ObjExtraData", Value::Obj(type_object::<ObjExtraData>(it)));
    }
}
