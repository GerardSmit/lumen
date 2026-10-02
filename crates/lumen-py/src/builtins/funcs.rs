//! Builtin functions.

use crate::ast::{BinOp, StmtKind};
use crate::fmath;
use crate::pyint::{BigInt, PyInt};
use crate::bytecode::UnOp;
use crate::num::{to_num, Num};
use crate::object::*;
use crate::vm::*;
use std::rc::Rc;

type Kw<'a> = &'a [(Obj, Value)];

fn print(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let mut sep = " ".to_string();
    let mut end = "\n".to_string();
    let mut file = Value::None;
    let mut flush = false;
    for (k, v) in kw {
        match k.as_str_kind().unwrap_or("") {
            "sep" => {
                if !v.is_none() {
                    match v.as_str() {
                        Some(s) => sep = s.to_string(),
                        None => {
                            let t = it.type_name_of(v);
                            return Err(it.type_error(&format!("sep must be None or a string, not {}", t)));
                        }
                    }
                }
            }
            "end" => {
                if !v.is_none() {
                    match v.as_str() {
                        Some(s) => end = s.to_string(),
                        None => {
                            let t = it.type_name_of(v);
                            return Err(it.type_error(&format!("end must be None or a string, not {}", t)));
                        }
                    }
                }
            }
            "file" => file = v.clone(),
            "flush" => flush = it.truthy(v)?,
            other => return Err(it.type_error(&format!("'{}' is an invalid keyword argument for print()", other))),
        }
    }
    if file.is_none() {
        match it.sys_attr("stdout") {
            Some(f) if !f.is_none() => file = f,
            _ => return Ok(Value::None),
        }
    }
    let native = super::iom::textio::is_native_textio(it, &file);
    if native {
        let mut out = String::new();
        for (i, v) in a.iter().enumerate() {
            if i > 0 {
                out.push_str(&sep);
            }
            match v.as_str() {
                Some(s) => out.push_str(s),
                None => out.push_str(&it.str_of(v)?),
            }
        }
        out.push_str(&end);
        it.write_to(&file, &out)?;
    } else {
        for (i, v) in a.iter().enumerate() {
            if i > 0 {
                it.write_to(&file, &sep)?;
            }
            match v.as_str() {
                Some(s) => it.write_to(&file, s)?,
                None => {
                    let s = it.str_of(v)?;
                    it.write_to(&file, &s)?;
                }
            }
        }
        it.write_to(&file, &end)?;
    }
    if flush {
        it.call_method(&file, "flush", Vec::new())?;
    }
    Ok(Value::None)
}

impl Interp {
    pub fn sys_attr(&mut self, name: &str) -> Option<Value> {
        let sys = self.sys_module.clone()?;
        let d = self.module_dict(&sys);
        dict_get_str(&d, name)
    }

    /// `f.write(s)`, directly for a native `TextIOWrapper`.
    pub fn write_to(&mut self, f: &Value, s: &str) -> R<()> {
        if let Some(r) = super::iom::textio::write_native(self, f, s) {
            return r.map(|_| ());
        }
        self.call_method(f, "write", vec![Value::str(s)])?;
        Ok(())
    }
}

fn len(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("len", a, 1, 1)?;
    Ok(Value::Int(it.len_of(&a[0])? as i64))
}

fn abs(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("abs", a, 1, 1)?;
    match &a[0] {
        Value::Int(i) => {
            return Ok(match i.checked_abs() {
                Some(v) => Value::Int(v),
                None => Value::big(BigInt::from_i64(*i).abs()),
            })
        }
        Value::Float(f) => return Ok(Value::Float(f.abs())),
        Value::Bool(b) => return Ok(Value::Int(*b as i64)),
        Value::Obj(o) => {
            if o.cls.is_some() {
                if let Some(m) = it.user_special(&a[0], "__abs__") {
                    return it.call_user_special(&a[0], &m, Vec::new());
                }
            }
            match &o.kind {
                Kind::Int(b) => return Ok(Value::big(b.abs())),
                Kind::Float(f) => return Ok(Value::Float(f.abs())),
                Kind::Complex(r, i) => return Ok(Value::Float(fmath::hypot(*r, *i))),
                _ => {}
            }
        }
        _ => {}
    }
    let cls = it.type_of(&a[0]);
    if let Some(m) = it.lookup_mro(&cls, "__abs__") {
        let b = it.bind_descr(&m, &a[0], &cls)?;
        return it.call(&b, Vec::new(), Vec::new());
    }
    let t = it.type_name_of(&a[0]);
    Err(it.type_error(&format!("bad operand type for abs(): '{}'", t)))
}

fn all(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("all", a, 1, 1)?;
    let iter = it.get_iter(&a[0])?;
    while let Some(v) = it.iter_next(&iter)? {
        if !it.truthy(&v)? {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

fn any(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("any", a, 1, 1)?;
    let iter = it.get_iter(&a[0])?;
    while let Some(v) = it.iter_next(&iter)? {
        if it.truthy(&v)? {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn ascii(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("ascii", a, 1, 1)?;
    let r = it.repr_of(&a[0])?;
    Ok(Value::string(crate::repr::ascii_escape(&r)))
}

fn radix_str(it: &mut Interp, v: &Value, radix: u32, prefix: &str) -> R<Value> {
    let n = match v.as_bigint() {
        Some(b) => b,
        None => {
            if it.has_index(v) {
                BigInt::from_i64(it.index_of(v)?)
            } else {
                let t = it.type_name_of(v);
                return Err(it.type_error(&format!("'{}' object cannot be interpreted as an integer", t)));
            }
        }
    };
    let s = n.abs().to_string_radix(radix);
    Ok(Value::string(format!("{}{}{}", if n.is_negative() { "-" } else { "" }, prefix, s)))
}

fn bin(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("bin", a, 1, 1)?;
    radix_str(it, &a[0], 2, "0b")
}
fn oct(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("oct", a, 1, 1)?;
    radix_str(it, &a[0], 8, "0o")
}
fn hex(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("hex", a, 1, 1)?;
    radix_str(it, &a[0], 16, "0x")
}

fn callable(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("callable", a, 1, 1)?;
    Ok(Value::Bool(match &a[0] {
        Value::Obj(o) => match &o.kind {
            Kind::Type(_) | Kind::Function(_) | Kind::Method(..) | Kind::Native(_) => true,
            _ => {
                let cls = it.type_of_obj(o);
                it.lookup_mro(&cls, "__call__").is_some()
            }
        },
        _ => false,
    }))
}

fn chr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("chr", a, 1, 1)?;
    let n = it.index_of(&a[0])?;
    match u32::try_from(n).ok().and_then(lumen_common::smuggle::code_point_str) {
        Some(c) => Ok(Value::str(&c)),
        None => Err(it.value_error("chr() arg not in range(0x110000)")),
    }
}

fn ord(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("ord", a, 1, 1)?;
    match &a[0] {
        Value::Obj(o) => match &o.kind {
            Kind::Str(s) => {
                if s.nchars == 1 {
                    Ok(Value::Int(lumen_common::smuggle::code_points(&s.s).next().unwrap_or(0) as i64))
                } else {
                    Err(it.type_error(&format!("ord() expected a character, but string of length {} found", s.nchars)))
                }
            }
            Kind::Bytes(b) if b.len() == 1 => Ok(Value::Int(b[0] as i64)),
            Kind::ByteArray(b) if b.len() == 1 => Ok(Value::Int(b.bytes()[0] as i64)),
            _ => {
                let t = it.type_name_of(&a[0]);
                Err(it.type_error(&format!("ord() expected string of length 1, but {} found", t)))
            }
        },
        v => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("ord() expected string of length 1, but {} found", t)))
        }
    }
}

fn attr_name(it: &mut Interp, v: &Value, what: &str) -> R<Obj> {
    match v {
        Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => Ok(o.clone()),
        _ => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("{}(): attribute name must be string, not '{}'", what, t)))
        }
    }
}

fn getattr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("getattr", a, 2, 3)?;
    let n = attr_name(it, &a[1], "getattr")?;
    match it.get_attr(&a[0], &n) {
        Ok(v) => Ok(v),
        Err(e) => {
            if a.len() == 3 && it.exc_is(&e, "AttributeError") {
                Ok(a[2].clone())
            } else {
                Err(e)
            }
        }
    }
}

fn hasattr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("hasattr", a, 2, 2)?;
    let n = attr_name(it, &a[1], "hasattr")?;
    match it.get_attr(&a[0], &n) {
        Ok(_) => Ok(Value::Bool(true)),
        Err(e) => {
            if it.exc_is(&e, "AttributeError") {
                Ok(Value::Bool(false))
            } else {
                Err(e)
            }
        }
    }
}

fn setattr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("setattr", a, 3, 3)?;
    let n = attr_name(it, &a[1], "setattr")?;
    it.set_attr(&a[0], &n, a[2].clone())?;
    Ok(Value::None)
}

fn delattr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("delattr", a, 2, 2)?;
    let n = attr_name(it, &a[1], "delattr")?;
    it.del_attr(&a[0], &n)?;
    Ok(Value::None)
}

fn dir(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("dir", a, 0, 1)?;
    if a.is_empty() {
        let l = locals(it, &[], &[])?;
        let keys = match &l {
            Value::Obj(o) => match &o.kind {
                Kind::Dict(d) => d.borrow().keys(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        };
        let mut names: Vec<String> = keys.iter().filter_map(|k| k.as_str().map(|s| s.to_string())).collect();
        names.sort();
        return Ok(Value::list(names.into_iter().map(Value::string).collect()));
    }
    let cls = it.type_of(&a[0]);
    if let Some(m) = it.lookup_mro(&cls, "__dir__") {
        let b = it.bind_descr(&m, &a[0], &cls)?;
        let r = it.call(&b, Vec::new(), Vec::new())?;
        let mut items = it.iterate_to_vec(&r)?;
        it.sort_values(&mut items, None, false)?;
        return Ok(Value::list(items));
    }
    Ok(Value::None)
}

fn divmod(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("divmod", a, 2, 2)?;
    if it.user_special(&a[0], "__divmod__").is_some() {
        return it.call_method(&a[0], "__divmod__", vec![a[1].clone()]);
    }
    if it.user_special(&a[1], "__rdivmod__").is_some() && it.user_special(&a[0], "__floordiv__").is_none() {
        return it.call_method(&a[1], "__rdivmod__", vec![a[0].clone()]);
    }
    if to_num(&a[0]).is_none() || to_num(&a[1]).is_none() {
        let (ta, tb) = (it.type_name_of(&a[0]), it.type_name_of(&a[1]));
        return Err(it.type_error(&format!("unsupported operand type(s) for divmod(): '{}' and '{}'", ta, tb)));
    }
    let q = it.binary_op(BinOp::FloorDiv, &a[0], &a[1])?;
    let r = it.binary_op(BinOp::Mod, &a[0], &a[1])?;
    Ok(Value::tuple(vec![q, r]))
}

fn format(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("format", a, 1, 2)?;
    let spec = match a.get(1) {
        Some(s) => match s.as_str() {
            Some(s) => s.to_string(),
            None => {
                let t = it.type_name_of(s);
                return Err(it.type_error(&format!("format() argument 2 must be str, not {}", t)));
            }
        },
        None => String::new(),
    };
    Ok(Value::string(it.format_value(&a[0], &spec)?))
}

fn frame_globals(it: &Interp) -> Obj {
    it.frames.last().map(|f| f.globals.clone()).unwrap_or_else(|| it.builtins.clone())
}

fn globals(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Obj(frame_globals(it)))
}

fn locals(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    let fr = match it.frames.last() {
        Some(f) => f,
        None => return Ok(Value::Obj(it.builtins.clone())),
    };
    if let Some(n) = &fr.names {
        return Ok(Value::Obj(n.clone()));
    }
    let d = it.new_dict();
    let code = fr.code.clone();
    let mut pairs: Vec<(Rc<str>, Value)> = Vec::new();
    for (i, n) in code.varnames.iter().enumerate() {
        if let Some(Some(v)) = fr.locals.get(i) {
            pairs.push((n.clone(), v.clone()));
        }
    }
    for (i, n) in code.cellvars.iter().chain(code.freevars.iter()).enumerate() {
        if let Some(c) = fr.cells.get(i) {
            if let Kind::Cell(v) = &c.kind {
                if let Some(x) = v.borrow().clone() {
                    pairs.push((n.clone(), x));
                }
            }
        }
    }
    for (n, v) in pairs {
        dict_set_str(&d, &n, v);
    }
    Ok(Value::Obj(d))
}

fn vars(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.check_args("vars", a, 0, 1)?;
    if a.is_empty() {
        return locals(it, a, kw);
    }
    match it.get_attr_str(&a[0], "__dict__") {
        Ok(v) => Ok(v),
        Err(e) => {
            if it.exc_is(&e, "AttributeError") {
                Err(it.type_error("vars() argument must have __dict__ attribute"))
            } else {
                Err(e)
            }
        }
    }
}

fn hash(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("hash", a, 1, 1)?;
    Ok(Value::Int(it.hash_value(&a[0])?))
}

fn id(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("id", a, 1, 1)?;
    Ok(Value::Int(it.id_of(&a[0]) as i64))
}

fn input(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("input", a, 0, 1)?;
    let mut streams = Vec::new();
    for name in ["stdin", "stdout", "stderr"] {
        match it.sys_attr(name) {
            Some(f) if !f.is_none() => streams.push(f),
            _ => return Err(it.new_exc_str("RuntimeError", &format!("input(): lost sys.{}", name))),
        }
    }
    let (fin, fout, ferr) = (&streams[0], &streams[1], &streams[2]);
    let _ = it.call_method(ferr, "flush", Vec::new());
    if let Some(p) = a.first() {
        let s = it.str_of(p)?;
        it.write_to(fout, &s)?;
    }
    it.call_method(fout, "flush", Vec::new())?;
    let line = it.call_method(fin, "readline", Vec::new())?;
    let Some(s) = line.as_str() else {
        return Err(it.type_error("object.readline() returned non-string"));
    };
    if s.is_empty() {
        return Err(it.new_exc_str("EOFError", "EOF when reading a line"));
    }
    Ok(Value::str(s.strip_suffix('\n').unwrap_or(s)))
}

fn isinstance(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("isinstance", a, 2, 2)?;
    Ok(Value::Bool(it.isinstance_value(&a[0], &a[1])?))
}

fn issubclass(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("issubclass", a, 2, 2)?;
    Ok(Value::Bool(it.issubclass_value(&a[0], &a[1])?))
}

fn iter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("iter", a, 1, 2)?;
    if a.len() == 2 {
        if !matches!(&a[0], Value::Obj(o) if matches!(o.kind, Kind::Function(_) | Kind::Method(..) | Kind::Native(_) | Kind::Type(_)) || it.lookup_mro(&it.type_of_obj(o), "__call__").is_some()) {
            return Err(it.type_error("iter(v, w): v must be callable"));
        }
        return Ok(it.mk_iter(IterState::CallIter { f: a[0].clone(), sentinel: a[1].clone(), done: false }));
    }
    it.get_iter(&a[0])
}

fn next(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("next", a, 1, 2)?;
    it.ret_val = Value::None;
    match it.iter_next(&a[0])? {
        Some(v) => Ok(v),
        None => {
            if a.len() == 2 {
                Ok(a[1].clone())
            } else {
                let v = std::mem::replace(&mut it.ret_val, Value::None);
                Err(it.stop_iteration(v))
            }
        }
    }
}

fn minmax(it: &mut Interp, a: &[Value], kw: Kw, name: &str, want_max: bool) -> R<Value> {
    let mut key = Value::None;
    let mut default: Option<Value> = None;
    for (k, v) in kw {
        match k.as_str_kind().unwrap_or("") {
            "key" => key = v.clone(),
            "default" => default = Some(v.clone()),
            other => return Err(it.type_error(&format!("'{}' is an invalid keyword argument for {}()", other, name))),
        }
    }
    let source = if a.len() == 1 {
        it.get_iter(&a[0])?
    } else if a.is_empty() {
        return Err(it.type_error(&format!("{} expected at least 1 argument, got 0", name)));
    } else {
        if default.is_some() {
            return Err(it.type_error(&format!("Cannot specify a default for {}() with multiple positional arguments", name)));
        }
        it.get_iter(&Value::tuple(a.to_vec()))?
    };
    let Some(first) = it.iter_next(&source)? else {
        return match default {
            Some(d) => Ok(d),
            None => Err(it.value_error(&format!("{}() iterable argument is empty", name))),
        };
    };
    let op = if want_max { crate::ast::CmpOp::Gt } else { crate::ast::CmpOp::Lt };
    let mut best = first;
    let mut best_key = if key.is_none() { best.clone() } else { it.call(&key, vec![best.clone()], Vec::new())? };
    while let Some(x) = it.iter_next(&source)? {
        let k = if key.is_none() { x.clone() } else { it.call(&key, vec![x.clone()], Vec::new())? };
        let r = it.compare_op(op, &k, &best_key)?;
        if it.truthy(&r)? {
            best = x;
            best_key = k;
        }
    }
    Ok(best)
}

fn max(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    minmax(it, a, kw, "max", true)
}
fn min(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    minmax(it, a, kw, "min", false)
}

fn pow(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("pow", a, kw, &["base", "exp", "mod"], 2)?;
    let (base, exp) = (b[0].clone().unwrap_or(Value::None), b[1].clone().unwrap_or(Value::None));
    match b[2].clone() {
        None | Some(Value::None) => it.binary_op(BinOp::Pow, &base, &exp),
        Some(m) => {
            if let (Some(x), Some(e), Some(md)) = (base.as_bigint(), exp.as_bigint(), m.as_bigint()) {
                return it.int_pow_mod(&x, &e, &md);
            }
            if it.user_special(&base, "__pow__").is_some() {
                return it.call_method(&base, "__pow__", vec![exp, m]);
            }
            Err(it.type_error("pow() 3rd argument not allowed unless all arguments are integers"))
        }
    }
}

fn repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("repr", a, 1, 1)?;
    Ok(Value::string(it.repr_of(&a[0])?))
}

fn round(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("round", a, kw, &["number", "ndigits"], 1)?;
    let x = b[0].clone().unwrap_or(Value::None);
    let nd = b[1].clone().filter(|v| !v.is_none());
    if let Value::Obj(o) = &x {
        if o.cls.is_some() {
            if let Some(m) = it.user_special(&x, "__round__") {
                let args = nd.map(|n| vec![n]).unwrap_or_default();
                return it.call_user_special(&x, &m, args);
            }
        }
    }
    match to_num(&x) {
        Some(Num::I(_)) | Some(Num::B(_)) => {
            let n = match nd {
                None => return Ok(match &x {
                    Value::Bool(b) => Value::Int(*b as i64),
                    _ => x,
                }),
                Some(n) => it.index_of(&n)?,
            };
            if n >= 0 {
                return Ok(x);
            }
            let big = x.as_bigint().unwrap_or_else(BigInt::zero);
            let Ok(p) = BigInt::from_i64(10).pow(&BigInt::from_i64(-n)) else {
                return Err(it.new_exc_str("MemoryError", ""));
            };
            let (q, r) = big.floor_divmod(&p);
            let twice = r.mul(&BigInt::from_i64(2));
            let cmp = twice.cmp(&p);
            let q = match cmp {
                std::cmp::Ordering::Greater => q.add(&BigInt::from_i64(1)),
                std::cmp::Ordering::Equal => {
                    if q.is_even() {
                        q
                    } else {
                        q.add(&BigInt::from_i64(1))
                    }
                }
                _ => q,
            };
            Ok(Value::big(q.mul(&p)))
        }
        Some(Num::F(f)) => match nd {
            None => {
                if f.is_nan() {
                    return Err(it.value_error("cannot convert float NaN to integer"));
                }
                if f.is_infinite() {
                    return Err(it.overflow_err("cannot convert float infinity to integer"));
                }
                let r = round_half_even(f);
                Ok(float_to_int(r))
            }
            Some(n) => {
                let n = it.index_of(&n)?;
                if !f.is_finite() {
                    return Ok(Value::Float(f));
                }
                if n > 323 {
                    return Ok(Value::Float(f));
                }
                if n < -308 {
                    return Ok(Value::Float(0.0 * f));
                }
                if n >= 0 {
                    let s = format!("{:.*}", n as usize, f);
                    Ok(Value::Float(s.parse().unwrap_or(f)))
                } else {
                    let p = fmath::powi(10.0, (-n) as i32);
                    Ok(Value::Float(round_half_even(f / p) * p))
                }
            }
        },
        None => {
            let t = it.type_name_of(&x);
            Err(it.type_error(&format!("type {} doesn't define __round__ method", t)))
        }
    }
}

pub fn round_half_even(f: f64) -> f64 {
    let r = fmath::round(f);
    if (f - fmath::trunc(f)).abs() == 0.5 {
        let t = fmath::trunc(f);
        if t % 2.0 == 0.0 {
            t
        } else {
            r
        }
    } else {
        r
    }
}

pub fn float_to_int(f: f64) -> Value {
    if f.abs() < 9.0e18 {
        Value::Int(f as i64)
    } else {
        Value::big(BigInt::from_f64_trunc(f))
    }
}

fn sorted(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    if a.len() != 1 {
        return Err(it.type_error(&format!("sorted expected 1 argument, got {}", a.len())));
    }
    let mut key = Value::None;
    let mut reverse = false;
    for (k, v) in kw {
        match k.as_str_kind().unwrap_or("") {
            "key" => key = v.clone(),
            "reverse" => reverse = it.truthy(v)?,
            other => return Err(it.type_error(&format!("sort() got an unexpected keyword argument '{}'", other))),
        }
    }
    let mut items = it.iterate_to_vec(&a[0])?;
    let key = if key.is_none() { None } else { Some(key) };
    it.sort_values(&mut items, key, reverse)?;
    Ok(Value::list(items))
}

fn sum(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("sum", a, kw, &["iterable", "start"], 1)?;
    let iter_v = b[0].clone().unwrap_or(Value::None);
    let mut acc = b[1].clone().unwrap_or(Value::Int(0));
    if let Some(s) = acc.as_str() {
        let _ = s;
        return Err(it.type_error("sum() can't sum strings [use ''.join(seq) instead]"));
    }
    let iter = it.get_iter(&iter_v)?;
    let mut fsum: Option<(f64, f64)> = None;
    while let Some(x) = it.iter_next(&iter)? {
        if let Some((s, c)) = fsum {
            let xf = match &x {
                Value::Float(f) => Some(*f),
                Value::Int(i) => Some(*i as f64),
                Value::Bool(b) => Some(*b as i64 as f64),
                _ => None,
            };
            if let Some(xf) = xf {
                let t = s + xf;
                let nc = if s.abs() >= xf.abs() { c + ((s - t) + xf) } else { c + ((xf - t) + s) };
                fsum = Some((t, nc));
                continue;
            }
            acc = Value::Float(if (s + c).is_finite() { s + c } else { s });
            fsum = None;
        }
        if let (Value::Int(p), Value::Int(q)) = (&acc, &x) {
            if let Some(r) = p.checked_add(*q) {
                acc = Value::Int(r);
                continue;
            }
        }
        if fsum.is_none() {
            if let (Value::Float(p), Value::Float(_) | Value::Int(_)) = (&acc, &x) {
                let xf = match &x {
                    Value::Float(f) => *f,
                    Value::Int(i) => *i as f64,
                    _ => 0.0,
                };
                let t = *p + xf;
                let c = if p.abs() >= xf.abs() { (*p - t) + xf } else { (xf - t) + *p };
                fsum = Some((t, c));
                continue;
            }
        }
        acc = it.binary_op(BinOp::Add, &acc, &x)?;
    }
    if let Some((s, c)) = fsum {
        return Ok(Value::Float(if (s + c).is_finite() { s + c } else { s }));
    }
    Ok(acc)
}

fn exit(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let cls = it.exc_type("SystemExit");
    Err(it.new_exc(&cls, a.to_vec()))
}

fn import(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("__import__", a, kw, &["name", "globals", "locals", "fromlist", "level"], 1)?;
    let name = it.str_arg(&b[0].clone().unwrap_or(Value::None), "__import__() argument 1")?;
    let from = b[3].clone().unwrap_or(Value::None);
    let level = match &b[4] {
        Some(v) => it.index_of(v)?.max(0) as usize,
        None => 0,
    };
    it.import_name(&name, level, &from)
}

fn build_class(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    it.build_class(a.to_vec(), kw.to_vec())
}

fn aiter(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("aiter", a, 1, 1)?;
    it.call_special(&a[0], "__aiter__", Vec::new())
}

fn anext(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("anext", a, 1, 2)?;
    it.call_special(&a[0], "__anext__", Vec::new())
}

impl Interp {
    pub fn compile_eval_str(&mut self, src: &str, filename: &str) -> R<Rc<crate::bytecode::Code>> {
        let text = src.trim();
        let parsed = crate::limits::with_literal_digit_limit(self.int_max_str_digits, || crate::parser::parse(text, filename));
        let module = match parsed {
            Ok(m) => m,
            Err(e) => return Err(self.syntax_error(&e.msg, filename, e.line, e.col)),
        };
        if module.body.len() == 1 {
            if let StmtKind::Expr(e) = &module.body[0].kind {
                return match crate::compile::compile_eval(e, filename) {
                    Ok(c) => Ok(c),
                    Err(e) => Err(self.syntax_error(&e.msg, filename, e.line, 0)),
                };
            }
        }
        Err(self.syntax_error("invalid syntax", filename, 1, 0))
    }
}

fn eval_exec(it: &mut Interp, a: &[Value], name: &str, is_eval: bool) -> R<Value> {
    it.check_args(name, a, 1, 3)?;
    let globals = match a.get(1) {
        Some(Value::None) | None => frame_globals(it),
        Some(Value::Obj(o)) if matches!(o.kind, Kind::Dict(_)) => o.clone(),
        Some(_) => return Err(it.type_error(&format!("{}() globals must be a dict", name))),
    };
    let locals_d = match a.get(2) {
        Some(Value::None) | None => {
            if a.len() >= 2 && !matches!(a.get(1), Some(Value::None)) {
                globals.clone()
            } else {
                match it.frames.last().and_then(|f| f.names.clone()) {
                    Some(n) => n,
                    None => match locals(it, &[], &[])? {
                        Value::Obj(o) => o,
                        _ => globals.clone(),
                    },
                }
            }
        }
        Some(Value::Obj(o)) => o.clone(),
        Some(_) => return Err(it.type_error(&format!("{}() locals must be a mapping", name))),
    };
    let code = match &a[0] {
        Value::Obj(o) => match &o.kind {
            Kind::Code(c) => c.clone(),
            Kind::Str(s) => {
                if is_eval {
                    it.compile_eval_str(&s.s, "<string>")?
                } else {
                    it.compile_source(&s.s, "<string>")?
                }
            }
            Kind::Bytes(b) => {
                let s = String::from_utf8_lossy(b).into_owned();
                if is_eval {
                    it.compile_eval_str(&s, "<string>")?
                } else {
                    it.compile_source(&s, "<string>")?
                }
            }
            _ => return Err(it.type_error(&format!("{}() arg 1 must be a string, bytes or code object", name))),
        },
        _ => return Err(it.type_error(&format!("{}() arg 1 must be a string, bytes or code object", name))),
    };
    if dict_get_str(&globals, "__builtins__").is_none() {
        dict_set_str(&globals, "__builtins__", Value::Obj(it.builtins.clone()));
    }
    let r = it.run_code(code, globals, locals_d)?;
    Ok(if is_eval { r } else { Value::None })
}

fn eval(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    eval_exec(it, a, "eval", true)
}
fn exec(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    eval_exec(it, a, "exec", false)
}

fn compile(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("compile", a, kw, &["source", "filename", "mode", "flags", "dont_inherit", "optimize"], 3)?;
    let src = it.str_arg(&b[0].clone().unwrap_or(Value::None), "compile() arg 1")?;
    let filename = it.str_arg(&b[1].clone().unwrap_or(Value::None), "compile() arg 2")?;
    let mode = it.str_arg(&b[2].clone().unwrap_or(Value::None), "compile() arg 3")?;
    let code = match mode.as_str() {
        "eval" => it.compile_eval_str(&src, &filename)?,
        "exec" | "single" => it.compile_source(&src, &filename)?,
        _ => return Err(it.value_error("compile() mode must be 'exec', 'eval' or 'single'")),
    };
    Ok(Value::Obj(Object::new(Kind::Code(code))))
}

fn breakpoint(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::None)
}

fn neg_unused(it: &mut Interp, a: &[Value]) -> R<Value> {
    it.unary_op(UnOp::Neg, &a[0])
}

pub fn init(it: &mut Interp) {
    let _ = neg_unused;
    let defs: &[(&'static str, NativeFn)] = &[
        ("print", print),
        ("len", len),
        ("abs", abs),
        ("all", all),
        ("any", any),
        ("ascii", ascii),
        ("bin", bin),
        ("oct", oct),
        ("hex", hex),
        ("callable", callable),
        ("chr", chr),
        ("ord", ord),
        ("getattr", getattr),
        ("hasattr", hasattr),
        ("setattr", setattr),
        ("delattr", delattr),
        ("dir", dir),
        ("divmod", divmod),
        ("format", format),
        ("globals", globals),
        ("locals", locals),
        ("vars", vars),
        ("hash", hash),
        ("id", id),
        ("input", input),
        ("isinstance", isinstance),
        ("issubclass", issubclass),
        ("iter", iter),
        ("next", next),
        ("max", max),
        ("min", min),
        ("pow", pow),
        ("repr", repr),
        ("round", round),
        ("sorted", sorted),
        ("sum", sum),
        ("exit", exit),
        ("quit", exit),
        ("__import__", import),
        ("__build_class__", build_class),
        ("aiter", aiter),
        ("anext", anext),
        ("eval", eval),
        ("exec", exec),
        ("compile", compile),
        ("breakpoint", breakpoint),
    ];
    for (name, f) in defs {
        let v = it.new_native(name, *f, false);
        dict_set_str(&it.builtins.clone(), name, v);
    }
}
