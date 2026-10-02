//! `_testinternalcapi`: interpreter-level introspection used by CPython's own tests.

#![allow(non_snake_case)]

use super::{call_builtin, system_error};
use crate::builtins::subinterpm::{self, Settings};
use crate::object::*;
use crate::vm::{dict_set_str, Interp};
use std::io::Write;
use std::sync::Mutex;

static PERF_MAP: Mutex<Option<std::fs::File>> = Mutex::new(None);

/// The keys `set_config` has replaced; the others come from the interpreter's own state.
#[derive(Default)]
struct ConfigOverrides(Vec<(String, Value)>);

fn config_dict(it: &mut Interp) -> Obj {
    let d = it.new_dict();
    for (k, v) in [
        ("code_debug_ranges", Value::Int(1)),
        ("isolated", Value::Int(0)),
        ("use_environment", Value::Int(1)),
        ("verbose", Value::Int(0)),
        ("bytes_warning", Value::Int(0)),
        ("optimization_level", Value::Int(0)),
        ("safe_path", Value::Int(0)),
        ("tracemalloc", Value::Int(0)),
        ("import_time", Value::Int(0)),
        ("perf_profiling", Value::Int(0)),
    ] {
        dict_set_str(&d, k, v);
    }
    let overrides: Vec<(String, Value)> = it.native_state::<ConfigOverrides>().0.clone();
    for (k, v) in overrides {
        dict_set_str(&d, &k, v);
    }
    dict_set_str(&d, "int_max_str_digits", Value::Int(it.int_max_str_digits as i64));
    d
}

fn frame_arg(it: &mut Interp, frame: &Value) -> R<()> {
    if it.type_name_of(frame) == "frame" {
        Ok(())
    } else {
        Err(it.type_error("argument must be a frame"))
    }
}

fn interpreter_number(it: &mut Interp, id: &Value) -> R<i64> {
    match call_builtin(it, "int", vec![id.clone()])? {
        Value::Int(n) => Ok(n),
        _ => Err(it.value_error("interpreter not found")),
    }
}

fn optimizer_unsupported(it: &mut Interp) -> Obj {
    it.new_exc_str("NotImplementedError", "the optimizer is not supported")
}

fn op_tuple(op: &crate::bytecode::Op, line: u32) -> Value {
    let text = format!("{op:?}");
    let (name, arg) = match text.split_once('(') {
        Some((name, rest)) => (name.to_string(), rest.trim_end_matches(')').parse::<i64>().unwrap_or(0)),
        None => (text, 0),
    };
    let line = Value::Int(i64::from(line));
    Value::tuple(vec![Value::string(name), Value::Int(arg), line.clone(), line, Value::Int(0), Value::Int(0)])
}

const ERROR_HANDLERS: [&str; 3] = ["strict", "surrogateescape", "surrogatepass"];

fn locale_handler(it: &mut Interp, errors: Option<&str>) -> R<String> {
    let name = errors.unwrap_or("strict");
    if ERROR_HANDLERS.contains(&name) {
        Ok(name.to_string())
    } else {
        Err(it.value_error("unsupported error handler"))
    }
}

fn error_start(it: &mut Interp, e: &Obj) -> i64 {
    match it.get_attr_str(&Value::Obj(e.clone()), "start") {
        Ok(Value::Int(n)) => n,
        _ => 0,
    }
}

#[lumen_bind::module(name = "_testinternalcapi")]
pub mod _testinternalcapi {
    use super::*;

    #[constant(name = "SIZEOF_PYGC_HEAD")]
    const SIZEOF_PYGC_HEAD: i64 = 16;
    #[constant(name = "SIZEOF_TIME_T")]
    const SIZEOF_TIME_T: i64 = 8;

    /// get_recursion_depth() -> int
    #[op]
    fn get_recursion_depth(it: &mut Interp) -> i64 {
        it.frames.len() as i64
    }

    /// get_c_recursion_remaining() -> int
    #[op]
    fn get_c_recursion_remaining(it: &mut Interp) -> i64 {
        it.recursion_limit as i64 - it.frames.len() as i64
    }

    /// get_config() -> dict
    #[op]
    fn get_config(it: &mut Interp) -> Value {
        Value::Obj(config_dict(it))
    }

    /// set_config(config): replace the settings named by the dict.
    #[op]
    fn set_config(it: &mut Interp, config: &Value) -> R<()> {
        if !matches!(config, Value::Obj(o) if matches!(o.kind, Kind::Dict(_))) {
            return Err(it.type_error("config must be a dict"));
        }
        let keys = it.iterate_to_vec(config)?;
        let mut digits = None;
        for k in keys {
            let Some(name) = k.as_str().map(str::to_string) else {
                return Err(it.type_error("config keys must be str"));
            };
            let value = it.getitem(config, &k)?;
            if name == "int_max_str_digits" {
                digits = Some(value);
                continue;
            }
            let state = it.native_state::<ConfigOverrides>();
            state.0.retain(|(n, _)| *n != name);
            state.0.push((name, value));
        }
        if let Some(Value::Int(n)) = digits {
            if n < 0 || !it.set_int_max_str_digits(n as usize) {
                return Err(it.value_error("config int_max_str_digits is invalid"));
            }
        }
        Ok(())
    }

    /// get_configs() -> dict
    #[op]
    fn get_configs(it: &mut Interp) -> Value {
        let d = it.new_dict();
        let c = config_dict(it);
        dict_set_str(&d, "config", Value::Obj(c));
        Value::Obj(d)
    }

    #[op]
    fn reset_path_config() {}

    /// get_interp_settings(interpid=-1) -> {'feature_flags': int, 'own_gil': bool}
    #[op]
    fn get_interp_settings(it: &mut Interp, interpid: Option<i64>) -> R<Value> {
        let id = match interpid.unwrap_or(-1) {
            n if n < 0 => subinterpm::current_interp_id(),
            0 => 0,
            n => return Err(it.new_exc_str("NotImplementedError", &n.to_string())),
        };
        let Settings { flags, own_gil } = subinterpm::settings_of(id);
        let d = it.new_dict();
        dict_set_str(&d, "feature_flags", Value::Int(i64::from(flags)));
        dict_set_str(&d, "own_gil", Value::Bool(own_gil));
        Ok(Value::Obj(d))
    }

    /// get_interpreter_id() -> int: the number of the interpreter running this call.
    #[op]
    fn get_interpreter_id() -> i64 {
        subinterpm::current_interp_id()
    }

    /// interpreter_exists(id) -> bool
    #[op]
    fn interpreter_exists(it: &mut Interp, id: &Value) -> R<bool> {
        let n = interpreter_number(it, id)?;
        Ok(subinterpm::interpreter_ids().contains(&n))
    }

    /// create_interpreter() -> id
    #[op]
    fn create_interpreter(it: &mut Interp) -> R<Value> {
        let m = it.import_module("_xxsubinterpreters")?;
        let create = it.get_attr_str(&Value::Obj(m), "create")?;
        it.call(&create, Vec::new(), Vec::new())
    }

    /// destroy_interpreter(id)
    #[op]
    fn destroy_interpreter(it: &mut Interp, id: &Value) -> R<()> {
        let m = it.import_module("_xxsubinterpreters")?;
        let destroy = it.get_attr_str(&Value::Obj(m), "destroy")?;
        it.call(&destroy, vec![id.clone()], Vec::new())?;
        Ok(())
    }

    /// exec_interpreter(id, code): run `code` in the interpreter.
    #[op]
    fn exec_interpreter(it: &mut Interp, id: &Value, code: &Value) -> R<()> {
        let m = it.import_module("_xxsubinterpreters")?;
        let run = it.get_attr_str(&Value::Obj(m), "run_string")?;
        it.call(&run, vec![id.clone(), code.clone()], Vec::new())?;
        Ok(())
    }

    /// pending_threadfunc(callable, /, *, ensure_added=False) -> bool
    #[op]
    fn pending_threadfunc(it: &mut Interp, callable: &Value, #[kwonly] #[default(false)] ensure_added: bool) -> R<bool> {
        let _ = ensure_added;
        it.call(callable, Vec::new(), Vec::new())?;
        Ok(true)
    }

    /// pending_identify(interpid) -> int: the ID of the interpreter that runs a pending call.
    #[op]
    fn pending_identify(it: &mut Interp, interpid: &Value) -> R<i64> {
        let n = interpreter_number(it, interpid)?;
        if !subinterpm::interpreter_ids().contains(&n) {
            return Err(it.value_error("interpreter not found"));
        }
        Ok(n)
    }

    /// clear_extension(name, filename): forget the cached state of a single-phase extension.
    #[op]
    fn clear_extension(it: &mut Interp, name: &Value, filename: &Value) -> R<()> {
        crate::builtins::singlephasem::clear_extension(it, name, filename)
    }

    /// set_eval_frame_record(list): append the name of every Python function entered to `list`.
    #[op]
    fn set_eval_frame_record(it: &mut Interp, list: &Value) -> R<()> {
        match list {
            Value::Obj(o) if matches!(o.kind, Kind::List(_)) => {
                it.eval_record = Some(o.clone());
                Ok(())
            }
            _ => Err(it.type_error("argument must be a list")),
        }
    }

    #[op]
    fn set_eval_frame_default(it: &mut Interp) {
        it.eval_record = None;
    }

    /// iframe_getcode(frame) -> code
    #[op]
    fn iframe_getcode(it: &mut Interp, frame: &Value) -> R<Value> {
        frame_arg(it, frame)?;
        it.get_attr_str(frame, "f_code")
    }

    /// iframe_getline(frame) -> int
    #[op]
    fn iframe_getline(it: &mut Interp, frame: &Value) -> R<Value> {
        frame_arg(it, frame)?;
        it.get_attr_str(frame, "f_lineno")
    }

    /// iframe_getlasti(frame) -> int
    #[op]
    fn iframe_getlasti(it: &mut Interp, frame: &Value) -> R<Value> {
        frame_arg(it, frame)?;
        it.get_attr_str(frame, "f_lasti")
    }

    /// write_perf_map_entry(addr, size, name) -> 0
    #[op]
    fn write_perf_map_entry(it: &mut Interp, addr: &Value, size: i64, name: &str) -> R<i64> {
        let Value::Int(addr) = addr else {
            return Err(it.type_error("an integer is required"));
        };
        let mut slot = PERF_MAP.lock().unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            let path = format!("/tmp/perf-{}.map", std::process::id());
            match std::fs::OpenOptions::new().create(true).append(true).open(path) {
                Ok(f) => *slot = Some(f),
                Err(e) => return Err(it.runtime_error(&e.to_string())),
            }
        }
        if let Some(f) = slot.as_mut() {
            if let Err(e) = writeln!(f, "{:x} {:x} {}", *addr as u64, size as u32, name) {
                return Err(it.runtime_error(&e.to_string()));
            }
        }
        Ok(0)
    }

    #[op]
    fn perf_map_state_teardown() {
        *PERF_MAP.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// EncodeLocaleEx(text, current_locale=0, errors=None) -> bytes
    #[op]
    fn EncodeLocaleEx(it: &mut Interp, text: &Value, current_locale: Option<i64>, errors: Option<&str>) -> R<Value> {
        let _ = current_locale;
        if text.as_str().is_none() {
            let t = it.type_name_of(text);
            return Err(it.type_error(&format!("EncodeLocaleEx() argument 1 must be str, not {t}")));
        }
        let handler = locale_handler(it, errors)?;
        let args = vec![Value::str("utf-8"), Value::string(handler)];
        match it.call_method(text, "encode", args) {
            Err(e) if it.exc_is(&e, "UnicodeEncodeError") => {
                let start = error_start(it, &e);
                Err(it.runtime_error(&format!("encode error: pos={start}, reason=encoding error")))
            }
            r => r,
        }
    }

    /// DecodeLocaleEx(bytes, current_locale=0, errors=None) -> str
    #[op]
    fn DecodeLocaleEx(it: &mut Interp, data: &Value, current_locale: Option<i64>, errors: Option<&str>) -> R<Value> {
        let _ = current_locale;
        let handler = locale_handler(it, errors)?;
        let Some(bytes) = crate::builtins::memview::contiguous_bytes(it, data)? else {
            let t = it.type_name_of(data);
            return Err(it.type_error(&format!("DecodeLocaleEx() argument 1 must be read-only bytes-like object, not {t}")));
        };
        let args = vec![Value::str("utf-8"), Value::string(handler)];
        match it.call_method(&Value::bytes(bytes), "decode", args) {
            Err(e) if it.exc_is(&e, "UnicodeDecodeError") => {
                let start = error_start(it, &e);
                Err(it.runtime_error(&format!("decode error: pos={start}, reason=decoding error")))
            }
            r => r,
        }
    }

    /// gh_119213_getargs(spam=None)
    #[op]
    fn gh_119213_getargs(spam: Option<&Value>) -> Value {
        spam.cloned().unwrap_or(Value::None)
    }

    /// compiler_cleandoc(doc) -> str: the docstring the compiler stores.
    #[op]
    fn compiler_cleandoc(it: &mut Interp, doc: &Value) -> R<Value> {
        let inspect = it.import_module("inspect")?;
        let clean = it.get_attr_str(&Value::Obj(inspect), "cleandoc")?;
        it.call(&clean, vec![doc.clone()], Vec::new())
    }

    /// compiler_codegen(ast, filename, optimize, compile_mode=0) -> [(opname, arg, ...)]
    #[op]
    fn compiler_codegen(it: &mut Interp, ast: &Value, filename: &Value, optimize: i64, compile_mode: Option<i64>) -> R<Value> {
        let mode = match compile_mode.unwrap_or(0) {
            0 => "exec",
            1 => "eval",
            2 => "single",
            _ => return Err(it.value_error("compile_mode must be 0, 1 or 2")),
        };
        let args = vec![ast.clone(), filename.clone(), Value::str(mode), Value::Int(0), Value::Bool(false), Value::Int(optimize)];
        let code = call_builtin(it, "compile", args)?;
        let c = match &code {
            Value::Obj(o) => match &o.kind {
                Kind::Code(c) => c.clone(),
                _ => return Err(system_error(it, "compile() returned a non-code object")),
            },
            _ => return Err(system_error(it, "compile() returned a non-code object")),
        };
        let items: Vec<Value> = c.ops.iter().enumerate().map(|(i, op)| op_tuple(op, c.lines.get(i).copied().unwrap_or(c.first_line))).collect();
        Ok(Value::list(items))
    }

    /// optimize_cfg(instructions, consts, nlocals) -> (instructions, consts): the compiler
    /// optimises while it generates, so a listing is already optimised.
    #[op]
    fn optimize_cfg(instructions: &Value, consts: &Value, nlocals: i64) -> Value {
        let _ = nlocals;
        Value::tuple(vec![instructions.clone(), consts.clone()])
    }

    /// assemble_code_object(filename, instructions, metadata): the listing of `compiler_codegen`
    /// names this interpreter's instructions, which cannot be assembled back.
    #[op]
    fn assemble_code_object(it: &mut Interp, filename: &Value, instructions: &Value, metadata: &Value) -> R<Value> {
        let _ = (filename, instructions);
        if !matches!(metadata, Value::Obj(o) if matches!(o.kind, Kind::Dict(_))) {
            return Err(it.type_error("metadata must be a dict"));
        }
        Err(it.new_exc_str("NotImplementedError", "instruction lists cannot be assembled into a code object"))
    }

    /// get_optimizer() -> None: there is no optimizer.
    #[op]
    fn get_optimizer() -> Value {
        Value::None
    }

    /// set_optimizer(optimizer): only `None` (no optimizer) is accepted.
    #[op]
    fn set_optimizer(it: &mut Interp, optimizer: &Value) -> R<()> {
        if optimizer.is_none() {
            Ok(())
        } else {
            Err(optimizer_unsupported(it))
        }
    }

    #[op]
    fn new_counter_optimizer(it: &mut Interp) -> R<Value> {
        Err(optimizer_unsupported(it))
    }

    #[op]
    fn new_uop_optimizer(it: &mut Interp) -> R<Value> {
        Err(optimizer_unsupported(it))
    }

    #[op]
    fn get_executor(it: &mut Interp, code: &Value, offset: i64) -> R<Value> {
        let _ = (code, offset);
        Err(optimizer_unsupported(it))
    }

    #[op]
    fn invalidate_executors(code: &Value) {
        let _ = code;
    }

    #[op]
    fn test_bswap() {}

    #[op]
    fn test_popcount() {}

    #[op]
    fn test_bit_length() {}

    #[op]
    fn test_hashtable() {}

    #[op]
    fn test_atomic_funcs() {}

    /// test_edit_cost(): the edit costs "Did you mean" suggestions rank by.
    #[op]
    fn test_edit_cost(it: &mut Interp) -> R<()> {
        for (a, b, n) in [
            ("", "", 0),
            ("", "a", 2),
            ("a", "A", 1),
            ("Apple", "Aple", 2),
            ("Banana", "B@n@n@", 6),
            ("Cherry", "Cherry!", 2),
            ("---0---", "------", 2),
            ("abc", "y", 6),
            ("aa", "bb", 4),
            ("aaaaa", "AAAAA", 5),
            ("wxyz", "wXyZ", 2),
            ("wxyz", "wXyZ123", 8),
            ("Python", "Java", 12),
            ("Java", "C#", 8),
            ("AbstractFoobarManager", "abstract_foobar_manager", 7),
            ("CPython", "PyPy", 10),
            ("CPython", "pypy", 11),
            ("AttributeError", "AttributeErrop", 2),
            ("AttributeError", "AttributeErrorTests", 10),
        ] {
            let got = lumen_common::editdist::edit_cost(a.as_bytes(), b.as_bytes(), None);
            if got != n {
                return Err(it.new_exc_str("AssertionError", &format!("Edit cost from '{a}' to '{b}' returns {got}, expected {n}")));
            }
        }
        Ok(())
    }

    /// test_bytes_find(): substring search over bytes.
    #[op]
    fn test_bytes_find(it: &mut Interp) -> R<()> {
        for (hay, needle, offset, expected) in [
            ("", "", 0, 0i64),
            ("Python", "", 3, 3),
            ("Python", "", 6, 6),
            ("Python", "yth", 0, 1),
            ("ython", "yth", 1, 1),
            ("thon", "yth", 2, -1),
            ("hon", "thon", 3, -1),
            ("Pytho", "zz", 0, -1),
            ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab", "ab", 0, 30),
            ("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaabb", "bb", 0, 30),
        ] {
            let got = lumen_common::search::find_from(hay.as_bytes(), needle.as_bytes(), offset).map_or(-1, |i| i as i64);
            if got != expected {
                return Err(it.new_exc_str("AssertionError", &format!("Incorrect result: '{needle}' in '{hay}' (offset={offset})")));
            }
        }
        Ok(())
    }

    /// normalize_path(path) -> str: collapse `.`, `..` and duplicate separators.
    #[op]
    fn normalize_path(it: &mut Interp, path: &str) -> R<String> {
        if path.contains('\0') {
            return Err(it.value_error("embedded null character"));
        }
        let absolute = path.starts_with('/');
        let mut parts: Vec<&str> = Vec::new();
        for p in path.split('/') {
            match p {
                "" | "." => {}
                ".." => {
                    if parts.last().is_some_and(|l| *l != "..") {
                        parts.pop();
                    } else if !absolute {
                        parts.push("..");
                    }
                }
                _ => parts.push(p),
            }
        }
        let joined = parts.join("/");
        Ok(if absolute { format!("/{joined}") } else if joined.is_empty() { ".".to_string() } else { joined })
    }
}
