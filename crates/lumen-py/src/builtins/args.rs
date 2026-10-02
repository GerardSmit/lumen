//! Argument checking and conversion helpers shared by the native functions.

use crate::object::*;
use crate::pyint::PyInt;
use crate::vm::*;
use std::cell::RefCell;

pub fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

pub fn kw_name(k: &Obj) -> &str {
    k.as_str_kind().unwrap_or("")
}

impl Interp {
    pub fn arity_error(&mut self, fname: &str, given: usize, min: usize, max: usize) -> Obj {
        let msg = if min == max {
            let w = if min == 1 { "exactly one argument".to_string() } else { format!("exactly {} arguments", min) };
            format!("{}() takes {} ({} given)", fname, w, given)
        } else if given < min {
            format!("{}() takes at least {} argument{} ({} given)", fname, min, plural(min), given)
        } else {
            format!("{}() takes at most {} argument{} ({} given)", fname, max, plural(max), given)
        };
        self.type_error(&msg)
    }

    pub fn check_args(&mut self, fname: &str, args: &[Value], min: usize, max: usize) -> R<()> {
        if args.len() < min || args.len() > max {
            return Err(self.arity_error(fname, args.len(), min, max));
        }
        Ok(())
    }

    pub fn no_kwargs(&mut self, fname: &str, kw: &[(Obj, Value)]) -> R<()> {
        if !kw.is_empty() {
            return Err(self.type_error(&format!("{}() takes no keyword arguments", fname)));
        }
        Ok(())
    }

    /// Binds positionals then keywords to `names`; `required` leading parameters must be present.
    pub fn bind_args(&mut self, fname: &str, args: &[Value], kw: &[(Obj, Value)], names: &[&str], required: usize) -> R<Vec<Option<Value>>> {
        let mut out: Vec<Option<Value>> = vec![None; names.len()];
        if args.len() > names.len() {
            return Err(self.arity_error(fname, args.len(), required, names.len()));
        }
        for (i, a) in args.iter().enumerate() {
            out[i] = Some(a.clone());
        }
        for (k, v) in kw {
            let kn = kw_name(k);
            match names.iter().position(|n| *n == kn) {
                Some(i) => {
                    if out[i].is_some() {
                        return Err(self.type_error(&format!("argument for {}() given by name ('{}') and position ({})", fname, kn, i + 1)));
                    }
                    out[i] = Some(v.clone());
                }
                None => return Err(self.type_error(&format!("{}() got an unexpected keyword argument '{}'", fname, kn))),
            }
        }
        for (i, n) in names.iter().enumerate().take(required) {
            if out[i].is_none() {
                return Err(self.type_error(&format!("{}() missing required argument '{}' (pos {})", fname, n, i + 1)));
            }
        }
        Ok(out)
    }

    pub fn int_arg(&mut self, v: &Value) -> R<i64> {
        self.index_of(v)
    }

    pub fn str_arg(&mut self, v: &Value, what: &str) -> R<String> {
        match v.as_str() {
            Some(s) => Ok(s.to_string()),
            None => {
                let t = self.type_name_of(v);
                Err(self.type_error(&format!("{} must be str, not {}", what, t)))
            }
        }
    }

    pub fn float_arg(&mut self, v: &Value) -> R<f64> {
        match v {
            Value::Float(f) => Ok(*f),
            Value::Int(i) => Ok(*i as f64),
            Value::Bool(b) => Ok(*b as i64 as f64),
            Value::Obj(o) => match &o.kind {
                Kind::Float(f) => Ok(*f),
                Kind::Int(b) => match b.to_float() {
                    Some(f) => Ok(f),
                    None => Err(self.overflow_err("int too large to convert to float")),
                },
                _ => self.float_via_dunder(v),
            },
            _ => self.float_via_dunder(v),
        }
    }

    fn float_via_dunder(&mut self, v: &Value) -> R<f64> {
        let cls = self.type_of(v);
        if let Some(m) = self.lookup_mro(&cls, "__float__") {
            let b = self.bind_descr(&m, v, &cls)?;
            let r = self.call(&b, Vec::new(), Vec::new())?;
            return match r {
                Value::Float(f) => Ok(f),
                Value::Obj(o) if matches!(o.kind, Kind::Float(_)) => {
                    if o.cls.is_some() {
                        let (tn, t) = (self.type_name_of(v), self.type_name_of(&Value::Obj(o.clone())));
                        let msg = format!(
                            "{tn}.__float__ returned non-float (type {t}).  The ability to return an instance of a strict subclass of float is deprecated, and may be removed in a future version of Python."
                        );
                        crate::builtins::warningsm::warn_category(self, "DeprecationWarning", &msg, 1)?;
                    }
                    match &o.kind {
                        Kind::Float(f) => Ok(*f),
                        _ => Ok(0.0),
                    }
                }
                _ => {
                    let t = self.type_name_of(&r);
                    Err(self.type_error(&format!("{}.__float__ returned non-float (type {})", self.type_name_of(v), t)))
                }
            };
        }
        if self.has_index(v) {
            let i = crate::bind::index(self, v)?;
            return self.float_arg(&i);
        }
        let t = self.type_name_of(v);
        Err(self.type_error(&format!("must be real number, not {}", t)))
    }

    /// Calls `sys.displayhook(v)`, as an interactive expression statement does.
    pub fn display_hook(&mut self, v: Value) -> R<Value> {
        let hook = self.sys_module.clone().and_then(|m| dict_get_str(&self.module_dict(&m), "displayhook"));
        match hook {
            Some(h) => self.call(&h, vec![v], Vec::new()),
            None => Err(self.runtime_error("lost sys.displayhook")),
        }
    }

    /// CPython's `PyComplex_AsCComplex`: a complex, the result of `__complex__`, or a real number.
    pub fn complex_arg(&mut self, v: &Value) -> R<(f64, f64)> {
        if let Value::Obj(o) = v {
            if let Kind::Complex(r, i) = &o.kind {
                if o.cls.is_none() {
                    return Ok((*r, *i));
                }
            }
        }
        let cls = self.type_of(v);
        if let Some(m) = self.lookup_mro(&cls, "__complex__") {
            let b = self.bind_descr(&m, v, &cls)?;
            let r = self.call(&b, Vec::new(), Vec::new())?;
            return match &r {
                Value::Obj(o) if matches!(o.kind, Kind::Complex(..)) => {
                    if o.cls.is_some() {
                        let t = self.type_name_of(&r);
                        let msg = format!(
                            "__complex__ returned non-complex (type {t}).  The ability to return an instance of a strict subclass of complex is deprecated, and may be removed in a future version of Python."
                        );
                        crate::builtins::warningsm::warn_category(self, "DeprecationWarning", &msg, 1)?;
                    }
                    match &o.kind {
                        Kind::Complex(re, im) => Ok((*re, *im)),
                        _ => unreachable!(),
                    }
                }
                _ => {
                    let t = self.type_name_of(&r);
                    Err(self.type_error(&format!("__complex__ returned non-complex (type {})", t)))
                }
            };
        }
        if let Value::Obj(o) = v {
            if let Kind::Complex(r, i) = &o.kind {
                return Ok((*r, *i));
            }
        }
        Ok((self.float_arg(v)?, 0.0))
    }

    pub fn list_val(&self, v: Vec<Value>) -> Value {
        Value::list(v)
    }

    pub fn new_dict(&self) -> Obj {
        Object::new(Kind::Dict(RefCell::new(crate::dict::PyDict::new())))
    }

    pub fn expect_self_kind(&mut self, v: &Value, ty: &str, meth: &str) -> R<()> {
        let ok = match (ty, v) {
            ("str", _) => v.as_str().is_some(),
            ("list", _) => list_of(v).is_some(),
            ("dict", _) => dict_of(v).is_some(),
            ("set", _) => matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Set(_))),
            _ => true,
        };
        if !ok {
            let t = self.type_name_of(v);
            return Err(self.type_error(&format!("descriptor '{}' for '{}' objects doesn't apply to a '{}' object", meth, ty, t)));
        }
        Ok(())
    }

    pub fn str_obj(&self, s: &str) -> Obj {
        match Value::str(s) {
            Value::Obj(o) => o,
            _ => unreachable!(),
        }
    }

    pub fn kw_get<'a>(kw: &'a [(Obj, Value)], name: &str) -> Option<&'a Value> {
        kw.iter().find(|(k, _)| kw_name(k) == name).map(|(_, v)| v)
    }

    pub fn kwargs_to_dict(&mut self, kw: &[(Obj, Value)]) -> R<Obj> {
        let d = self.new_dict();
        for (k, v) in kw {
            self.dict_set(&d, Value::Obj(k.clone()), v.clone())?;
        }
        Ok(d)
    }
}
