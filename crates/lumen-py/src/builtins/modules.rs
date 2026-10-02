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
    ("_io", bound::<super::iom::_io::Module>),
    ("_random", bound::<super::randomm::_random::Module>),
    ("_sha2", |it| Some(super::sha2m::make(it))),
    ("_sre", |it| Some(super::sre::make(it))),
    ("_string", |it| Some(super::stringm::make(it))),
    ("_struct", bound::<super::structm::_struct::Module>),
    ("_thread", |it| Some(super::sysmods::make_thread(it))),
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
    ("sys", |it| Some(make_sys(it))),
    ("time", |it| Some(make_time(it))),
];

pub fn builtin_module(it: &mut Interp, name: &str) -> Option<Obj> {
    let (_, make) = BUILTIN_MODULES.iter().find(|(n, _)| *n == name)?;
    make(it)
}

pub fn init(it: &mut Interp) {
    let sys = make_sys(it);
    it.register_module("sys", &sys);
    super::iom::init_std_streams(it, &sys);
}

fn set_fn(it: &mut Interp, d: &Obj, name: &'static str, f: NativeFn) {
    let v = it.new_native(name, f, false);
    dict_set_str(d, name, v);
}

fn make_sys(it: &mut Interp) -> Obj {
    if let Some(Value::Obj(m)) = dict_get_str(&it.modules, "sys") {
        return m;
    }
    let m = it.new_module("sys");
    let d = it.module_dict(&m);
    it.sys_module = Some(m.clone());
    dict_set_str(&d, "modules", Value::Obj(it.modules.clone()));
    dict_set_str(&d, "argv", Value::list(vec![Value::str("")]));
    dict_set_str(&d, "path", Value::list(vec![Value::str(crate::frozen::FROZEN_DIR)]));
    dict_set_str(&d, "maxsize", Value::Int(i64::MAX));
    dict_set_str(&d, "maxunicode", Value::Int(0x10ffff));
    dict_set_str(&d, "byteorder", Value::str("little"));
    dict_set_str(&d, "version", Value::str("3.12.15 (lumen-py)"));
    dict_set_str(&d, "hexversion", Value::Int(0x030c0ff0));
    let (platform, executable, argv) = {
        let p = it.platform.borrow();
        (p.platform_name(), p.executable(), p.argv())
    };
    dict_set_str(&d, "platform", Value::str(&platform));
    dict_set_str(&d, "executable", Value::str(&executable));
    if !argv.is_empty() {
        dict_set_str(&d, "argv", Value::list(argv.iter().map(|a| Value::str(a)).collect()));
        it.argv = argv;
    }
    dict_set_str(&d, "builtin_module_names", Value::tuple(BUILTIN_MODULES.iter().map(|(n, _)| Value::str(n)).collect()));
    set_fn(it, &d, "exit", sys_exit);
    set_fn(it, &d, "getrecursionlimit", sys_getrecursionlimit);
    set_fn(it, &d, "setrecursionlimit", sys_setrecursionlimit);
    set_fn(it, &d, "exc_info", sys_exc_info);
    set_fn(it, &d, "intern", sys_intern);
    set_fn(it, &d, "get_int_max_str_digits", sys_get_int_max_str_digits);
    set_fn(it, &d, "set_int_max_str_digits", sys_set_int_max_str_digits);
    super::sysextra::init_sys(it, &d);
    m
}

fn sys_get_int_max_str_digits(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("get_int_max_str_digits", a, 0, 0)?;
    Ok(Value::Int(it.int_max_str_digits() as i64))
}

fn sys_set_int_max_str_digits(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("set_int_max_str_digits", a, kw, &["maxdigits"], 1)?;
    let n = it.index_of(b[0].as_ref().unwrap_or(&Value::None))?;
    if n < 0 || !it.set_int_max_str_digits(n as usize) {
        return Err(it.value_error(&format!("maxdigits must be >= {} or 0 for unlimited", crate::limits::INT_MAX_STR_DIGITS_THRESHOLD)));
    }
    // sys.flags is built lazily and cached; drop it so it reflects the new limit, as CPython does.
    if let Some(m) = it.sys_module.clone() {
        let d = it.module_dict(&m);
        dict_del_str(&d, "flags");
    }
    Ok(Value::None)
}

fn sys_exit(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("exit", a, 0, 1)?;
    let cls = it.exc_type("SystemExit");
    Err(it.new_exc(&cls, a.to_vec()))
}

fn sys_getrecursionlimit(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Int(it.recursion_limit as i64))
}

fn sys_setrecursionlimit(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("setrecursionlimit", a, 1, 1)?;
    let n = it.index_of(&a[0])?;
    if n < 1 {
        return Err(it.value_error("recursion limit must be greater or equal than 1"));
    }
    it.recursion_limit = (n as usize).min(200_000);
    Ok(Value::None)
}

fn sys_exc_info(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(match it.handled.clone() {
        Some(e) => {
            let t = Value::Obj(it.type_of_obj(&e));
            let tb = match &e.kind {
                Kind::Exception(d) => it.make_tb(&d.borrow().tb),
                _ => Value::None,
            };
            Value::tuple(vec![t, Value::Obj(e), tb])
        }
        None => Value::tuple(vec![Value::None, Value::None, Value::None]),
    })
}

fn sys_intern(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("intern", a, 1, 1)?;
    Ok(a[0].clone())
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
