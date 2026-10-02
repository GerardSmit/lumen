//! Module system: file imports, packages, relative imports, `sys.modules`, and the program entry.

use crate::dict::PyDict;
use crate::object::*;
use crate::vm::*;
use lumen_common::limits::StopFlags;
use std::cell::RefCell;
use crate::platform::{parent_dir, FoundModule};
use std::rc::Rc;

impl Interp {
    pub fn new_module(&mut self, name: &str) -> Obj {
        let d = Object::new(Kind::Dict(RefCell::new(PyDict::new())));
        dict_set_str(&d, "__name__", Value::str(name));
        dict_set_str(&d, "__doc__", Value::None);
        dict_set_str(&d, "__package__", Value::None);
        dict_set_str(&d, "__spec__", Value::None);
        dict_set_str(&d, "__loader__", Value::None);
        Object::with_dict(Kind::Module, d)
    }

    pub fn module_dict(&self, m: &Obj) -> Obj {
        match m.dict.borrow().as_ref() {
            Some(d) => d.clone(),
            None => Object::new(Kind::Dict(RefCell::new(PyDict::new()))),
        }
    }

    pub fn register_module(&mut self, name: &str, m: &Obj) {
        dict_set_str(&self.modules.clone(), name, Value::Obj(m.clone()));
    }

    /// A `SyntaxError` (or the `IndentationError` / `TabError` its message calls for) at
    /// `line`, 0-based column `col` (`None`: unknown) of `src`.
    pub fn syntax_error(&mut self, msg: &str, file: &str, line: u32, col: Option<u32>, src: &str) -> Obj {
        let text = src.split_inclusive('\n').nth((line as usize).wrapping_sub(1)).map(|l| match l.ends_with('\n') {
            true => l.to_string(),
            false => format!("{l}\n"),
        });
        let text = text.map_or(Value::None, Value::string);
        let offset = col.map_or(Value::None, |c| Value::Int(c as i64 + 1));
        let end_offset = col.map_or(Value::None, |c| Value::Int(c as i64 + 2));
        let detail = Value::tuple(vec![Value::str(file), Value::Int(line as i64), offset, text, Value::Int(line as i64), end_offset]);
        let cls = Value::Obj(self.exc_type(syntax_error_kind(msg)));
        match self.call(&cls, vec![Value::str(msg), detail], Vec::new()) {
            Ok(Value::Obj(e)) => e,
            Ok(_) => self.new_exc_str("SyntaxError", msg),
            Err(e) => e,
        }
    }

    pub fn compile_source(&mut self, src: &str, filename: &str) -> R<Rc<crate::bytecode::Code>> {
        let parsed = crate::limits::with_literal_digit_limit(self.int_max_str_digits, || crate::parser::parse(src, filename));
        let module = match parsed {
            Ok(m) => m,
            Err(e) => return Err(self.syntax_error(&e.msg, filename, e.line, Some(e.col), src)),
        };
        match crate::compile::compile_module(&module, filename) {
            Ok(c) => Ok(c),
            Err(e) => Err(self.syntax_error(&e.msg, filename, e.line, None, src)),
        }
    }

    fn sys_dict(&self) -> Option<Obj> {
        self.sys_module.as_ref().map(|m| self.module_dict(m))
    }

    pub fn set_argv(&mut self, argv: &[String]) {
        self.argv = argv.to_vec();
        if let Some(d) = self.sys_dict() {
            dict_set_str(&d, "argv", Value::list(argv.iter().map(|a| Value::str(a)).collect()));
        }
    }

    /// Sets `sys.path`; the first entry is also where top-level imports resolve first.
    pub fn set_path(&mut self, dirs: &[String]) {
        self.script_dir = dirs.first().cloned().unwrap_or_else(|| ".".into());
        self.path_set = true;
        if let Some(d) = self.sys_dict() {
            let mut entries: Vec<Value> = dirs.iter().map(|a| Value::str(a)).collect();
            entries.push(Value::str(crate::frozen::FROZEN_DIR));
            dict_set_str(&d, "path", Value::list(entries));
        }
    }

    /// Runs the file as `__main__`, returning its exit status. `sys.path` defaults to the file's
    /// directory unless [`Interp::set_path`] was called.
    pub fn run_file(&mut self, path: &str) -> i32 {
        let read = self.platform.borrow_mut().read_file(path);
        let src = match read {
            Ok(b) => lumen_common::smuggle::escape_text_owned(String::from_utf8_lossy(&b).into_owned()),
            Err(e) => {
                self.write_stderr(&format!("lumen-py: can't open file '{}': {}\n", path, e));
                return 2;
            }
        };
        if !self.path_set {
            let abs = self.platform.borrow_mut().canonicalize(path);
            self.set_path(&[parent_dir(&abs)]);
        }
        // CPython runs the script under its absolute path (`__file__`, tracebacks), joined to
        // the working directory without resolving links.
        let cwd = self.platform.borrow_mut().getcwd();
        let filename = match cwd {
            Ok(cwd) if !path.starts_with('/') => format!("{}/{}", cwd.trim_end_matches('/'), path),
            _ => path.to_string(),
        };
        if self.argv.is_empty() {
            self.set_argv(&[path.to_string()]);
        }
        self.run_source(&src, &filename)
    }

    /// Runs `src` as the `__main__` module and returns its exit status (`SystemExit` codes
    /// included; uncaught exceptions are reported on stderr and give 1).
    pub fn run_source(&mut self, src: &str, filename: &str) -> i32 {
        if self.argv.is_empty() {
            self.set_argv(&[filename.to_string()]);
        }
        let main = self.new_module("__main__");
        let globals = self.module_dict(&main);
        dict_set_str(&globals, "__file__", Value::str(filename));
        dict_set_str(&globals, "__builtins__", Value::Obj(self.builtins.clone()));
        self.register_module("__main__", &main);
        self.main_globals = Some(globals.clone());
        self.interrupted = false;
        let stop = StopFlags::from_handle(&self.interrupt);
        let outcome = lumen_common::bigint::interruptible(&stop, || match self.compile_source(src, filename) {
            Ok(code) => self.run_code(code, globals.clone(), globals),
            Err(e) => Err(e),
        });
        let result = match outcome {
            Some(r) => r,
            None => Err(self.interrupt_exc()),
        };
        let result = match result {
            Ok(_) if self.interrupt.is_interrupted() => Err(self.interrupt_exc()),
            r => r.map_err(|e| self.supersede_by_interrupt(e)),
        };
        let code = match result {
            Ok(_) => 0,
            Err(e) => {
                if self.is_exc_instance(&e, "KeyboardInterrupt") {
                    self.interrupted = true;
                    self.interrupt.clear();
                }
                self.flush_out();
                self.report_uncaught(&e)
            }
        };
        self.run_exit_hooks();
        code
    }

    fn run_exit_hooks(&mut self) {
        self.run_atexit();
        crate::builtins::iom::flush_std_streams(self);
        self.flush_out();
    }

    fn sys_path(&mut self) -> Vec<String> {
        if let Some(sys) = self.sys_module.clone() {
            let d = self.module_dict(&sys);
            if let Some(p) = dict_get_str(&d, "path") {
                if let Some(l) = list_of(&p) {
                    return l.borrow().iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
                }
            }
        }
        vec![self.script_dir.clone()]
    }

    fn module_not_found(&mut self, name: &str, msg: String) -> Obj {
        let e = self.new_exc_str("ModuleNotFoundError", &msg);
        self.set_exc_attr(&e, "name", Value::str(name));
        e
    }

    pub fn import_module(&mut self, full: &str) -> R<Obj> {
        if let Some(Value::Obj(m)) = dict_get_str(&self.modules, full) {
            return Ok(m);
        }
        if let Some(m) = crate::builtins::modules::builtin_module(self, full) {
            self.register_module(full, &m);
            return Ok(m);
        }
        let (parent_name, leaf) = match full.rfind('.') {
            Some(i) => (Some(&full[..i]), &full[i + 1..]),
            None => (None, full),
        };
        let (dirs, parent_mod) = match parent_name {
            Some(p) => {
                let pm = self.import_module(p)?;
                let pd = self.module_dict(&pm);
                let path = match dict_get_str(&pd, "__path__") {
                    Some(v) => v,
                    None => return Err(self.module_not_found(full, format!("No module named '{}'; '{}' is not a package", full, p))),
                };
                let dirs: Vec<String> = match list_of(&path) {
                    Some(l) => l.borrow().iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect(),
                    None => Vec::new(),
                };
                (dirs, Some(pm))
            }
            None => (self.sys_path(), None),
        };
        let found = self.platform.borrow_mut().find_module(&dirs, leaf);
        let Some(FoundModule { filename: file_s, source, is_package: is_pkg }) = found else {
            return Err(self.module_not_found(full, format!("No module named '{}'", full)));
        };
        let src = lumen_common::smuggle::escape_text_owned(String::from_utf8_lossy(&source).into_owned());
        let m = self.new_module(full);
        let g = self.module_dict(&m);
        dict_set_str(&g, "__file__", Value::str(&file_s));
        dict_set_str(&g, "__builtins__", Value::Obj(self.builtins.clone()));
        if is_pkg {
            dict_set_str(&g, "__package__", Value::str(full));
            dict_set_str(&g, "__path__", Value::list(vec![Value::str(&parent_dir(&file_s))]));
        } else {
            dict_set_str(&g, "__package__", Value::str(parent_name.unwrap_or("")));
        }
        self.register_module(full, &m);
        let code = match self.compile_source(&src, &file_s) {
            Ok(c) => c,
            Err(e) => {
                dict_del_str(&self.modules.clone(), full);
                return Err(e);
            }
        };
        if let Err(e) = self.run_code(code, g.clone(), g) {
            dict_del_str(&self.modules.clone(), full);
            return Err(e);
        }
        if let Some(pm) = parent_mod {
            let pd = self.module_dict(&pm);
            dict_set_str(&pd, leaf, Value::Obj(m.clone()));
        }
        Ok(m)
    }

    fn resolve_relative(&mut self, name: &str, level: usize) -> R<String> {
        let globals = self.frames.last().map(|f| f.globals.clone());
        let g = match globals {
            Some(g) => g,
            None => return Err(self.new_exc_str("ImportError", "attempted relative import with no known parent package")),
        };
        let pkg = match dict_get_str(&g, "__package__") {
            Some(Value::Obj(o)) if matches!(o.kind, Kind::Str(_)) => o.as_str_kind().unwrap_or("").to_string(),
            _ => {
                let n = dict_get_str(&g, "__name__").and_then(|v| v.as_str().map(|s| s.to_string())).unwrap_or_default();
                if dict_get_str(&g, "__path__").is_some() {
                    n
                } else {
                    n.rsplit_once('.').map(|(a, _)| a.to_string()).unwrap_or_default()
                }
            }
        };
        if pkg.is_empty() {
            return Err(self.new_exc_str("ImportError", "attempted relative import with no known parent package"));
        }
        let mut base: &str = &pkg;
        for _ in 1..level {
            match base.rfind('.') {
                Some(i) => base = &base[..i],
                None => return Err(self.new_exc_str("ImportError", "attempted relative import beyond top-level package")),
            }
        }
        Ok(if name.is_empty() { base.to_string() } else { format!("{}.{}", base, name) })
    }

    pub fn import_name(&mut self, name: &str, level: usize, fromlist: &Value) -> R<Value> {
        let full = if level > 0 { self.resolve_relative(name, level)? } else { name.to_string() };
        let mut prefix = String::new();
        let mut top: Option<Obj> = None;
        for (i, part) in full.split('.').enumerate() {
            if i > 0 {
                prefix.push('.');
            }
            prefix.push_str(part);
            let m = self.import_module(&prefix)?;
            if i == 0 {
                top = Some(m);
            }
        }
        let leaf = self.import_module(&full)?;
        let has_from = !fromlist.is_none();
        if has_from {
            let ld = self.module_dict(&leaf);
            if dict_get_str(&ld, "__path__").is_some() {
                let names = self.iterate_to_vec(fromlist)?;
                for n in names {
                    if let Some(s) = n.as_str() {
                        if s == "*" || dict_get_str(&ld, s).is_some() {
                            continue;
                        }
                        let sub = format!("{}.{}", full, s);
                        if let Err(e) = self.import_module(&sub) {
                            if !(self.exc_is(&e, "ModuleNotFoundError") && self.exc_name_is(&e, &sub)) {
                                return Err(e);
                            }
                        }
                    }
                }
            }
            return Ok(Value::Obj(leaf));
        }
        if level > 0 {
            return Ok(Value::Obj(leaf));
        }
        Ok(Value::Obj(top.unwrap_or(leaf)))
    }

    fn exc_name_is(&mut self, e: &Obj, name: &str) -> bool {
        match e.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "name")) {
            Some(v) => v.as_str() == Some(name),
            None => false,
        }
    }

    pub fn import_from(&mut self, m: &Value, name: &Obj) -> R<Value> {
        match self.get_attr(m, name) {
            Ok(v) => Ok(v),
            Err(e) => {
                if !self.exc_is(&e, "AttributeError") {
                    return Err(e);
                }
                let nm = name.as_str_kind().unwrap_or("").to_string();
                let (modname, file) = match m {
                    Value::Obj(o) if matches!(o.kind, Kind::Module) => {
                        let d = self.module_dict(o);
                        (
                            dict_get_str(&d, "__name__").and_then(|v| v.as_str().map(|s| s.to_string())),
                            dict_get_str(&d, "__file__").and_then(|v| v.as_str().map(|s| s.to_string())),
                        )
                    }
                    _ => (None, None),
                };
                if let Some(mn) = &modname {
                    if let Some(v) = dict_get_str(&self.modules, &format!("{}.{}", mn, nm)) {
                        return Ok(v);
                    }
                }
                let mn = modname.unwrap_or_else(|| "<unknown module name>".into());
                let msg = match file {
                    Some(f) => format!("cannot import name '{}' from '{}' ({})", nm, mn, f),
                    None => format!("cannot import name '{}' from '{}' (unknown location)", nm, mn),
                };
                let err = self.new_exc_str("ImportError", &msg);
                self.set_exc_attr(&err, "name", Value::str(&mn));
                Err(err)
            }
        }
    }

    pub fn import_star(&mut self, m: &Value, ns: &Obj) -> R<()> {
        let d = match m {
            Value::Obj(o) if matches!(o.kind, Kind::Module) => self.module_dict(o),
            _ => return Err(self.type_error("import * requires a module")),
        };
        let names: Vec<Value> = match dict_get_str(&d, "__all__") {
            Some(all) => self.iterate_to_vec(&all)?,
            None => match &d.kind {
                Kind::Dict(p) => p.borrow().keys().into_iter().filter(|k| k.as_str().map(|s| !s.starts_with('_')).unwrap_or(false)).collect(),
                _ => Vec::new(),
            },
        };
        for n in names {
            let no = match &n {
                Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => o.clone(),
                _ => return Err(self.type_error("Item in __all__ must be str")),
            };
            let v = self.get_attr(m, &no)?;
            dict_set_name(ns, &no, v);
        }
        Ok(())
    }
}

/// The `SyntaxError` subclass CPython raises for a tokenizer / parser message.
pub fn syntax_error_kind(msg: &str) -> &'static str {
    if msg.starts_with("inconsistent use of tabs") {
        "TabError"
    } else if msg.contains("indent") {
        "IndentationError"
    } else {
        "SyntaxError"
    }
}
