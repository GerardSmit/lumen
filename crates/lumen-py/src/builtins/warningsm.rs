//! `_warnings`: the accelerator `warnings.py` imports its filters, lock helpers and `warn` /
//! `warn_explicit` from. The warning machinery itself is `_py_warnings`; the natives here hand
//! their arguments to it, so one implementation serves both the accelerated and the pure
//! `warnings` module.

pub use _warnings::{warn_category, warn_explicit_category};

/// _warnings provides basic warning filtering support.
/// It is a helper module to speed up interpreter start-up.
#[lumen_bind::module(name = "_warnings")]
pub mod _warnings {
    use crate::object::*;
    use crate::vm::*;

    fn py_warnings(it: &mut Interp, name: &str) -> R<Value> {
        let m = Value::Obj(it.import_module("_py_warnings")?);
        it.get_attr_str(&m, name)
    }

    /// `warnings.warn(msg, category, stacklevel)` for a builtin warning category, from native code.
    pub fn warn_category(it: &mut Interp, category: &str, msg: &str, stacklevel: i64) -> R<()> {
        let m = it.import_module("_warnings")?;
        let f = it.get_attr_str(&Value::Obj(m), "warn")?;
        let cat = Value::Obj(it.exc_type(category));
        it.call(
            &f,
            vec![Value::str(msg), cat, Value::Int(stacklevel)],
            Vec::new(),
        )?;
        Ok(())
    }

    /// `PyErr_WarnExplicit(category, msg, filename, lineno, NULL, NULL)` for a builtin category:
    /// the module is the filename without `.py`, and there is no registry.
    pub fn warn_explicit_category(
        it: &mut Interp,
        category: &str,
        msg: &str,
        filename: &str,
        lineno: u32,
    ) -> R<()> {
        // Through `warnings`, not `_py_warnings`: importing it binds `_py_warnings._wm`, which compile-time
        // warnings can need before user code ever imports `warnings`.
        let w = Value::Obj(it.import_module("warnings")?);
        let f = it.get_attr_str(&w, "warn_explicit")?;
        let cat = Value::Obj(it.exc_type(category));
        let module = Value::str(filename.strip_suffix(".py").unwrap_or(filename));
        it.call(
            &f,
            vec![
                Value::str(msg),
                cat,
                Value::str(filename),
                Value::Int(lineno as i64),
                module,
            ],
            Vec::new(),
        )?;
        Ok(())
    }

    /// Issue a warning, or maybe ignore it or raise an exception.
    ///
    ///   message
    ///     Text of the warning message.
    ///   category
    ///     The Warning category subclass. Defaults to UserWarning.
    ///   stacklevel
    ///     How far up the call stack to make this warning appear. A value of 2 for
    ///     example attributes the warning to the caller of the code calling warn().
    ///   source
    ///     If supplied, the destroyed object which emitted a ResourceWarning
    ///   skip_file_prefixes
    ///     An optional tuple of module filename prefixes indicating frames to skip
    ///     during stacklevel computations for stack frame attribution.
    #[op]
    fn warn(
        it: &mut Interp,
        #[kw] message: &Value,
        #[kw] category: Option<&Value>,
        #[kw]
        #[default(1)]
        stacklevel: i64,
        #[kw] source: Option<&Value>,
        #[kwonly] skip_file_prefixes: Option<&Value>,
    ) -> R<Value> {
        let mut kwargs = Vec::new();
        if let Some(v) = skip_file_prefixes {
            if v.tuple_items().is_none() {
                let t = it.type_name_of(v);
                return Err(it.type_error(&format!(
                    "warn() argument 'skip_file_prefixes' must be tuple, not {t}"
                )));
            }
            kwargs.push((it.str_obj("skip_file_prefixes"), v.clone()));
        }
        let f = py_warnings(it, "warn")?;
        let args = vec![
            message.clone(),
            category.cloned().unwrap_or(Value::None),
            Value::Int(stacklevel),
            source.cloned().unwrap_or(Value::None),
        ];
        it.call(&f, args, kwargs)
    }

    /// Issue a warning, or maybe ignore it or raise an exception.
    #[op]
    #[allow(clippy::too_many_arguments)]
    fn warn_explicit(
        it: &mut Interp,
        #[kw] message: &Value,
        #[kw] category: &Value,
        #[kw] filename: &Value,
        #[kw] lineno: i64,
        #[kw] module: Option<&Value>,
        #[kw] registry: Option<&Value>,
        #[kw] module_globals: Option<&Value>,
        #[kw] source: Option<&Value>,
    ) -> R<Value> {
        let f = py_warnings(it, "warn_explicit")?;
        let args = vec![
            message.clone(),
            category.clone(),
            filename.clone(),
            Value::Int(lineno),
            module.cloned().unwrap_or(Value::None),
            registry.cloned().unwrap_or(Value::None),
            module_globals.cloned().unwrap_or(Value::None),
            source.cloned().unwrap_or(Value::None),
        ];
        it.call(&f, args, Vec::new())
    }

    fn lock_call(it: &mut Interp, method: &str) -> R<()> {
        let lock = py_warnings(it, "_lock")?;
        it.call_method(&lock, method, Vec::new())?;
        Ok(())
    }

    #[op]
    fn _acquire_lock(it: &mut Interp) -> R<()> {
        lock_call(it, "acquire")
    }

    #[op]
    fn _release_lock(it: &mut Interp) -> R<()> {
        lock_call(it, "release")
    }

    #[op]
    fn _filters_mutated_lock_held(it: &mut Interp) -> R<()> {
        let f = py_warnings(it, "_filters_mutated_lock_held")?;
        it.call(&f, Vec::new(), Vec::new())?;
        Ok(())
    }

    #[init]
    fn init(it: &mut Interp, m: &Value) -> R<()> {
        let Value::Obj(m) = m else { return Ok(()) };
        let d = it.module_dict(m);
        let default_filters = [
            ("default", "DeprecationWarning", Some("__main__")),
            ("ignore", "DeprecationWarning", None),
            ("ignore", "PendingDeprecationWarning", None),
            ("ignore", "ImportWarning", None),
            ("ignore", "ResourceWarning", None),
        ];
        let filters: Vec<Value> = default_filters
            .iter()
            .map(|(action, cat, module)| {
                let module = module.map(Value::str).unwrap_or(Value::None);
                Value::tuple(vec![
                    Value::str(action),
                    Value::None,
                    Value::Obj(it.exc_types[cat].clone()),
                    module,
                    Value::Int(0),
                ])
            })
            .collect();
        dict_set_str(&d, "filters", Value::list(filters));
        dict_set_str(&d, "_defaultaction", Value::str("default"));
        dict_set_str(&d, "_onceregistry", Value::Obj(it.new_dict()));
        let cv = Value::Obj(it.import_module("_contextvars")?);
        let var = it.get_attr_str(&cv, "ContextVar")?;
        let ctx = it.call(&var, vec![Value::str("_warnings_context")], Vec::new())?;
        dict_set_str(&d, "_warnings_context", ctx);
        Ok(())
    }
}
