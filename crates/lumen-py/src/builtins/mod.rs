//! Builtin functions, types and native modules.

pub mod alias;
pub mod args;
pub mod arraym;
pub mod astconv;
pub mod astm;
pub mod astnodes;
pub mod binasciim;
pub mod bisectm;
pub mod bytesm;
pub mod bz2m;
pub mod cmathm;
pub mod codecsm;
pub mod collectionsm;
pub mod contextvarsm;
pub mod csvm;
pub mod decimalm;
pub mod decompressor;
pub mod descr;
pub mod dictm;
pub mod errnom;
pub mod excgroup;
pub mod excm;
pub mod fcntlm;
pub mod format;
pub mod funcs;
pub mod functoolsm;
pub mod genm;
pub mod hashlibm;
pub mod heapqm;
pub mod impm;
pub mod interpchanm;
pub mod iom;
pub mod iterm;
pub mod itertools;
pub mod jsonm;
pub mod listm;
pub mod lsprofm;
pub mod lzmam;
pub mod marshalm;
pub mod mathm;
pub mod memview;
pub mod mmapm;
pub mod modules;
pub mod monitoringm;
pub mod multiprocessingm;
pub mod native;
pub mod numeric;
pub mod objectm;
pub mod opcodem;
pub mod operatorm;
pub mod oserror;
pub mod picklem;
pub mod posixm;
pub mod posixsubprocessm;
pub mod pwdm;
pub mod pyexpatm;
pub mod randomm;
pub mod readlinem;
pub mod resourcem;
pub mod scproxym;
pub mod selectm;
pub mod signalm;
pub mod singlephasem;
pub mod slots;
pub mod socketm;
pub mod sre;
#[cfg(all(unix, not(target_os = "android")))]
pub mod sslm;
pub mod statisticsm;
pub mod stringm;
pub mod strm;
pub mod structm;
pub mod subinterpm;
pub mod sysextra;
pub mod syslogm;
pub mod sysm;
pub mod sysmods;
pub mod termiosm;
pub mod testbufferm;
pub mod testcapi;
pub mod testextm;
pub mod threadm;
pub mod timem;
pub mod tokenizem;
pub mod typingm;
pub mod unicodedatam;
pub mod unraisable;
pub mod warningsm;
pub mod weakm;
pub mod xid;
pub mod xxlimitedm;
pub mod zlibm;
pub mod zoneinfom;

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
    let excs: Vec<(&'static str, Obj)> =
        it.exc_types.iter().map(|(k, v)| (*k, v.clone())).collect();
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
