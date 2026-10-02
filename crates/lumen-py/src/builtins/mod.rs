//! Builtin functions, types and native modules.

pub mod alias;
pub mod args;
pub mod bytesm;
pub mod contextvarsm;
pub mod csvm;
pub mod decimalm;
pub mod jsonm;
pub mod posixsubprocessm;
pub mod selectm;
pub mod cryptm;
pub mod fcntlm;
pub mod mmapm;
pub mod multiprocessingm;
pub mod pwdm;
pub mod readlinem;
pub mod resourcem;
pub mod syslogm;
pub mod termiosm;
pub mod scproxym;
pub mod signalm;
pub mod socketm;
pub mod descr;
pub mod dictm;
pub mod excgroup;
pub mod excm;
pub mod format;
pub mod funcs;
pub mod iom;
pub mod arraym;
pub mod astconv;
pub mod binasciim;
pub mod bisectm;
pub mod unicodedatam;
pub mod unraisable;
pub mod functoolsm;
pub mod genm;
pub mod bz2m;
pub mod lzmam;
pub mod pyexpatm;
pub mod zlibm;
#[cfg(all(unix, not(target_os = "android")))]
pub mod sslm;
pub mod hashlibm;
pub mod heapqm;
pub mod impm;
pub mod iterm;
pub mod itertools;
pub mod listm;
pub mod marshalm;
pub mod cmathm;
pub mod mathm;
pub mod operatorm;
pub mod picklem;
pub mod memview;
pub mod modules;
pub mod native;
pub mod numeric;
pub mod objectm;
pub mod slots;
pub mod statisticsm;
pub mod strm;
pub mod sysextra;
pub mod sysm;
pub mod sysmods;
pub mod threadm;
pub mod astm;
pub mod astnodes;
pub mod timem;
pub mod tokenizem;
pub mod weakm;
pub mod stringm;
pub mod warningsm;
pub mod collectionsm;
pub mod codecsm;
pub mod randomm;
pub mod sre;
pub mod structm;
pub mod oserror;
pub mod errnom;
pub mod posixm;
pub mod typingm;
pub mod zoneinfom;
pub mod xid;
pub mod testcapi;
pub mod interpchanm;
pub mod subinterpm;
pub mod singlephasem;
pub mod testbufferm;
pub mod testextm;
pub mod xxlimitedm;
pub mod lsprofm;
pub mod monitoringm;
pub mod opcodem;

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
    funcs::init(it);
    sysextra::init_frame_type(it);
    sysextra::init_code_type(it);
    descr::init(it);
    numeric::init_descriptors(it);
    dictm::init_descriptors(it);
    alias::init(it);
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
