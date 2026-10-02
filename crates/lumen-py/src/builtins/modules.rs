//! The table of native modules.

use crate::object::*;
use crate::vm::*;


type MakeModule = fn(&mut Interp) -> Option<Obj>;

fn bound<M: lumen_bind::Module<crate::bind::PyHost>>(it: &mut Interp) -> Option<Obj> {
    crate::bind::module_object::<M>(it).ok()
}

/// The native modules, sorted by name (also `sys.builtin_module_names`).
const BUILTIN_MODULES: &[(&str, MakeModule)] = &[
    ("_ast", bound::<super::astm::_ast::Module>),
    ("_bisect", bound::<super::bisectm::_bisect::Module>),
    ("_blake2", bound::<super::hashlibm::_blake2::Module>),
    ("_codecs", |it| Some(super::codecsm::make(it))),
    ("_collections", |it| Some(super::collectionsm::make(it))),
    ("_contextvars", bound::<super::contextvarsm::_contextvars::Module>),
    ("_csv", bound::<super::csvm::_csv::Module>),
    ("_hashlib", bound::<super::hashlibm::_hashlib::Module>),
    ("_heapq", bound::<super::heapqm::_heapq::Module>),
    ("_imp", bound::<super::impm::_imp::Module>),
    ("_io", bound::<super::iom::_io::Module>),
    ("_md5", bound::<super::hashlibm::_md5::Module>),
    ("_posixsubprocess", bound::<super::posixsubprocessm::_posixsubprocess::Module>),
    ("_random", bound::<super::randomm::_random::Module>),
    ("_scproxy", bound::<super::scproxym::_scproxy::Module>),
    ("_sha1", bound::<super::hashlibm::_sha1::Module>),
    ("_sha2", bound::<super::hashlibm::_sha2::Module>),
    ("_sha3", bound::<super::hashlibm::_sha3::Module>),
    ("_signal", bound::<super::signalm::_signal::Module>),
    ("_socket", bound::<super::socketm::_socket::Module>),
    ("_sre", |it| Some(super::sre::make(it))),
    ("_string", |it| Some(super::stringm::make(it))),
    ("_struct", bound::<super::structm::_struct::Module>),
    ("_thread", bound::<super::threadm::_thread::Module>),
    ("_tokenize", bound::<super::tokenizem::_tokenize::Module>),
    ("_typing", |it| Some(super::typingm::make(it))),
    ("_warnings", |it| Some(super::warningsm::make(it))),
    ("_weakref", |it| Some(super::weakm::make(it))),
    ("_zoneinfo", bound::<super::zoneinfom::_zoneinfo::Module>),
    ("array", bound::<super::arraym::array::Module>),
    ("atexit", |it| Some(super::sysmods::make_atexit(it))),
    ("binascii", bound::<super::binasciim::binascii::Module>),
    ("builtins", |it| Some(super::sysmods::make_builtins(it))),
    ("cmath", bound::<super::cmathm::cmath::Module>),
    ("errno", bound::<super::errnom::errno::Module>),
    ("gc", |it| Some(super::sysmods::make_gc(it))),
    ("itertools", bound::<super::itertools::itertools::Module>),
    ("marshal", bound::<super::marshalm::marshal::Module>),
    ("math", bound::<super::mathm::math::Module>),
    ("posix", bound::<super::posixm::posix::Module>),
    ("select", bound::<super::selectm::select::Module>),
    ("sys", |it| it.sys_module.clone()),
    ("time", bound::<super::timem::time::Module>),
    ("unicodedata", bound::<super::unicodedatam::unicodedata::Module>),
    ("zlib", bound::<super::zlibm::zlib::Module>),
];

pub fn builtin_module(it: &mut Interp, name: &str) -> Option<Obj> {
    if name.starts_with("_sysconfigdata_") {
        return Some(sysconfig_data(it, name));
    }
    let (_, make) = BUILTIN_MODULES.iter().find(|(n, _)| *n == name)?;
    make(it)
}

pub fn init(it: &mut Interp) {
    let Ok(sys) = crate::bind::module_object::<super::sysm::sys::Module>(it) else { return };
    super::iom::init_std_streams(it, &sys);
}

/// `sys.builtin_module_names`.
pub fn builtin_module_names() -> Vec<&'static str> {
    BUILTIN_MODULES.iter().map(|(n, _)| *n).collect()
}

/// `_sysconfigdata_*`, the build-time configuration `sysconfig` reads (generated when CPython is
/// built).
fn sysconfig_data(it: &mut Interp, name: &str) -> Obj {
    let m = it.new_module(name);
    let d = it.module_dict(&m);
    let platform = it.platform.borrow().platform_name();
    let vars = it.new_dict();
    let strs = [
        ("TZPATH", "/usr/share/zoneinfo:/usr/lib/zoneinfo:/usr/share/lib/zoneinfo:/etc/zoneinfo".to_string()),
        ("VERSION", "3.12".to_string()),
        ("ABIFLAGS", String::new()),
        ("MACHDEP", platform.clone()),
        ("SOABI", format!("cpython-312-{platform}")),
        ("EXT_SUFFIX", format!(".cpython-312-{platform}.so")),
        ("SHLIB_SUFFIX", ".so".to_string()),
    ];
    for (k, v) in strs {
        dict_set_str(&vars, k, Value::string(v));
    }
    for (k, v) in [("Py_DEBUG", 0), ("Py_ENABLE_SHARED", 0), ("WITH_DOC_STRINGS", 1), ("SIZEOF_VOID_P", 8)] {
        dict_set_str(&vars, k, Value::Int(v));
    }
    dict_set_str(&d, "build_time_vars", Value::Obj(vars));
    m
}
