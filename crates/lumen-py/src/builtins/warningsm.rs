//! `_warnings`: the native core that `warnings.py` falls back on for `warn` and `warn_explicit`.

pub use _warnings::{warn_category, warn_explicit_category};

/// _warnings provides basic warning filtering support.
/// It is a helper module to speed up interpreter start-up.
#[lumen_bind::module(name = "_warnings")]
pub mod _warnings {
    use crate::object::*;
    use crate::vm::*;

    fn warnings_module(it: &mut Interp) -> Option<Value> {
        dict_get_str(&it.modules, "warnings")
    }

    fn state_dict(it: &mut Interp) -> Obj {
        match dict_get_str(&it.modules, "_warnings") {
            Some(Value::Obj(m)) => it.module_dict(&m),
            _ => unreachable!("_warnings is loaded"),
        }
    }

    fn setting(it: &mut Interp, name: &str) -> R<Value> {
        if let Some(w) = warnings_module(it) {
            if let Ok(v) = it.get_attr_str(&w, name) {
                return Ok(v);
            }
        }
        let d = state_dict(it);
        Ok(dict_get_str(&d, name).unwrap_or(Value::None))
    }

    fn category_of(it: &mut Interp, message: &Value, category: &Value) -> R<(Value, Value)> {
        let warning = Value::Obj(it.exc_types["Warning"].clone());
        if it.isinstance_value(message, &warning)? {
            return Ok((message.clone(), Value::Obj(it.type_of(message))));
        }
        let cat = if category.is_none() { Value::Obj(it.exc_types["UserWarning"].clone()) } else { category.clone() };
        if !matches!(&cat, Value::Obj(c) if matches!(c.kind, Kind::Type(_))) || !it.issubclass_value(&cat, &warning)? {
            let r = it.repr_of(&cat)?;
            return Err(it.type_error(&format!("category must be a Warning subclass, not '{}'", r.trim_matches('\''))));
        }
        let inst = it.call(&cat, vec![message.clone()], Vec::new())?;
        Ok((inst, cat))
    }

    fn matches_opt(it: &mut Interp, pat: &Value, text: &Value) -> R<bool> {
        if pat.is_none() {
            return Ok(true);
        }
        // `check_matched`: an exact `str` is one of the internal default filters, matched by equality.
        if let Value::Obj(o) = pat {
            if matches!(o.kind, Kind::Str(_)) && o.cls.is_none() {
                return it.values_eq(pat, text);
            }
        }
        let m = it.call_method(pat, "match", vec![text.clone()])?;
        it.truthy(&m)
    }

    fn get_action(it: &mut Interp, text: &Value, category: &Value, module: &Value, lineno: i64) -> R<Value> {
        let filters = setting(it, "filters")?;
        for f in it.iterate_to_vec(&filters)? {
            let Some(t) = f.tuple_items() else { continue };
            if t.len() != 5 {
                continue;
            }
            let ln = it.index_of(&t[4])?;
            if matches_opt(it, &t[1], text)? && it.issubclass_value(category, &t[2])? && matches_opt(it, &t[3], module)? && (ln == 0 || ln == lineno) {
                return Ok(t[0].clone());
            }
        }
        setting(it, "defaultaction")
    }

    fn emit(it: &mut Interp, message: &Value, category: &Value, filename: &Value, lineno: i64, source: &Value) -> R<()> {
        if let Some(w) = warnings_module(it) {
            if let (Ok(wm), Ok(show)) = (it.get_attr_str(&w, "WarningMessage"), it.get_attr_str(&w, "_showwarnmsg")) {
                let msg = it.call(&wm, vec![message.clone(), category.clone(), filename.clone(), Value::Int(lineno), Value::None, Value::None, source.clone()], Vec::new())?;
                it.call(&show, vec![msg], Vec::new())?;
                return Ok(());
            }
        }
        let name = it.get_attr_str(category, "__name__")?;
        let file = it.str_of(filename)?;
        let mut line = format!("{}:{}: {}: {}\n", file, lineno, it.str_of(&name)?, it.str_of(message)?);
        if let Some(src) = u32::try_from(lineno).ok().and_then(|n| it.source_line(&file, n)) {
            line.push_str(&format!("  {src}\n"));
        }
        it.write_stderr(&line);
        Ok(())
    }

    fn warn_explicit_impl(it: &mut Interp, message: &Value, category: &Value, filename: &Value, lineno: i64, module: &Value, registry: &Value, source: &Value) -> R<()> {
        let (inst, cat) = category_of(it, message, category)?;
        let text = if matches!(&inst, Value::Obj(_)) && !it.isinstance_value(message, &Value::Obj(it.types.str_.clone()))? { Value::string(it.str_of(&inst)?) } else { message.clone() };
        let key = Value::tuple(vec![text.clone(), cat.clone(), Value::Int(lineno)]);
        let has_registry = !registry.is_none();
        if has_registry {
            let already = it.call_method(registry, "get", vec![key.clone()])?;
            if it.truthy(&already)? {
                return Ok(());
            }
        }
        let action = get_action(it, &text, &cat, module, lineno)?;
        let action = it.str_of(&action)?;
        match action.as_str() {
            "ignore" => Ok(()),
            "error" => Err(it.do_raise(Some(inst), None)?),
            "once" | "module" | "default" | "always" => {
                if action != "always" && has_registry {
                    match action.as_str() {
                        "once" => {
                            let once = setting(it, "onceregistry")?;
                            let okey = Value::tuple(vec![text.clone(), cat.clone()]);
                            let seen = it.call_method(&once, "get", vec![okey.clone()])?;
                            if it.truthy(&seen)? {
                                return Ok(());
                            }
                            it.setitem(&once, okey, Value::Bool(true))?;
                        }
                        "module" => {
                            let mkey = Value::tuple(vec![text.clone(), cat.clone(), Value::Int(0)]);
                            let seen = it.call_method(registry, "get", vec![mkey.clone()])?;
                            if it.truthy(&seen)? {
                                return Ok(());
                            }
                            it.setitem(registry, mkey, Value::Bool(true))?;
                            it.setitem(registry, key, Value::Bool(true))?;
                        }
                        _ => it.setitem(registry, key, Value::Bool(true))?,
                    }
                }
                emit(it, &inst, &cat, filename, lineno, source)
            }
            other => {
                let msg = format!("Unrecognized action ({:?}) in warnings.filters", other);
                Err(it.new_exc_str("RuntimeError", &msg))
            }
        }
    }

    /// `warnings.warn(msg, category, stacklevel)` for a builtin warning category, from native code.
    pub fn warn_category(it: &mut Interp, category: &str, msg: &str, stacklevel: i64) -> R<()> {
        let m = it.import_module("_warnings")?;
        let f = it.get_attr_str(&Value::Obj(m), "warn")?;
        let cat = Value::Obj(it.exc_type(category));
        it.call(&f, vec![Value::str(msg), cat, Value::Int(stacklevel)], Vec::new())?;
        Ok(())
    }

    /// `PyErr_WarnExplicit(category, msg, filename, lineno, NULL, NULL)` for a builtin category:
    /// the module is the filename without `.py`, and there is no registry.
    pub fn warn_explicit_category(it: &mut Interp, category: &str, msg: &str, filename: &str, lineno: u32) -> R<()> {
        it.import_module("_warnings")?;
        let cat = Value::Obj(it.exc_type(category));
        let module = Value::str(filename.strip_suffix(".py").unwrap_or(filename));
        warn_explicit_impl(it, &Value::str(msg), &cat, &Value::str(filename), lineno as i64, &module, &Value::None, &Value::None)
    }

    fn skipped(it: &Interp, frame: usize, prefixes: &[Value]) -> bool {
        let name = &it.frames[frame].code.filename;
        prefixes.iter().any(|p| p.as_str().is_some_and(|p| name.starts_with(p)))
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
        let prefixes = match skip_file_prefixes {
            None => Vec::new(),
            Some(v) => match v.tuple_items() {
                Some(t) => t.to_vec(),
                None => {
                    let t = it.type_name_of(v);
                    return Err(it.type_error(&format!("warn() argument 'skip_file_prefixes' must be tuple, not {t}")));
                }
            },
        };
        let mut stacklevel = stacklevel;
        if !prefixes.is_empty() && stacklevel < 2 {
            stacklevel = 2;
        }
        let mut frame = it.frames.len().checked_sub(1);
        while stacklevel > 1 {
            stacklevel -= 1;
            frame = frame.and_then(|f| f.checked_sub(1));
            while let Some(f) = frame {
                if !skipped(it, f, &prefixes) {
                    break;
                }
                frame = f.checked_sub(1);
            }
        }
        let (filename, lineno, globals) = match frame {
            Some(i) => {
                let f = &it.frames[i];
                (Value::str(&f.code.filename), f.code.line_at(f.pc.saturating_sub(1)) as i64, Some(f.globals.clone()))
            }
            None => (Value::str("sys"), 1, None),
        };
        let (module, registry) = match globals {
            Some(g) => {
                let module = dict_get_str(&g, "__name__").unwrap_or_else(|| Value::str("<string>"));
                let registry = match dict_get_str(&g, "__warningregistry__") {
                    Some(r) => r,
                    None => {
                        let r = Value::Obj(it.new_dict());
                        dict_set_str(&g, "__warningregistry__", r.clone());
                        r
                    }
                };
                (module, registry)
            }
            None => (Value::str("sys"), Value::None),
        };
        let category = category.cloned().unwrap_or(Value::None);
        let source = source.cloned().unwrap_or(Value::None);
        warn_explicit_impl(it, message, &category, &filename, lineno, &module, &registry, &source)?;
        Ok(Value::None)
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
        let _ = module_globals;
        let module = match module {
            Some(m) if !m.is_none() => m.clone(),
            _ => {
                let s = it.str_of(filename)?;
                Value::string(s.strip_suffix(".py").unwrap_or(&s).to_string())
            }
        };
        let none = Value::None;
        warn_explicit_impl(it, message, category, filename, lineno, &module, registry.unwrap_or(&none), source.unwrap_or(&none))?;
        Ok(Value::None)
    }

    #[op]
    fn _filters_mutated() {}

    #[init]
    fn init(it: &mut Interp, m: &Value) {
        let Value::Obj(m) = m else { return };
        let d = it.module_dict(m);
        let default_filters = [("default", "DeprecationWarning", Some("__main__")), ("ignore", "DeprecationWarning", None), ("ignore", "PendingDeprecationWarning", None), ("ignore", "ImportWarning", None), ("ignore", "ResourceWarning", None)];
        let filters: Vec<Value> = default_filters
            .iter()
            .map(|(action, cat, module)| {
                let module = module.map(Value::str).unwrap_or(Value::None);
                Value::tuple(vec![Value::str(action), Value::None, Value::Obj(it.exc_types[cat].clone()), module, Value::Int(0)])
            })
            .collect();
        dict_set_str(&d, "filters", Value::list(filters));
        dict_set_str(&d, "_defaultaction", Value::str("default"));
        dict_set_str(&d, "defaultaction", Value::str("default"));
        let once = Value::Obj(it.new_dict());
        dict_set_str(&d, "_onceregistry", once.clone());
        dict_set_str(&d, "onceregistry", once);
    }
}
