//! Native modules: `sys` and `time`.

use crate::object::*;
use crate::vm::*;

type Kw<'a> = &'a [(Obj, Value)];

type MakeModule = fn(&mut Interp) -> Option<Obj>;

fn bound<M: lumen_bind::Module<crate::bind::PyHost>>(it: &mut Interp) -> Option<Obj> {
    crate::bind::module_object::<M>(it).ok()
}

/// The native modules, sorted by name (also `sys.builtin_module_names`).
const BUILTIN_MODULES: &[(&str, MakeModule)] = &[
    ("_codecs", |it| Some(super::codecsm::make(it))),
    ("_collections", |it| Some(super::collectionsm::make(it))),
    ("_contextvars", bound::<super::contextvarsm::_contextvars::Module>),
    ("_io", bound::<super::iom::_io::Module>),
    ("_random", bound::<super::randomm::_random::Module>),
    ("_sha2", |it| Some(super::sha2m::make(it))),
    ("_sre", |it| Some(super::sre::make(it))),
    ("_string", |it| Some(super::stringm::make(it))),
    ("_struct", bound::<super::structm::_struct::Module>),
    ("_thread", bound::<super::threadm::_thread::Module>),
    ("_tokenize", bound::<super::tokenizem::_tokenize::Module>),
    ("_typing", |it| Some(super::typingm::make(it))),
    ("_warnings", |it| Some(super::warningsm::make(it))),
    ("_weakref", |it| Some(super::weakm::make(it))),
    ("atexit", |it| Some(super::sysmods::make_atexit(it))),
    ("builtins", |it| Some(super::sysmods::make_builtins(it))),
    ("errno", bound::<super::errnom::errno::Module>),
    ("gc", |it| Some(super::sysmods::make_gc(it))),
    ("itertools", bound::<super::itertools::itertools::Module>),
    ("math", bound::<super::mathm::math::Module>),
    ("posix", bound::<super::posixm::posix::Module>),
    ("sys", |it| it.sys_module.clone()),
    ("time", |it| Some(make_time(it))),
];

pub fn builtin_module(it: &mut Interp, name: &str) -> Option<Obj> {
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

fn set_fn(it: &mut Interp, d: &Obj, name: &'static str, f: NativeFn) {
    let v = it.new_native(name, f, false);
    dict_set_str(d, name, v);
}

// ---- time ---------------------------------------------------------------------------------------

fn t_time(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    let ns = it.platform.borrow().wall_time_ns();
    Ok(Value::Float((ns / 1_000_000_000) as f64 + (ns % 1_000_000_000) as f64 / 1e9))
}

fn t_time_ns(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(it.platform.borrow().wall_time_ns() as i64))
}

fn elapsed_ns(it: &Interp) -> u64 {
    it.platform.borrow().monotonic_ns().saturating_sub(it.start_ns)
}

fn t_perf(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    let ns = elapsed_ns(it);
    Ok(Value::Float((ns / 1_000_000_000) as f64 + (ns % 1_000_000_000) as f64 / 1e9 + 1000.0))
}

fn t_perf_ns(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(elapsed_ns(it) as i64 + 1_000_000_000_000))
}

fn t_sleep(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("sleep", a, 1, 1)?;
    let s = it.float_arg(&a[0])?;
    if s < 0.0 {
        return Err(it.value_error("sleep length must be non-negative"));
    }
    it.flush_out();
    let deadline = it.platform.borrow().monotonic_ns().saturating_add((s.min(1e9) * 1e9) as u64);
    loop {
        it.poll()?;
        let left = deadline.saturating_sub(it.platform.borrow().monotonic_ns());
        if left == 0 {
            return Ok(Value::None);
        }
        // Sleep in slices so an interrupt is noticed promptly.
        it.platform.borrow_mut().sleep(left.min(20_000_000) as f64 / 1e9);
    }
}

fn make_time(it: &mut Interp) -> Obj {
    let m = it.new_module("time");
    let d = it.module_dict(&m);
    let fns: &[(&'static str, NativeFn)] = &[
        ("time", t_time),
        ("time_ns", t_time_ns),
        ("perf_counter", t_perf),
        ("monotonic", t_perf),
        ("process_time", t_perf),
        ("perf_counter_ns", t_perf_ns),
        ("monotonic_ns", t_perf_ns),
        ("sleep", t_sleep),
    ];
    for (n, f) in fns {
        set_fn(it, &d, n, *f);
    }
    m
}
