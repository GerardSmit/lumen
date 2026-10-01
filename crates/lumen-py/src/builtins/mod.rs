//! Builtin functions, types and native modules.

pub mod alias;
pub mod args;
pub mod bytesm;
pub mod descr;
pub mod dictm;
pub mod excgroup;
pub mod excm;
pub mod file;
pub mod format;
pub mod funcs;
pub mod genm;
pub mod iterm;
pub mod itertools;
pub mod listm;
pub mod modules;
pub mod native;
pub mod numeric;
pub mod objectm;
pub mod slots;
pub mod strm;
pub mod sysextra;
pub mod sysmods;
pub mod weakm;
pub mod stringm;
pub mod warningsm;
pub mod collectionsm;

use crate::object::*;
use crate::vm::*;

pub fn init(it: &mut Interp) {
    objectm::init(it);
    numeric::init(it);
    strm::init(it);
    listm::init(it);
    dictm::init(it);
    bytesm::init(it);
    iterm::init(it);
    genm::init(it);
    excm::init(it);
    excgroup::init(it);
    file::init(it);
    funcs::init(it);
    sysextra::init_frame_type(it);
    descr::init(it);
    modules::init(it);
    register_names(it);
}

fn register_names(it: &mut Interp) {
    let b = it.builtins.clone();
    let t = &it.types;
    let named: Vec<(&str, Obj)> = vec![
        ("object", t.object.clone()),
        ("type", t.type_.clone()),
        ("int", t.int.clone()),
        ("bool", t.bool_.clone()),
        ("float", t.float.clone()),
        ("complex", t.complex.clone()),
        ("str", t.str_.clone()),
        ("list", t.list.clone()),
        ("tuple", t.tuple.clone()),
        ("dict", t.dict.clone()),
        ("set", t.set.clone()),
        ("frozenset", t.frozenset.clone()),
        ("bytes", t.bytes.clone()),
        ("bytearray", t.bytearray.clone()),
        ("range", t.range.clone()),
        ("slice", t.slice.clone()),
        ("property", t.property.clone()),
        ("staticmethod", t.staticmethod.clone()),
        ("classmethod", t.classmethod.clone()),
        ("super", t.super_.clone()),
        ("enumerate", t.enumerate.clone()),
        ("zip", t.zip.clone()),
        ("map", t.map.clone()),
        ("filter", t.filter.clone()),
        ("reversed", t.reversed.clone()),
    ];
    if let Some(d) = it.types.object.dict.borrow().as_ref() {
        dict_set_str(d, "__doc__", Value::str("The base class of the class hierarchy."));
    }
    for (n, o) in named {
        dict_set_str(&b, n, Value::Obj(o));
    }
    let excs: Vec<(&'static str, Obj)> = it.exc_types.iter().map(|(k, v)| (*k, v.clone())).collect();
    for (n, o) in excs {
        dict_set_str(&b, n, Value::Obj(o));
    }
    dict_set_str(&b, "None", Value::None);
    dict_set_str(&b, "True", Value::Bool(true));
    dict_set_str(&b, "False", Value::Bool(false));
    dict_set_str(&b, "Ellipsis", Value::Ellipsis);
    dict_set_str(&b, "NotImplemented", Value::NotImplemented);
    dict_set_str(&b, "__debug__", Value::Bool(true));
    dict_set_str(&b, "__name__", Value::str("builtins"));
}
