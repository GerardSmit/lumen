//! `_testcapi` wrappers of the evaluation, frame, function, code, sys, run and import APIs
//! (`eval.c`, `run.c`, `sys.c`, `import.c` and the matching parts of `_testcapimodule.c`).
//! `None` stands for a `NULL` pointer.

use super::{bad_internal_call, builtin, call_builtin, system_error};
use crate::builtins::funcs::{frame_globals, locals_value};
use crate::builtins::sysextra::frame_generator;
use crate::bytecode::{Code, Op};
use crate::object::*;
use crate::vm::{dict_del_str, dict_get_str, dict_set_str, Interp};
use std::cell::RefCell;
use std::rc::Rc;

const PY_SINGLE_INPUT: i64 = 256;
const PY_FILE_INPUT: i64 = 257;
const PY_EVAL_INPUT: i64 = 258;

fn is_frame(it: &Interp, v: &Value) -> bool {
    matches!(v, Value::Obj(o) if Rc::ptr_eq(&it.type_of_obj(o), &it.types.frame))
}

fn frame_arg(it: &mut Interp, v: &Value) -> R<()> {
    if is_frame(it, v) {
        Ok(())
    } else {
        Err(it.type_error("argument must be a frame"))
    }
}

fn function_arg(it: &mut Interp, v: &Value) -> R<Obj> {
    match v {
        Value::Obj(o) if matches!(o.kind, Kind::Function(_)) => Ok(o.clone()),
        _ => Err(bad_internal_call(it)),
    }
}

/// A `z#` argument: `str` as is, `bytes` decoded as UTF-8 (a bad byte is a `UnicodeDecodeError`).
pub(super) fn c_string(it: &mut Interp, v: &Value) -> R<Option<String>> {
    match v {
        Value::None => Ok(None),
        Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => Ok(v.as_str().map(str::to_string)),
        _ => {
            let bytes = Value::bytes(it.bytes_from_object(v)?);
            let s = it.call_method(&bytes, "decode", vec![Value::str("utf-8")])?;
            Ok(s.as_str().map(str::to_string))
        }
    }
}

pub(super) fn required_c_string(it: &mut Interp, v: &Value) -> R<String> {
    match c_string(it, v)? {
        Some(s) => Ok(s),
        None => Err(it.type_error("argument must be str or bytes, not None")),
    }
}

fn sys_dict(it: &mut Interp) -> Option<Obj> {
    let m = it.sys_module.clone()?;
    Some(it.module_dict(&m))
}

fn func_name(it: &mut Interp, func: &Value) -> R<String> {
    let named = matches!(func, Value::Obj(o) if matches!(o.kind, Kind::Function(_) | Kind::Method(..) | Kind::Native(_)));
    if named {
        let n = it.get_attr_str(func, "__name__")?;
        return Ok(n.as_str().unwrap_or("").to_string());
    }
    Ok(it.tp_name_of(func))
}

fn new_empty_code(filename: &str, name: &str, first_line: i64) -> Rc<Code> {
    let line = first_line.max(0) as u32;
    Rc::new(Code {
        name: name.into(),
        qualname: name.into(),
        filename: filename.into(),
        first_line: line,
        ops: vec![Op::LoadConst(0), Op::ReturnValue],
        lines: vec![line, line],
        consts: vec![Value::None],
        names: Vec::new(),
        varnames: Vec::new(),
        cellvars: Vec::new(),
        freevars: Vec::new(),
        argcount: 0,
        posonly: 0,
        kwonly: 0,
        flags: 0,
        cell_args: Vec::new(),
        doc: None,
    })
}

fn is_dict(v: &Value) -> bool {
    matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Dict(_)))
}

/// Compiles `source` for `start` and runs it, as `PyRun_StringFlags` does.
fn run_source(it: &mut Interp, source: Value, filename: &str, start: i64, globals: &Value, locals: Option<&Value>, cf_flags: i64) -> R<Value> {
    let mode = match start {
        PY_SINGLE_INPUT => "single",
        PY_FILE_INPUT => "exec",
        PY_EVAL_INPUT => "eval",
        _ => return Err(it.value_error("invalid start symbol")),
    };
    let code = call_builtin(it, "compile", vec![source, Value::str(filename), Value::str(mode), Value::Int(cf_flags)])?;
    let mut args = vec![code, globals.clone()];
    if let Some(l) = locals {
        args.push(l.clone());
    }
    if start == PY_EVAL_INPUT {
        call_builtin(it, "eval", args)
    } else {
        call_builtin(it, "exec", args)?;
        Ok(Value::None)
    }
}

fn import_args(it: &mut Interp, globals: &Value, locals: &Value, fromlist: &Value) -> Vec<Value> {
    let _ = it;
    vec![globals.clone(), locals.clone(), fromlist.clone()]
}

fn import_level(it: &mut Interp, name: &Value, globals: &Value, locals: &Value, fromlist: &Value, level: i64) -> R<Value> {
    let mut args = vec![name.clone()];
    args.extend(import_args(it, globals, locals, fromlist));
    args.push(Value::Int(level));
    call_builtin(it, "__import__", args)
}

fn get_module(it: &mut Interp, name: &Value) -> R<Option<Value>> {
    let modules = match sys_dict(it).and_then(|d| dict_get_str(&d, "modules")) {
        Some(m) => m,
        None => return Ok(None),
    };
    let Value::Obj(m) = &modules else { return Ok(None) };
    it.dict_get(m, name)
}

fn add_module(it: &mut Interp, name: &Value) -> R<Value> {
    if let Some(m) = get_module(it, name)? {
        return Ok(m);
    }
    let ty = it.types.module.clone();
    let m = it.call(&Value::Obj(ty), vec![name.clone()], Vec::new())?;
    let modules = match sys_dict(it).and_then(|d| dict_get_str(&d, "modules")) {
        Some(Value::Obj(m)) => m,
        _ => return Err(it.new_exc_str("RuntimeError", "unable to get sys.modules")),
    };
    it.dict_set(&modules, name.clone(), m.clone())?;
    Ok(m)
}

fn exec_code_module(it: &mut Interp, name: &Value, code: &Value, pathname: Option<&Value>, cpathname: Option<&Value>) -> R<Value> {
    if !matches!(code, Value::Obj(o) if matches!(o.kind, Kind::Code(_))) {
        return Err(bad_internal_call(it));
    }
    let module = add_module(it, name)?;
    let d = it.module_dict(match &module {
        Value::Obj(o) => o,
        _ => return Err(bad_internal_call(it)),
    });
    if dict_get_str(&d, "__builtins__").is_none() {
        dict_set_str(&d, "__builtins__", Value::Obj(it.builtins.clone()));
    }
    let pathname = match pathname {
        Some(p) if !p.is_none() => Some(p.clone()),
        _ => None,
    };
    let cpathname = cpathname.filter(|p| !p.is_none()).cloned();
    if pathname.is_some() || cpathname.is_some() {
        let ext = it.import_module("importlib._bootstrap_external")?;
        let fix = it.get_attr_str(&Value::Obj(ext), "_fix_up_module")?;
        let path = pathname.clone().unwrap_or_else(|| cpathname.clone().unwrap_or(Value::None));
        let cpath = cpathname.unwrap_or(Value::None);
        it.call(&fix, vec![Value::Obj(d.clone()), name.clone(), path, cpath], Vec::new())?;
    }
    call_builtin(it, "exec", vec![code.clone(), Value::Obj(d)])?;
    match get_module(it, name)? {
        Some(m) => Ok(m),
        None => {
            let n = it.repr_of(name)?;
            Err(it.new_exc_str("ImportError", &format!("Loaded module {n} not found in sys.modules")))
        }
    }
}

#[lumen_bind::module(name = "_testcapi")]
pub mod evalm {
    use super::*;

    // ---- evaluation -----------------------------------------------------------------------------

    /// eval_get_func_name(func): `PyEval_GetFuncName`.
    #[op]
    fn eval_get_func_name(it: &mut Interp, func: &Value) -> R<String> {
        func_name(it, func)
    }

    /// eval_get_func_desc(func): `PyEval_GetFuncDesc`.
    #[op]
    fn eval_get_func_desc(func: &Value) -> &'static str {
        match func {
            Value::Obj(o) if matches!(o.kind, Kind::Function(_) | Kind::Method(..) | Kind::Native(_)) => "()",
            _ => " object",
        }
    }

    #[op]
    fn eval_getlocals(it: &mut Interp) -> Value {
        locals_value(it)
    }

    #[op]
    fn eval_getglobals(it: &mut Interp) -> Value {
        Value::Obj(frame_globals(it))
    }

    #[op]
    fn eval_getbuiltins(it: &mut Interp) -> Value {
        Value::Obj(it.builtins.clone())
    }

    #[op]
    fn eval_getframe(it: &mut Interp) -> Value {
        match it.frames.len() {
            0 => Value::None,
            n => it.frame_object(n - 1),
        }
    }

    #[op]
    fn eval_get_recursion_limit(it: &mut Interp) -> i64 {
        it.recursion_limit as i64
    }

    #[op]
    fn eval_set_recursion_limit(it: &mut Interp, limit: i64) {
        it.recursion_limit = (limit.max(1) as usize).min(200_000);
    }

    /// eval_code_ex(code, globals, locals=None, args=None, kwargs=None, defaults=None, kw_defaults=None, closure=None)
    #[op(hint(py(arg_style = "parse", arg_name = "eval_code_ex")))]
    #[allow(clippy::too_many_arguments)]
    fn eval_code_ex(
        it: &mut Interp,
        code: &Value,
        globals: &Value,
        locals: Option<&Value>,
        args: Option<&Value>,
        kwargs: Option<&Value>,
        defaults: Option<&Value>,
        kw_defaults: Option<&Value>,
        closure: Option<&Value>,
    ) -> R<Value> {
        let Value::Obj(code_obj) = code else { return Err(bad_internal_call(it)) };
        let Kind::Code(c) = &code_obj.kind else { return Err(bad_internal_call(it)) };
        let c = c.clone();
        let g = match globals {
            Value::Obj(g) if matches!(g.kind, Kind::Dict(_)) => g.clone(),
            _ => return Err(system_error(it, "PyEval_EvalCodeEx: globals must be a dict")),
        };
        let tuple_of = |it: &mut Interp, v: Option<&Value>, what: &str| -> R<Vec<Value>> {
            match v {
                None => Ok(Vec::new()),
                Some(t) => match t.tuple_items() {
                    Some(items) => Ok(items.to_vec()),
                    None => {
                        let n = it.type_name_of(t);
                        Err(it.type_error(&format!("eval_code_ex() argument {what} must be tuple, not {n}")))
                    }
                },
            }
        };
        let args = tuple_of(it, args.filter(|a| !a.is_none()), "4")?;
        let defaults = tuple_of(it, defaults.filter(|a| !a.is_none()), "6")?;
        let kwargs = match kwargs.filter(|k| !k.is_none()) {
            Some(k) if is_dict(k) => it.dict_to_kwargs(k)?,
            Some(k) => {
                let n = it.type_name_of(k);
                return Err(it.type_error(&format!("eval_code_ex() argument 5 must be dict, not {n}")));
            }
            None => Vec::new(),
        };
        let is_function = c.name.as_ref() != "<module>" && !c.has(crate::bytecode::CO_CLASS_BODY);
        if !is_function {
            let l = match locals.filter(|l| !l.is_none()) {
                Some(Value::Obj(l)) if matches!(l.kind, Kind::Dict(_)) || it.lookup_mro(&it.type_of_obj(l), "__getitem__").is_some() => l.clone(),
                Some(_) => return Err(it.type_error("locals must be a mapping")),
                None => g.clone(),
            };
            if dict_get_str(&g, "__builtins__").is_none() {
                dict_set_str(&g, "__builtins__", Value::Obj(it.builtins.clone()));
            }
            return it.run_code(c, g, l);
        }
        let kwdefaults = match kw_defaults.filter(|k| !k.is_none()) {
            Some(d) if is_dict(d) => it.dict_to_kwargs(d)?,
            Some(_) => return Err(system_error(it, "non-dict keyword only default args")),
            None => Vec::new(),
        };
        let closure: Vec<Obj> = match closure.filter(|c| !c.is_none()) {
            Some(cl) => match cl.tuple_items() {
                Some(items) => items.iter().filter_map(|v| v.as_obj().cloned()).collect(),
                None => return Err(system_error(it, "closure must be a tuple")),
            },
            None => Vec::new(),
        };
        let f = Function {
            name: RefCell::new(c.name.clone()),
            qualname: RefCell::new(c.qualname.clone()),
            code: RefCell::new(c),
            globals: g,
            defaults: RefCell::new(defaults),
            kwdefaults: RefCell::new(kwdefaults),
            closure,
            annotations: RefCell::new(None),
            type_params: RefCell::new(None),
        };
        let func = Value::Obj(Object::new(Kind::Function(Box::new(f))));
        it.call(&func, args, kwargs)
    }

    // ---- frames ---------------------------------------------------------------------------------

    #[op]
    fn frame_getlocals(it: &mut Interp, frame: &Value) -> R<Value> {
        frame_arg(it, frame)?;
        it.get_attr_str(frame, "f_locals")
    }

    #[op]
    fn frame_getglobals(it: &mut Interp, frame: &Value) -> R<Value> {
        frame_arg(it, frame)?;
        it.get_attr_str(frame, "f_globals")
    }

    #[op]
    fn frame_getgenerator(it: &mut Interp, frame: &Value) -> R<Value> {
        frame_arg(it, frame)?;
        Ok(frame_generator(it, frame).unwrap_or(Value::None))
    }

    #[op]
    fn frame_getbuiltins(it: &mut Interp, frame: &Value) -> R<Value> {
        frame_arg(it, frame)?;
        it.get_attr_str(frame, "f_builtins")
    }

    #[op]
    fn frame_getlasti(it: &mut Interp, frame: &Value) -> R<Value> {
        frame_arg(it, frame)?;
        let n = it.get_attr_str(frame, "f_lasti")?;
        Ok(match n {
            Value::Int(i) if i < 0 => Value::None,
            n => n,
        })
    }

    /// frame_new(code, globals, locals): a frame that is not executing.
    #[op]
    fn frame_new(it: &mut Interp, code: &Value, globals: &Value, locals: &Value) -> R<Value> {
        let _ = locals;
        let Value::Obj(c) = code else { return Err(it.type_error("argument must be a code object")) };
        let Kind::Code(c) = &c.kind else { return Err(it.type_error("argument must be a code object")) };
        let Value::Obj(g) = globals else { return Err(bad_internal_call(it)) };
        let line = c.first_line;
        Ok(it.dead_frame_object(c.clone(), g.clone(), line, 0))
    }

    /// frame_getvar(frame, name): `PyFrame_GetVar`.
    #[op]
    fn frame_getvar(it: &mut Interp, frame: &Value, name: &Value) -> R<Value> {
        frame_arg(it, frame)?;
        frame_var(it, frame, name)
    }

    /// frame_getvarstring(frame, name): `PyFrame_GetVarString`.
    #[op]
    fn frame_getvarstring(it: &mut Interp, frame: &Value, name: &Value) -> R<Value> {
        frame_arg(it, frame)?;
        let n = required_c_string(it, name)?;
        frame_var(it, frame, &Value::string(n))
    }

    #[op]
    fn gen_get_code(it: &mut Interp, gen: &Value) -> R<Value> {
        match gen {
            Value::Obj(o) if matches!(o.kind, Kind::Generator(_)) => it.get_attr_str(gen, "gi_code"),
            _ => Err(it.type_error("argument must be a generator object")),
        }
    }

    /// code_newempty(filename, funcname, firstlineno): `PyCode_NewEmpty`.
    #[op(hint(py(arg_style = "parse", arg_name = "code_newempty")))]
    fn code_newempty(it: &mut Interp, filename: &str, funcname: &str, firstlineno: i64) -> Value {
        let obj = Object::new(Kind::Code(new_empty_code(filename, funcname, firstlineno)));
        it.code_created(&obj);
        Value::Obj(obj)
    }

    // ---- functions ------------------------------------------------------------------------------

    #[op]
    fn function_get_code(it: &mut Interp, func: &Value) -> R<Value> {
        function_arg(it, func)?;
        it.get_attr_str(func, "__code__")
    }

    #[op]
    fn function_get_globals(it: &mut Interp, func: &Value) -> R<Value> {
        let f = function_arg(it, func)?;
        match &f.kind {
            Kind::Function(fd) => Ok(Value::Obj(fd.globals.clone())),
            _ => Err(bad_internal_call(it)),
        }
    }

    #[op]
    fn function_get_module(it: &mut Interp, func: &Value) -> R<Value> {
        function_arg(it, func)?;
        it.get_attr_str(func, "__module__")
    }

    #[op]
    fn function_get_defaults(it: &mut Interp, func: &Value) -> R<Value> {
        function_arg(it, func)?;
        it.get_attr_str(func, "__defaults__")
    }

    #[op]
    fn function_set_defaults(it: &mut Interp, func: &Value, defaults: &Value) -> R<()> {
        function_arg(it, func)?;
        if !defaults.is_none() && defaults.tuple_items().is_none() {
            return Err(system_error(it, "non-tuple default args"));
        }
        it.set_attr_str(func, "__defaults__", defaults.clone())
    }

    #[op]
    fn function_get_kw_defaults(it: &mut Interp, func: &Value) -> R<Value> {
        function_arg(it, func)?;
        it.get_attr_str(func, "__kwdefaults__")
    }

    #[op]
    fn function_set_kw_defaults(it: &mut Interp, func: &Value, defaults: &Value) -> R<()> {
        function_arg(it, func)?;
        if !defaults.is_none() && !is_dict(defaults) {
            return Err(system_error(it, "non-dict keyword only default args"));
        }
        it.set_attr_str(func, "__kwdefaults__", defaults.clone())
    }

    // ---- sys ------------------------------------------------------------------------------------

    /// sys_getobject(name): `PySys_GetObject`; `AttributeError` (the class) when unset or undecodable.
    #[op]
    fn sys_getobject(it: &mut Interp, name: &Value) -> Value {
        let attribute_error = Value::Obj(it.exc_type("AttributeError"));
        let Ok(Some(n)) = c_string(it, name) else {
            return attribute_error;
        };
        sys_dict(it).and_then(|d| dict_get_str(&d, &n)).unwrap_or(attribute_error)
    }

    /// sys_setobject(name, value): `PySys_SetObject`; a NULL value deletes the attribute.
    #[op]
    fn sys_setobject(it: &mut Interp, name: &Value, value: &Value) -> R<i64> {
        let n = required_c_string(it, name)?;
        let Some(d) = sys_dict(it) else { return Ok(-1) };
        if value.is_none() {
            dict_del_str(&d, &n);
        } else {
            dict_set_str(&d, &n, value.clone());
        }
        Ok(0)
    }

    /// sys_getxoptions(): `PySys_GetXOptions`.
    #[op]
    fn sys_getxoptions(it: &mut Interp) -> Value {
        let Some(d) = sys_dict(it) else { return Value::None };
        match dict_get_str(&d, "_xoptions") {
            Some(v) if is_dict(&v) => v,
            _ => {
                let fresh = Value::Obj(it.new_dict());
                dict_set_str(&d, "_xoptions", fresh.clone());
                fresh
            }
        }
    }

    // ---- run and compile ------------------------------------------------------------------------

    /// run_stringflags(str, start, globals, locals=None, cf_flags=0, cf_feature_version=0)
    #[op(hint(py(arg_style = "parse", arg_name = "run_stringflags")))]
    fn run_stringflags(
        it: &mut Interp,
        source: &Value,
        start: i64,
        globals: &Value,
        locals: Option<&Value>,
        cf_flags: Option<i64>,
        cf_feature_version: Option<i64>,
    ) -> R<Value> {
        let _ = cf_feature_version;
        let source = match source {
            Value::None => return Err(it.type_error("run_stringflags() argument 1 must not be None")),
            s => s.clone(),
        };
        let locals = locals.filter(|l| !l.is_none());
        run_source(it, source, "<string>", start, globals, locals, cf_flags.unwrap_or(0))
    }

    /// run_fileexflags(filename, start, globals, locals=None, closeit=0, cf_flags=0, cf_feature_version=0)
    #[op(hint(py(arg_style = "parse", arg_name = "run_fileexflags")))]
    #[allow(clippy::too_many_arguments)]
    fn run_fileexflags(
        it: &mut Interp,
        filename: &Value,
        start: i64,
        globals: &Value,
        locals: Option<&Value>,
        closeit: Option<i64>,
        cf_flags: Option<i64>,
        cf_feature_version: Option<i64>,
    ) -> R<Value> {
        let _ = (closeit, cf_feature_version);
        let name = match filename {
            Value::None => return Err(it.type_error("run_fileexflags() argument 1 must not be None")),
            f => f.clone(),
        };
        let file = call_builtin(it, "open", vec![name.clone(), Value::str("rb")])?;
        let data = it.call_method(&file, "read", Vec::new());
        let closed = it.call_method(&file, "close", Vec::new());
        let data = data?;
        closed?;
        let path = crate::bind::path::fspath(it, &name)?;
        let shown = match &path {
            Value::Obj(o) => match &o.kind {
                Kind::Bytes(b) => crate::bind::path::bytes_path(b),
                _ => path.as_str().unwrap_or("").to_string(),
            },
            _ => String::new(),
        };
        let locals = locals.filter(|l| !l.is_none());
        run_source(it, data, &shown, start, globals, locals, cf_flags.unwrap_or(0))
    }

    /// Py_CompileString(str): compile `str` (honouring a coding cookie) as a module.
    #[op(hint(py(arg_style = "parse", arg_name = "Py_CompileString")))]
    fn Py_CompileString(it: &mut Interp, source: &Value) -> R<Value> {
        call_builtin(it, "compile", vec![source.clone(), Value::str("<string>"), Value::str("exec")])
    }

    // ---- import ---------------------------------------------------------------------------------

    #[op]
    fn PyImport_GetMagicNumber(it: &mut Interp) -> R<Value> {
        let util = it.import_module("importlib.util")?;
        let magic = it.get_attr_str(&Value::Obj(util), "MAGIC_NUMBER")?;
        let from_bytes = it.get_attr_str(&builtin(it, "int"), "from_bytes")?;
        it.call(&from_bytes, vec![magic, Value::str("little")], Vec::new())
    }

    #[op]
    fn PyImport_GetMagicTag(it: &mut Interp) -> R<Value> {
        let sys = it.import_module("sys")?;
        let implementation = it.get_attr_str(&Value::Obj(sys), "implementation")?;
        it.get_attr_str(&implementation, "cache_tag")
    }

    #[op]
    fn PyImport_GetModuleDict(it: &mut Interp) -> Value {
        sys_dict(it).and_then(|d| dict_get_str(&d, "modules")).unwrap_or(Value::None)
    }

    /// PyImport_GetModule(name): the module in `sys.modules`, or the `KeyError` class.
    #[op]
    fn PyImport_GetModule(it: &mut Interp, name: &Value) -> R<Value> {
        match get_module(it, name)? {
            Some(m) => Ok(m),
            None => Ok(Value::Obj(it.exc_type("KeyError"))),
        }
    }

    #[op]
    fn PyImport_AddModuleObject(it: &mut Interp, name: &Value) -> R<Value> {
        add_module(it, name)
    }

    #[op(hint(py(arg_style = "parse", arg_name = "PyImport_AddModule")))]
    fn PyImport_AddModule(it: &mut Interp, name: &Value) -> R<Value> {
        let n = required_c_string(it, name)?;
        add_module(it, &Value::string(n))
    }

    /// PyImport_Import(name): import through `__import__` and return the named module.
    #[op]
    fn PyImport_Import(it: &mut Interp, name: &Value) -> R<Value> {
        if name.is_none() {
            return Err(system_error(it, "Python import called with NULL name"));
        }
        if name.as_str().is_none() {
            let n = it.type_name_of(name);
            return Err(it.type_error(&format!("module name must be str, not {n}")));
        }
        import_named(it, name)
    }

    #[op(hint(py(arg_style = "parse", arg_name = "PyImport_ImportModule")))]
    fn PyImport_ImportModule(it: &mut Interp, name: &Value) -> R<Value> {
        let n = required_c_string(it, name)?;
        import_named(it, &Value::string(n))
    }

    #[op(hint(py(arg_style = "parse", arg_name = "PyImport_ImportModuleNoBlock")))]
    fn PyImport_ImportModuleNoBlock(it: &mut Interp, name: &Value) -> R<Value> {
        let n = required_c_string(it, name)?;
        import_named(it, &Value::string(n))
    }

    #[op(hint(py(arg_style = "parse", arg_name = "PyImport_ImportModuleEx")))]
    fn PyImport_ImportModuleEx(it: &mut Interp, name: &Value, globals: &Value, locals: &Value, fromlist: &Value) -> R<Value> {
        let n = required_c_string(it, name)?;
        import_level(it, &Value::string(n), globals, locals, fromlist, 0)
    }

    #[op(hint(py(arg_style = "parse", arg_name = "PyImport_ImportModuleLevel")))]
    fn PyImport_ImportModuleLevel(it: &mut Interp, name: &Value, globals: &Value, locals: &Value, fromlist: &Value, level: i64) -> R<Value> {
        let n = required_c_string(it, name)?;
        import_level(it, &Value::string(n), globals, locals, fromlist, level)
    }

    #[op(hint(py(arg_style = "parse", arg_name = "PyImport_ImportModuleLevelObject")))]
    fn PyImport_ImportModuleLevelObject(it: &mut Interp, name: &Value, globals: &Value, locals: &Value, fromlist: &Value, level: i64) -> R<Value> {
        if name.is_none() {
            return Err(it.value_error("Empty module name"));
        }
        if name.as_str().is_none() {
            return Err(it.type_error("module name must be str, not bytes"));
        }
        import_level(it, name, globals, locals, fromlist, level)
    }

    /// PyImport_ImportFrozenModule(name): frozen modules are not built into this interpreter.
    #[op(hint(py(arg_style = "parse", arg_name = "PyImport_ImportFrozenModule")))]
    fn PyImport_ImportFrozenModule(it: &mut Interp, name: &Value) -> R<i64> {
        let n = required_c_string(it, name)?;
        frozen(it, &n)
    }

    #[op]
    fn PyImport_ImportFrozenModuleObject(it: &mut Interp, name: &Value) -> R<i64> {
        match name.as_str() {
            Some(n) => frozen(it, n),
            None => Ok(0),
        }
    }

    #[op(hint(py(arg_style = "parse", arg_name = "PyImport_ExecCodeModule")))]
    fn PyImport_ExecCodeModule(it: &mut Interp, name: &Value, code: &Value) -> R<Value> {
        let n = required_c_string(it, name)?;
        exec_code_module(it, &Value::string(n), code, None, None)
    }

    #[op(hint(py(arg_style = "parse", arg_name = "PyImport_ExecCodeModuleEx")))]
    fn PyImport_ExecCodeModuleEx(it: &mut Interp, name: &Value, code: &Value, pathname: &Value) -> R<Value> {
        let n = required_c_string(it, name)?;
        let p = c_string(it, pathname)?.map(Value::string);
        exec_code_module(it, &Value::string(n), code, p.as_ref(), None)
    }

    #[op(hint(py(arg_style = "parse", arg_name = "PyImport_ExecCodeModuleWithPathnames")))]
    fn PyImport_ExecCodeModuleWithPathnames(it: &mut Interp, name: &Value, code: &Value, pathname: &Value, cpathname: &Value) -> R<Value> {
        let n = required_c_string(it, name)?;
        let p = c_string(it, pathname)?.map(Value::string);
        let c = c_string(it, cpathname)?.map(Value::string);
        exec_code_module(it, &Value::string(n), code, p.as_ref(), c.as_ref())
    }

    #[op(hint(py(arg_style = "parse", arg_name = "PyImport_ExecCodeModuleObject")))]
    fn PyImport_ExecCodeModuleObject(it: &mut Interp, name: &Value, code: &Value, pathname: &Value, cpathname: &Value) -> R<Value> {
        exec_code_module(it, name, code, Some(pathname), Some(cpathname))
    }
}

fn frame_var(it: &mut Interp, frame: &Value, name: &Value) -> R<Value> {
    if name.as_str().is_none() {
        let n = it.type_name_of(name);
        return Err(it.type_error(&format!("name must be str, not {n}")));
    }
    let locals = it.get_attr_str(frame, "f_locals")?;
    match locals {
        Value::Obj(d) if matches!(d.kind, Kind::Dict(_)) => match it.dict_get(&d, name)? {
            Some(v) => Ok(v),
            None => {
                let r = it.repr_of(name)?;
                Err(it.new_exc_str("NameError", &format!("variable {r} does not exist")))
            }
        },
        other => match it.getitem(&other, name) {
            Ok(v) => Ok(v),
            Err(e) if it.exc_is(&e, "KeyError") => {
                let r = it.repr_of(name)?;
                Err(it.new_exc_str("NameError", &format!("variable {r} does not exist")))
            }
            Err(e) => Err(e),
        },
    }
}

/// `PyImport_Import`: `__import__(name, globals, locals, ['__doc__'], 0)`, then the module `name`.
fn import_named(it: &mut Interp, name: &Value) -> R<Value> {
    let globals = Value::Obj(frame_globals(it));
    let fromlist = Value::list(vec![Value::str("__doc__")]);
    import_level(it, name, &globals, &globals, &fromlist, 0)?;
    match get_module(it, name)? {
        Some(m) => Ok(m),
        None => {
            let n = it.repr_of(name)?;
            Err(it.new_exc_str("KeyError", &n))
        }
    }
}

/// `PyImport_ImportFrozenModule`: 1 when `name` is a frozen module that was executed, else 0.
fn frozen(it: &mut Interp, name: &str) -> R<i64> {
    let imp = it.import_module("_imp")?;
    let is_frozen = it.get_attr_str(&Value::Obj(imp), "is_frozen")?;
    let r = it.call(&is_frozen, vec![Value::str(name)], Vec::new())?;
    if !it.truthy(&r)? {
        return Ok(0);
    }
    it.import_module(name)?;
    Ok(1)
}
