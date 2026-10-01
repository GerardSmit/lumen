//! `_warnings`: the native core that `warnings.py` falls back on for `warn` and `warn_explicit`.

use super::native::*;
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
    let m = it.call_method(pat, "match", vec![text.clone()])?;
    Ok(!m.is_none())
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
    let line = format!("{}:{}: {}: {}\n", it.str_of(filename)?, lineno, it.str_of(&name)?, it.str_of(message)?);
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

fn warn(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("warn", a, kw, &["message", "category", "stacklevel", "source"], 1)?;
    let message = b[0].clone().unwrap();
    let category = b[1].clone().unwrap_or(Value::None);
    let stacklevel = match &b[2] {
        Some(v) => it.index_of(v)?.max(1) as usize,
        None => 1,
    };
    let source = b[3].clone().unwrap_or(Value::None);
    let n = it.frames.len();
    let (filename, lineno, globals) = if stacklevel <= n && n > 0 {
        let f = &it.frames[n - stacklevel];
        (Value::str(&f.code.filename), f.code.line_at(f.pc.saturating_sub(1)) as i64, Some(f.globals.clone()))
    } else {
        (Value::str("sys"), 1, None)
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
    warn_explicit_impl(it, &message, &category, &filename, lineno, &module, &registry, &source)?;
    Ok(Value::None)
}

fn warn_explicit(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("warn_explicit", a, kw, &["message", "category", "filename", "lineno", "module", "registry", "module_globals", "source"], 4)?;
    let get = |i: usize| b[i].clone().unwrap_or(Value::None);
    let lineno = it.index_of(&get(3))?;
    let filename = get(2);
    let module = if get(4).is_none() {
        let s = it.str_of(&filename)?;
        Value::string(s.strip_suffix(".py").unwrap_or(&s).to_string())
    } else {
        get(4)
    };
    warn_explicit_impl(it, &get(0), &get(1), &filename, lineno, &module, &get(5), &get(7))?;
    Ok(Value::None)
}

fn noop(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::None)
}

pub fn make(it: &mut Interp) -> Obj {
    let m = it.new_module("_warnings");
    let d = it.module_dict(&m);
    it.register_module("_warnings", &m);
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
    dict_set_str(&d, "_onceregistry", Value::Obj(it.new_dict()));
    dict_set_str(&d, "onceregistry", dict_get_str(&d, "_onceregistry").unwrap());
    set_fn(it, &d, "warn", warn);
    set_fn(it, &d, "warn_explicit", warn_explicit);
    set_fn(it, &d, "_filters_mutated", noop);
    set_fn(it, &d, "_warn_unawaited_coroutine", noop);
    m
}
