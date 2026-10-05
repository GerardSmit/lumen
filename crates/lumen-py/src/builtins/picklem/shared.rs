//! State and lookups the pickler and unpickler share: the `copyreg` and `_compat_pickle` tables,
//! the three exception classes, and the attribute-path helpers of `_pickle.c`.

use crate::object::*;
use crate::vm::{dict_get_str, Interp};
use std::rc::Rc;

pub struct Shared {
    pub dispatch_table: Obj,
    pub ext_registry: Obj,
    pub ext_cache: Obj,
    pub inverted: Obj,
    pub name_2to3: Obj,
    pub import_2to3: Obj,
    pub name_3to2: Obj,
    pub import_3to2: Obj,
    pub codecs_encode: Value,
    pub getattr: Value,
    pub partial: Value,
    pub pickling_error: Obj,
    pub unpickling_error: Obj,
}

/// Per-interpreter module state: the exception classes (set when the module is created) and the
/// tables loaded on first use.
#[derive(Default)]
pub struct ModState {
    pub errors: Option<(Obj, Obj, Obj)>,
    pub shared: Option<Rc<Shared>>,
}

fn module_attr(it: &mut Interp, module: &str, name: &str) -> R<Value> {
    let m = it.import_module(module)?;
    it.get_attr_str(&Value::Obj(m), name)
}

fn dict_attr(it: &mut Interp, module: &str, name: &str, what: &str) -> R<Obj> {
    match module_attr(it, module, name)? {
        Value::Obj(o) if matches!(o.kind, Kind::Dict(_)) => Ok(o),
        other => {
            let t = it.tp_name_of(&other);
            Err(it.runtime_error(&format!("{} should be a dict, not {}", what, t)))
        }
    }
}

pub fn shared(it: &mut Interp) -> R<Rc<Shared>> {
    if let Some(s) = it.native_state::<ModState>().shared.clone() {
        return Ok(s);
    }
    let Some((_, pickling_error, unpickling_error)) = it.native_state::<ModState>().errors.clone() else {
        return Err(it.runtime_error("_pickle module state is not initialised"));
    };
    let sh = Rc::new(Shared {
        dispatch_table: dict_attr(it, "copyreg", "dispatch_table", "copyreg.dispatch_table")?,
        ext_registry: dict_attr(it, "copyreg", "_extension_registry", "copyreg._extension_registry")?,
        ext_cache: dict_attr(it, "copyreg", "_extension_cache", "copyreg._extension_cache")?,
        inverted: dict_attr(it, "copyreg", "_inverted_registry", "copyreg._inverted_registry")?,
        name_2to3: dict_attr(it, "_compat_pickle", "NAME_MAPPING", "_compat_pickle.NAME_MAPPING")?,
        import_2to3: dict_attr(it, "_compat_pickle", "IMPORT_MAPPING", "_compat_pickle.IMPORT_MAPPING")?,
        name_3to2: dict_attr(it, "_compat_pickle", "REVERSE_NAME_MAPPING", "_compat_pickle.REVERSE_NAME_MAPPING")?,
        import_3to2: dict_attr(it, "_compat_pickle", "REVERSE_IMPORT_MAPPING", "_compat_pickle.REVERSE_IMPORT_MAPPING")?,
        codecs_encode: module_attr(it, "codecs", "encode")?,
        getattr: dict_get_str(&it.builtins, "getattr").unwrap_or(Value::None),
        partial: module_attr(it, "functools", "partial")?,
        pickling_error,
        unpickling_error,
    });
    it.native_state::<ModState>().shared = Some(sh.clone());
    Ok(sh)
}

pub fn pickling_error(it: &mut Interp, sh: &Shared, msg: &str) -> Obj {
    it.new_exc(&sh.pickling_error, vec![Value::str(msg)])
}

pub fn unpickling_error(it: &mut Interp, sh: &Shared, msg: &str) -> Obj {
    it.new_exc(&sh.unpickling_error, vec![Value::str(msg)])
}

/// `getattr(v, name)`, or `None` when the attribute does not exist.
pub fn attr_opt(it: &mut Interp, v: &Value, name: &str) -> R<Option<Value>> {
    match it.get_attr_str(v, name) {
        Ok(x) => Ok(Some(x)),
        Err(e) if it.exc_is(&e, "AttributeError") => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn flag(it: &mut Interp, v: Option<&Value>, default: bool) -> R<bool> {
    match v {
        None => Ok(default),
        Some(v) => it.truthy(v),
    }
}

/// An argument that must be a `str` without NUL characters (a clinic `const char *`).
pub fn str_arg(it: &mut Interp, v: Option<&Value>, default: &str, func: &str, arg: &str) -> R<String> {
    let Some(v) = v else { return Ok(default.to_string()) };
    match v.as_str() {
        Some(s) if s.contains('\0') => Err(it.value_error("embedded null character")),
        Some(s) => Ok(s.to_string()),
        None => {
            let t = it.tp_name_of(v);
            Err(it.type_error(&format!("{}() argument '{}' must be str, not {}", func, arg, t)))
        }
    }
}

/// Whether `v` supports `next()`: an iterator or generator, or any object whose type defines
/// `__next__`.
pub fn is_iter(it: &Interp, v: &Value) -> bool {
    let Value::Obj(o) = v else { return false };
    if matches!(o.kind, Kind::Iter(_) | Kind::Generator(_)) {
        return true;
    }
    let cls = it.type_of_obj(o);
    it.lookup_mro(&cls, "__next__").is_some()
}

/// A callable looked up as an attribute of `me`: a bound method of `me` is kept unbound (and
/// called with `me`), so that `me` does not own a reference to itself.
pub type MethodRef = (Value, bool);

pub fn split_bound(f: Value, me: &Value) -> MethodRef {
    if let Value::Obj(o) = &f {
        if let Kind::Method(func, this) = &o.kind {
            if this.is(me) {
                return (func.clone(), true);
            }
        }
    }
    (f, false)
}

pub fn call_bound(it: &mut Interp, f: &MethodRef, me: Option<&Value>, arg: Value) -> R<Value> {
    match (f.1, me) {
        (true, Some(m)) => it.call(&f.0, vec![m.clone(), arg], Vec::new()),
        _ => it.call(&f.0, vec![arg], Vec::new()),
    }
}

pub fn rebuild_bound(f: &MethodRef, me: &Value) -> Value {
    if f.1 {
        Value::Obj(Object::new(Kind::Method(f.0.clone(), me.clone())))
    } else {
        f.0.clone()
    }
}

/// `name` split at dots, refusing `<locals>` components.
pub fn dotted_path(it: &mut Interp, name: &Value, obj: Option<&Value>) -> R<Vec<String>> {
    let Some(s) = name.as_str() else {
        let t = it.tp_name_of(name);
        return Err(it.type_error(&format!("descriptor 'split' for 'str' objects doesn't apply to a '{}' object", t)));
    };
    let parts: Vec<String> = s.split('.').map(str::to_string).collect();
    if parts.iter().any(|p| p == "<locals>") {
        let n = it.repr_of(name)?;
        let msg = match obj {
            None => format!("Can't get local object {}", n),
            Some(o) => {
                let r = it.repr_of(o)?;
                format!("Can't get local attribute {} on {}", n, r)
            }
        };
        return Err(it.new_exc_str("AttributeError", &msg));
    }
    Ok(parts)
}

/// The value reached by following `names` from `obj`, with the object holding the last
/// attribute; `None` when an attribute is missing.
pub fn deep_attribute(it: &mut Interp, obj: &Value, names: &[String]) -> R<Option<(Value, Value)>> {
    let mut cur = obj.clone();
    let mut parent = obj.clone();
    for n in names {
        let holder = cur.clone();
        match attr_opt(it, &holder, n)? {
            Some(v) => {
                parent = holder;
                cur = v;
            }
            None => return Ok(None),
        }
    }
    Ok(Some((cur, parent)))
}

/// `getattribute` of `_pickle.c`: a (possibly dotted) attribute of `obj`.
pub fn getattribute(it: &mut Interp, obj: &Value, name: &Value, allow_qualname: bool) -> R<Value> {
    let found = if allow_qualname {
        let path = dotted_path(it, name, Some(obj))?;
        deep_attribute(it, obj, &path)?.map(|x| x.0)
    } else {
        match name.as_str() {
            Some(n) => attr_opt(it, obj, n)?,
            None => {
                let t = it.tp_name_of(name);
                return Err(it.type_error(&format!("attribute name must be string, not '{}'", t)));
            }
        }
    };
    match found {
        Some(v) => Ok(v),
        None => {
            let n = it.repr_of(name)?;
            let o = it.repr_of(obj)?;
            Err(it.new_exc_str("AttributeError", &format!("Can't get attribute {} on {}", n, o)))
        }
    }
}

/// The keyword arguments of a call from a dict with `str` keys.
pub fn dict_to_kw(it: &mut Interp, d: &Value) -> R<Vec<(Obj, Value)>> {
    let Some(pd) = dict_of(d) else { return Ok(Vec::new()) };
    let entries: Vec<(Value, Value)> = pd.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect();
    let mut out = Vec::with_capacity(entries.len());
    for (k, v) in entries {
        match k {
            Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => out.push((o, v)),
            _ => return Err(it.type_error("keywords must be strings")),
        }
    }
    Ok(out)
}

pub fn new_dict() -> Obj {
    Object::new(Kind::Dict(std::cell::RefCell::new(crate::dict::PyDict::new())))
}
