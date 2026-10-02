//! `_imp`: the low-level import hooks importlib builds on. The embedded standard library is an
//! ordinary `sys.path` entry (see [`crate::frozen`]), so there are no frozen modules here, and
//! no extension modules can be loaded.

/// (Extremely) low-level import machinery bits as used by importlib.
#[lumen_bind::module(name = "_imp")]
pub mod _imp {
    use crate::object::*;
    use crate::vm::{dict_get_str, dict_set_str, Interp};

    /// The import lock's recursion depth.
    #[derive(Default)]
    struct ImportLock {
        depth: u32,
    }

    fn no_frozen(it: &mut Interp, name: &Value) -> Obj {
        let r = it.repr_of(name).unwrap_or_default();
        let e = it.new_exc_str("ImportError", &format!("No such frozen object named {r}"));
        it.set_exc_attr(&e, "name", name.clone());
        e
    }

    /// Return True if the import lock is currently held, else False.
    ///
    /// On platforms without threads, return False.
    #[op]
    fn lock_held(it: &mut Interp) -> bool {
        it.native_state::<ImportLock>().depth > 0
    }

    /// Acquires the interpreter's import lock for the current thread.
    ///
    /// This lock should be used by import hooks to ensure thread-safety when importing
    /// modules. On platforms without threads, this function does nothing.
    #[op]
    fn acquire_lock(it: &mut Interp) {
        it.native_state::<ImportLock>().depth += 1;
    }

    /// Release the interpreter's import lock.
    ///
    /// On platforms without threads, this function does nothing.
    #[op]
    fn release_lock(it: &mut Interp) -> R<()> {
        let lock = it.native_state::<ImportLock>();
        if lock.depth == 0 {
            return Err(it.runtime_error("not holding the import lock"));
        }
        lock.depth -= 1;
        Ok(())
    }

    /// Returns True if the module name corresponds to a built-in module.
    #[op]
    fn is_builtin(name: &str) -> i64 {
        match name {
            "sys" | "builtins" => -1,
            _ if crate::builtins::modules::builtin_module_names().contains(&name) => 1,
            _ => 0,
        }
    }

    /// Create an extension module.
    #[op]
    fn create_builtin(it: &mut Interp, spec: &Value) -> R<Value> {
        let name = it.get_attr_str(spec, "name")?;
        let Some(n) = name.as_str().map(str::to_string) else {
            let t = it.type_name_of(&name);
            return Err(it.type_error(&format!("name must be string, not {t}")));
        };
        if let Some(m) = dict_get_str(&it.modules, &n) {
            return Ok(m);
        }
        Ok(crate::builtins::modules::builtin_module(it, &n).map_or(Value::None, Value::Obj))
    }

    /// Initialize a built-in module.
    #[op(hint(py(text_signature = "($module, mod, /)")))]
    fn exec_builtin(module: &Value) -> i64 {
        let _ = module;
        0
    }

    /// Returns True if the module name corresponds to a frozen module.
    #[op]
    fn is_frozen(name: &str) -> bool {
        let _ = name;
        false
    }

    /// Return info about the corresponding frozen module (if there is one) or None.
    ///
    /// The returned info (a 2-tuple):
    ///
    ///  * data         the raw marshalled bytes
    ///  * is_package   whether or not it is a package
    ///  * origname     the originally frozen module's name, or None if not
    ///                 a stdlib module (this will usually be the same as
    ///                 the module's current name)
    #[op]
    fn find_frozen(name: &str, #[kwonly] withdata: Option<&Value>) -> Value {
        let _ = (name, withdata);
        Value::None
    }

    /// Create a code object for a frozen module.
    #[op]
    fn get_frozen_object(it: &mut Interp, name: &Value, data: Option<&Value>) -> R<Value> {
        let _ = data;
        Err(no_frozen(it, name))
    }

    /// Returns True if the module name is of a frozen package.
    #[op]
    fn is_frozen_package(it: &mut Interp, name: &Value) -> R<bool> {
        Err(no_frozen(it, name))
    }

    /// Initializes a frozen module.
    #[op]
    fn init_frozen(name: &str) -> Value {
        let _ = name;
        Value::None
    }

    /// Returns the list of available frozen modules.
    #[op]
    fn _frozen_module_names() -> Value {
        Value::list(Vec::new())
    }

    /// Returns the list of file suffixes used to identify extension modules.
    #[op]
    fn extension_suffixes() -> Value {
        Value::list(Vec::new())
    }

    /// Create an extension module.
    #[op(hint(py(text_signature = "($module, spec, file=<unrepresentable>, /)")))]
    fn create_dynamic(it: &mut Interp, spec: &Value, file: Option<&Value>) -> R<Value> {
        let _ = file;
        let name = it.get_attr_str(spec, "name")?;
        let n = it.str_of(&name)?;
        let e = it.new_exc_str("ImportError", &format!("extension modules are not supported: '{n}'"));
        it.set_exc_attr(&e, "name", name);
        Err(e)
    }

    /// Initialize an extension module.
    #[op(hint(py(text_signature = "($module, mod, /)")))]
    fn exec_dynamic(module: &Value) -> i64 {
        let _ = module;
        0
    }

    /// Changes code.co_filename to specify the passed-in file path.
    ///
    ///   code
    ///     Code object to change.
    ///   path
    ///     File path to use.
    #[op]
    fn _fix_co_filename(code: &Value, path: &str) {
        let _ = (code, path);
    }

    #[op]
    fn source_hash(it: &mut Interp, #[kw] key: i64, #[kw] source: &Value) -> R<Value> {
        let Some(data) = crate::builtins::memview::contiguous_bytes(it, source)? else {
            let t = it.type_name_of(source);
            return Err(it.type_error(&format!("a bytes-like object is required, not '{t}'")));
        };
        Ok(Value::bytes(lumen_common::siphash::siphash13(key as u64, 0, &data).to_le_bytes().to_vec()))
    }

    /// (internal-only) Override PyConfig.use_frozen_modules.
    ///
    /// (-1: "off", 1: "on", 0: no override)
    /// See frozen_modules() in Lib/test/support/import_helper.py.
    #[op(hint(py(text_signature = "($module, override, /)")))]
    fn _override_frozen_modules_for_tests(flag: i64) {
        let _ = flag;
    }

    /// (internal-only) Override PyInterpreterConfig.check_multi_interp_extensions.
    ///
    /// (-1: "never", 1: "always", 0: no override)
    #[op(hint(py(text_signature = "($module, override, /)")))]
    fn _override_multi_interp_extensions_check(flag: i64) -> i64 {
        crate::builtins::subinterpm::override_extensions_check(flag)
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        dict_set_str(&d, "check_hash_based_pycs", Value::str("default"));
    }
}
