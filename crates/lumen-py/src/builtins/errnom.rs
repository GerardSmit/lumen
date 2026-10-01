//! `errno`: the symbolic error codes of the host, generated from lumen-os's errno tables.

use crate::object::*;
use crate::vm::*;

pub fn make(it: &mut Interp) -> Obj {
    let m = it.new_module("errno");
    let d = it.module_dict(&m);
    let errorcode = it.new_dict();
    dict_set_str(&d, "errorcode", Value::Obj(errorcode.clone()));
    for (name, num) in lumen_os::errno::names() {
        dict_set_str(&d, name, Value::Int(num as i64));
        let _ = it.dict_set(&errorcode, Value::Int(num as i64), Value::str(name));
    }
    m
}
