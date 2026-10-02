//! Builtin functions.

use crate::ast::{BinOp, StmtKind};
use crate::bind::KwArgs;
use crate::fmath;
use crate::num::{to_num, Num};
use crate::object::*;
use crate::pyint::{BigInt, PyInt};
use crate::vm::*;
use std::rc::Rc;

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

    pub fn compile_eval_str(&mut self, src: &str, filename: &str) -> R<Rc<crate::bytecode::Code>> {
        let text = src.trim();
        let parsed = crate::limits::with_literal_digit_limit(self.int_max_str_digits, || crate::parser::parse(text, filename));
        let module = match parsed {
            Ok(m) => m,
            Err(e) => return Err(self.syntax_error(&e.msg, filename, e.line, Some(e.col), text)),
        };
        if module.body.len() == 1 {
            if let StmtKind::Expr(e) = &module.body[0].kind {
                return match crate::compile::compile_eval(e, filename) {
                    Ok(c) => Ok(c),
                    Err(e) => Err(self.syntax_error(&e.msg, filename, e.line, None, text)),
                };
            }
        }
        Err(self.syntax_error("invalid syntax", filename, 1, None, text))
    }
}

/// `print`'s `sep`/`end`: `None` or a string.
fn print_text<'a>(it: &mut Interp, v: Option<&'a Value>, what: &str) -> R<Option<&'a str>> {
    match v {
        None | Some(Value::None) => Ok(None),
        Some(v) => match v.as_str() {
            Some(s) => Ok(Some(s)),
            None => {
                let t = it.type_name_of(v);
                Err(it.type_error(&format!("{} must be None or a string, not {}", what, t)))
            }
        },
    }
}

fn radix_str(it: &mut Interp, v: &Value, radix: u32, prefix: &str) -> R<String> {
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
    Ok(format!("{}{}{}", if n.is_negative() { "-" } else { "" }, prefix, s))
}

fn attr_name(it: &mut Interp, v: &Value) -> R<Obj> {
    match v {
        Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => Ok(o.clone()),
        _ => {
            let t = it.type_name_of(v);
            Err(it.type_error(&format!("attribute name must be string, not '{}'", t)))
        }
    }
}

/// `aiter`/`anext`: call the type's `special` method, or fail as CPython does without it.
fn async_call(it: &mut Interp, v: &Value, special: &str, what: &str) -> R<Value> {
    let cls = it.type_of(v);
    if it.lookup_mro(&cls, special).is_none() {
        let t = it.type_name_of(v);
        return Err(it.type_error(&format!("'{}' object is not {}", t, what)));
    }
    it.call_special(v, special, Vec::new())
}

pub(crate) fn frame_globals(it: &Interp) -> Obj {
    it.frames.last().map(|f| f.globals.clone()).unwrap_or_else(|| it.builtins.clone())
}

/// `locals()`: the frame's namespace, or a snapshot of its fast locals and cells.
pub(crate) fn locals_value(it: &mut Interp) -> Value {
    let fr = match it.frames.last() {
        Some(f) => f,
        None => return Value::Obj(it.builtins.clone()),
    };
    if let Some(n) = &fr.names {
        return Value::Obj(n.clone());
    }
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
    let d = it.new_dict();
    for (n, v) in pairs {
        dict_set_str(&d, &n, v);
    }
    Value::Obj(d)
}

fn minmax(it: &mut Interp, a: &[Value], kw: &KwArgs<'_>, name: &str, want_max: bool) -> R<Value> {
    if a.is_empty() {
        return Err(it.type_error(&format!("{} expected at least 1 argument, got 0", name)));
    }
    let mut key = None;
    let mut default: Option<Value> = None;
    for (k, v) in kw.iter() {
        match k {
            "key" if !v.is_none() => key = Some(v.clone()),
            "key" => key = None,
            "default" => default = Some(v.clone()),
            other => return Err(it.type_error(&format!("'{}' is an invalid keyword argument for {}()", other, name))),
        }
    }
    let source = if a.len() == 1 {
        it.get_iter(&a[0])?
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
    let mut best_key = match &key {
        None => best.clone(),
        Some(f) => it.call(f, vec![best.clone()], Vec::new())?,
    };
    while let Some(x) = it.iter_next(&source)? {
        let k = match &key {
            None => x.clone(),
            Some(f) => it.call(f, vec![x.clone()], Vec::new())?,
        };
        let r = it.compare_op(op, &k, &best_key)?;
        if it.truthy(&r)? {
            best = x;
            best_key = k;
        }
    }
    Ok(best)
}

fn round_number(it: &mut Interp, x: &Value, nd: Option<&Value>) -> R<Value> {
    if let Value::Obj(o) = x {
        if o.cls.is_some() {
            if let Some(m) = it.user_special(x, "__round__") {
                let args = nd.map(|n| vec![n.clone()]).unwrap_or_default();
                return it.call_user_special(x, &m, args);
            }
        }
    }
    match to_num(x) {
        Some(Num::I(_)) | Some(Num::B(_)) => {
            let n = match nd {
                None => {
                    return Ok(match x {
                        Value::Bool(b) => Value::Int(*b as i64),
                        _ => x.clone(),
                    })
                }
                Some(n) => it.index_of(n)?,
            };
            if n >= 0 {
                return Ok(x.clone());
            }
            let big = x.as_bigint().unwrap_or_else(BigInt::zero);
            let Ok(p) = BigInt::from_i64(10).pow(&BigInt::from_i64(-n)) else {
                return Err(it.new_exc_str("MemoryError", ""));
            };
            let (q, r) = big.floor_divmod(&p);
            let twice = r.mul(&BigInt::from_i64(2));
            let q = match twice.cmp(&p) {
                std::cmp::Ordering::Greater => q.add(&BigInt::from_i64(1)),
                std::cmp::Ordering::Equal if !q.is_even() => q.add(&BigInt::from_i64(1)),
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
                Ok(float_to_int(round_half_even(f)))
            }
            Some(n) => {
                let n = it.index_of(n)?;
                if !f.is_finite() || n > 323 {
                    return Ok(Value::Float(f));
                }
                if n < -308 {
                    return Ok(Value::Float(0.0 * f));
                }
                if n >= 0 {
                    let s = lumen_common::float::format::fixed(f, n as usize, lumen_common::float::format::Rounding::HalfEven);
                    Ok(Value::Float(s.parse::<f64>().map_or(f, |r| r.copysign(f))))
                } else {
                    let p = fmath::powi(10.0, (-n) as i32);
                    Ok(Value::Float(round_half_even(f / p) * p))
                }
            }
        },
        None => {
            let t = it.type_name_of(x);
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

/// `sum`, adding floats with Neumaier compensation as CPython does.
fn sum_values(it: &mut Interp, iterable: &Value, mut acc: Value) -> R<Value> {
    if acc.as_str().is_some() {
        return Err(it.type_error("sum() can't sum strings [use ''.join(seq) instead]"));
    }
    let iter = it.get_iter(iterable)?;
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

fn eval_exec(it: &mut Interp, source: &Value, globals: Option<&Value>, locals: Option<&Value>, closure: Option<&Value>, is_eval: bool) -> R<Value> {
    let name = if is_eval { "eval" } else { "exec" };
    let globals_d = match globals {
        None => frame_globals(it),
        Some(Value::Obj(o)) if matches!(o.kind, Kind::Dict(_)) => o.clone(),
        Some(g) if is_eval => {
            let _ = g;
            return Err(it.type_error("globals must be a dict"));
        }
        Some(g) => {
            let t = it.type_name_of(g);
            return Err(it.type_error(&format!("exec() globals must be a dict, not {}", t)));
        }
    };
    let locals_d = match locals {
        None if globals.is_some() => globals_d.clone(),
        None => match it.frames.last().and_then(|f| f.names.clone()) {
            Some(n) => n,
            None => match locals_value(it) {
                Value::Obj(o) => o,
                _ => globals_d.clone(),
            },
        },
        Some(Value::Obj(o)) => o.clone(),
        Some(_) if is_eval => return Err(it.type_error("locals must be a mapping")),
        Some(l) => {
            let t = it.type_name_of(l);
            return Err(it.type_error(&format!("locals must be a mapping or None, not {}", t)));
        }
    };
    let bad = |it: &mut Interp| it.type_error(&format!("{}() arg 1 must be a string, bytes or code object", name));
    let code = match source {
        Value::Obj(o) => match &o.kind {
            Kind::Code(c) => c.clone(),
            Kind::Str(s) if is_eval => it.compile_eval_str(&s.s, "<string>")?,
            Kind::Str(s) => it.compile_source(&s.s, "<string>")?,
            Kind::Bytes(b) => {
                let s = it.decode_source(b, "<string>")?;
                if is_eval {
                    it.compile_eval_str(&s, "<string>")?
                } else {
                    it.compile_source(&s, "<string>")?
                }
            }
            _ => return Err(bad(it)),
        },
        _ => return Err(bad(it)),
    };
    let closure = closure.filter(|c| !matches!(c, Value::None));
    let nfree = code.freevars.len();
    let mut cells: Vec<Obj> = Vec::new();
    if !matches!(source, Value::Obj(o) if matches!(o.kind, Kind::Code(_))) {
        if closure.is_some() {
            return Err(it.type_error("closure can only be used when source is a code object"));
        }
    } else if nfree == 0 {
        if closure.is_some() {
            return Err(it.type_error("cannot use a closure with this code object"));
        }
    } else if is_eval {
        return Err(it.type_error("code object passed to eval() may not contain free variables"));
    } else {
        if let Some(Value::Obj(c)) = closure {
            if let (Kind::Tuple(items), None) = (&c.kind, c.cls.as_ref()) {
                if items.len() == nfree {
                    for v in items {
                        match v {
                            Value::Obj(cell) if matches!(cell.kind, Kind::Cell(_)) => cells.push(cell.clone()),
                            _ => break,
                        }
                    }
                }
            }
        }
        if cells.len() != nfree {
            return Err(it.type_error(&format!("code object requires a closure of exactly length {nfree}")));
        }
    }
    if dict_get_str(&globals_d, "__builtins__").is_none() {
        dict_set_str(&globals_d, "__builtins__", Value::Obj(it.builtins.clone()));
    }
    let r = it.run_code_closure(code, globals_d, locals_d, &cells)?;
    Ok(if is_eval { r } else { Value::None })
}

/// `PyUnicode_FSDecoder`: a `str`, `bytes` or path-like filename as text.
fn fs_filename(it: &mut Interp, v: &Value) -> R<String> {
    let p = crate::bind::path::fspath(it, v)?;
    match &p {
        Value::Obj(o) => match &o.kind {
            Kind::Bytes(b) => Ok(crate::bind::path::bytes_path(b)),
            _ => Ok(p.as_str().unwrap_or("").to_string()),
        },
        _ => Ok(String::new()),
    }
}

const PY_CF_ONLY_AST: i64 = 0x400;

fn compile_source(it: &mut Interp, mut source: Value, filename: &Value, mode: &str, flags: i64) -> R<Value> {
    let filename = fs_filename(it, filename)?;
    let ast_mode = match mode {
        "exec" => super::astconv::Mode::Exec,
        "eval" => super::astconv::Mode::Eval,
        "single" => super::astconv::Mode::Single,
        "func_type" if flags & PY_CF_ONLY_AST != 0 => {
            return Err(it.value_error("compile() mode 'func_type' is not supported"));
        }
        "func_type" => return Err(it.value_error("compile() mode 'func_type' requires flag PyCF_ONLY_AST")),
        _ => return Err(it.value_error("compile() mode must be 'exec', 'eval' or 'single'")),
    };
    if crate::bind::is_instance::<super::astm::_ast::AST>(it, &source) {
        if flags & PY_CF_ONLY_AST != 0 {
            return Ok(source);
        }
        // Compiling a tree goes through its source text: positions follow the unparsed text.
        let ast = Value::Obj(it.import_module("ast")?);
        let unparse = it.get_attr_str(&ast, "unparse")?;
        source = it.call(&unparse, vec![source], Vec::new())?;
    }
    let src = match &source {
        Value::Obj(o) if matches!(o.kind, Kind::Str(_)) => o.as_str_kind().unwrap_or("").to_string(),
        _ => match crate::builtins::memview::contiguous_bytes(it, &source)? {
            Some(bytes) => it.decode_source(&bytes, &filename)?,
            None => return Err(it.type_error("compile() arg 1 must be a string, bytes or AST object")),
        },
    };
    if src.contains('\0') {
        let e = it.syntax_error("source code string cannot contain null bytes", &filename, 0, None, "");
        if let Some(d) = e.dict.borrow().as_ref() {
            dict_set_str(d, "lineno", Value::None);
        }
        return Err(e);
    }
    if flags & PY_CF_ONLY_AST != 0 {
        let text = if ast_mode == super::astconv::Mode::Eval { src.trim_start_matches([' ', '\t']) } else { &src };
        let parsed = crate::limits::with_literal_digit_limit(it.int_max_str_digits, || crate::parser::parse(text, &filename));
        let module = match parsed {
            Ok(m) => m,
            Err(e) => return Err(it.syntax_error(&e.msg, &filename, e.line, Some(e.col), text)),
        };
        if ast_mode == super::astconv::Mode::Eval && !matches!(module.body.as_slice(), [s] if matches!(s.kind, StmtKind::Expr(_))) {
            return Err(it.syntax_error("invalid syntax", &filename, 1, None, text));
        }
        return super::astconv::module_to_py(it, &module, ast_mode);
    }
    let code = match ast_mode {
        super::astconv::Mode::Eval => it.compile_eval_str(&src, &filename)?,
        super::astconv::Mode::Exec => it.compile_source(&src, &filename)?,
        super::astconv::Mode::Single => it.compile_source_mode(&src, &filename, true)?,
    };
    let obj = crate::bytecode::Code::object(&code);
    it.code_created(&obj);
    Ok(Value::Obj(obj))
}

pub fn init(it: &mut Interp) {
    crate::bind::install_functions::<builtin_fns::Module>(&it.builtins.clone());
}

// The functions of the `builtins` module, installed into the interpreter's builtins dict.
#[lumen_bind::module(name = "builtins")]
pub mod builtin_fns {
    use super::*;
    use crate::bind::KwArgs;
    use lumen_bind::Passed;

    /// Prints the values to a stream, or to sys.stdout by default.
    ///
    ///   sep
    ///     string inserted between values, default a space.
    ///   end
    ///     string appended after the last value, default a newline.
    ///   file
    ///     a file-like object (stream); defaults to the current sys.stdout.
    ///   flush
    ///     whether to forcibly flush the stream.
    #[op(hint(py(text_signature = "($module, /, *args, sep=' ', end='\\n', file=None, flush=False)")))]
    fn print(
        it: &mut Interp,
        #[varargs] args: &[Value],
        #[kwonly] sep: Option<&Value>,
        #[kwonly] end: Option<&Value>,
        #[kwonly] file: Option<&Value>,
        #[kwonly] flush: Option<&Value>,
    ) -> R<()> {
        let sep = print_text(it, sep, "sep")?.unwrap_or(" ");
        let end = print_text(it, end, "end")?.unwrap_or("\n");
        let file = match file {
            Some(f) => f.clone(),
            None => match it.sys_attr("stdout") {
                Some(f) if !f.is_none() => f,
                _ => return Ok(()),
            },
        };
        if crate::builtins::iom::textio::is_native_textio(it, &file) {
            let mut out = String::new();
            for (i, v) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(sep);
                }
                match v.as_exact_str() {
                    Some(s) => out.push_str(s),
                    None => out.push_str(&it.str_of(v)?),
                }
            }
            out.push_str(end);
            it.write_to(&file, &out)?;
        } else {
            for (i, v) in args.iter().enumerate() {
                if i > 0 {
                    it.write_to(&file, sep)?;
                }
                match v.as_exact_str() {
                    Some(s) => it.write_to(&file, s)?,
                    None => {
                        let s = it.str_of(v)?;
                        it.write_to(&file, &s)?;
                    }
                }
            }
            it.write_to(&file, end)?;
        }
        if let Some(f) = flush {
            if it.truthy(f)? {
                it.call_method(&file, "flush", Vec::new())?;
            }
        }
        Ok(())
    }

    /// Return the number of items in a container.
    #[op]
    fn len(it: &mut Interp, obj: &Value) -> R<Value> {
        Ok(Value::Int(it.len_of(obj)? as i64))
    }

    /// Return the absolute value of the argument.
    #[op]
    fn abs(it: &mut Interp, x: &Value) -> R<Value> {
        match x {
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
                    if let Some(m) = it.user_special(x, "__abs__") {
                        return it.call_user_special(x, &m, Vec::new());
                    }
                }
                match &o.kind {
                    Kind::Int(b) => return Ok(Value::big(b.abs())),
                    Kind::Float(f) => return Ok(Value::Float(f.abs())),
                    Kind::Complex(r, i) => {
                        return match lumen_common::float::complex::abs(lumen_common::float::complex::Complex::new(*r, *i)) {
                            Ok(f) => Ok(Value::Float(f)),
                            Err(_) => Err(it.overflow_err("absolute value too large")),
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        let cls = it.type_of(x);
        if let Some(m) = it.lookup_mro(&cls, "__abs__") {
            let b = it.bind_descr(&m, x, &cls)?;
            return it.call(&b, Vec::new(), Vec::new());
        }
        let t = it.type_name_of(x);
        Err(it.type_error(&format!("bad operand type for abs(): '{}'", t)))
    }

    /// Return True if bool(x) is True for all values x in the iterable.
    ///
    /// If the iterable is empty, return True.
    #[op]
    fn all(it: &mut Interp, iterable: &Value) -> R<bool> {
        let iter = it.get_iter(iterable)?;
        while let Some(v) = it.iter_next(&iter)? {
            if !it.truthy(&v)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Return True if bool(x) is True for any x in the iterable.
    ///
    /// If the iterable is empty, return False.
    #[op]
    fn any(it: &mut Interp, iterable: &Value) -> R<bool> {
        let iter = it.get_iter(iterable)?;
        while let Some(v) = it.iter_next(&iter)? {
            if it.truthy(&v)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Return an ASCII-only representation of an object.
    ///
    /// As repr(), return a string containing a printable representation of an
    /// object, but escape the non-ASCII characters in the string returned by
    /// repr() using \\x, \\u or \\U escapes. This generates a string similar
    /// to that returned by repr() in Python 2.
    #[op]
    fn ascii(it: &mut Interp, obj: &Value) -> R<String> {
        let r = it.repr_of(obj)?;
        Ok(crate::repr::ascii_escape(&r))
    }

    /// Return the binary representation of an integer.
    ///
    ///    >>> bin(2796202)
    ///    '0b1010101010101010101010'
    #[op]
    fn bin(it: &mut Interp, number: &Value) -> R<String> {
        radix_str(it, number, 2, "0b")
    }

    /// Return the octal representation of an integer.
    ///
    ///    >>> oct(342391)
    ///    '0o1234567'
    #[op]
    fn oct(it: &mut Interp, number: &Value) -> R<String> {
        radix_str(it, number, 8, "0o")
    }

    /// Return the hexadecimal representation of an integer.
    ///
    ///    >>> hex(12648430)
    ///    '0xc0ffee'
    #[op]
    fn hex(it: &mut Interp, number: &Value) -> R<String> {
        radix_str(it, number, 16, "0x")
    }

    /// Return whether the object is callable (i.e., some kind of function).
    ///
    /// Note that classes are callable, as are instances of classes with a
    /// __call__() method.
    #[op]
    fn callable(it: &mut Interp, obj: &Value) -> bool {
        match obj {
            Value::Obj(o) => match &o.kind {
                Kind::Type(_) | Kind::Function(_) | Kind::Method(..) | Kind::Native(_) => true,
                _ => {
                    let cls = it.type_of_obj(o);
                    it.lookup_mro(&cls, "__call__").is_some()
                }
            },
            _ => false,
        }
    }

    /// Return a Unicode string of one character with ordinal i; 0 <= i <= 0x10ffff.
    #[op]
    fn chr(it: &mut Interp, i: &Value) -> R<Value> {
        let n = it.index_of(i)?;
        match u32::try_from(n).ok().and_then(lumen_common::smuggle::code_point_str) {
            Some(c) => Ok(Value::str(&c)),
            None => Err(it.value_error("chr() arg not in range(0x110000)")),
        }
    }

    /// Return the Unicode code point for a one-character string.
    #[op]
    fn ord(it: &mut Interp, c: &Value) -> R<i64> {
        if let Value::Obj(o) = c {
            match &o.kind {
                Kind::Str(s) if s.nchars == 1 => return Ok(lumen_common::smuggle::code_points(&s.s).next().unwrap_or(0) as i64),
                Kind::Str(s) => {
                    return Err(it.type_error(&format!("ord() expected a character, but string of length {} found", s.nchars)))
                }
                Kind::Bytes(b) if b.len() == 1 => return Ok(b[0] as i64),
                Kind::ByteArray(b) if b.len() == 1 => return Ok(b.bytes()[0] as i64),
                _ => {}
            }
        }
        let t = it.type_name_of(c);
        Err(it.type_error(&format!("ord() expected string of length 1, but {} found", t)))
    }

    /// getattr(object, name[, default]) -> value
    ///
    /// Get a named attribute from an object; getattr(x, 'y') is equivalent to x.y.
    /// When a default argument is given, it is returned when the attribute doesn't
    /// exist; without it, an exception is raised in that case.
    #[op(hint(py(text_signature = "", arg_style = "unpack")))]
    fn getattr(it: &mut Interp, object: &Value, name: &Value, default: Passed<&Value>) -> R<Value> {
        let n = attr_name(it, name)?;
        match it.get_attr(object, &n) {
            Err(e) if default.0.is_some() && it.exc_is(&e, "AttributeError") => Ok(default.0.cloned().unwrap_or(Value::None)),
            r => r,
        }
    }

    /// Return whether the object has an attribute with the given name.
    ///
    /// This is done by calling getattr(obj, name) and catching AttributeError.
    #[op]
    fn hasattr(it: &mut Interp, obj: &Value, name: &Value) -> R<bool> {
        let n = attr_name(it, name)?;
        match it.get_attr(obj, &n) {
            Ok(_) => Ok(true),
            Err(e) if it.exc_is(&e, "AttributeError") => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Sets the named attribute on the given object to the specified value.
    ///
    /// setattr(x, 'y', v) is equivalent to ``x.y = v``
    #[op]
    fn setattr(it: &mut Interp, obj: &Value, name: &Value, value: &Value) -> R<()> {
        let n = attr_name(it, name)?;
        it.set_attr(obj, &n, value.clone())
    }

    /// Deletes the named attribute from the given object.
    ///
    /// delattr(x, 'y') is equivalent to ``del x.y``
    #[op]
    fn delattr(it: &mut Interp, obj: &Value, name: &Value) -> R<()> {
        let n = attr_name(it, name)?;
        it.del_attr(obj, &n)
    }

    /// dir([object]) -> list of strings
    ///
    /// If called without an argument, return the names in the current scope.
    /// Else, return an alphabetized list of names comprising (some of) the attributes
    /// of the given object, and of attributes reachable from it.
    /// If the object supplies a method named __dir__, it will be used; otherwise
    /// the default dir() logic is used and returns:
    ///   for a module object: the module's attributes.
    ///   for a class object:  its attributes, and recursively the attributes
    ///     of its bases.
    ///   for any other object: its attributes, its class's attributes, and
    ///     recursively the attributes of its class's base classes.
    #[op(hint(py(text_signature = "")))]
    fn dir(it: &mut Interp, object: Passed<&Value>) -> R<Value> {
        let Some(obj) = object.0 else {
            let keys = match locals_value(it) {
                Value::Obj(o) => match &o.kind {
                    Kind::Dict(d) => d.borrow().keys(),
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            let mut names: Vec<String> = keys.iter().filter_map(|k| k.as_str().map(|s| s.to_string())).collect();
            names.sort();
            return Ok(Value::list(names.into_iter().map(Value::string).collect()));
        };
        let cls = it.type_of(obj);
        if let Some(m) = it.lookup_mro(&cls, "__dir__") {
            let b = it.bind_descr(&m, obj, &cls)?;
            let r = it.call(&b, Vec::new(), Vec::new())?;
            let mut items = it.iterate_to_vec(&r)?;
            it.sort_values(&mut items, None, false)?;
            return Ok(Value::list(items));
        }
        Ok(Value::None)
    }

    /// Return the tuple (x//y, x%y).  Invariant: div*y + mod == x.
    #[op]
    fn divmod(it: &mut Interp, x: &Value, y: &Value) -> R<Value> {
        if it.user_special(x, "__divmod__").is_some() {
            return it.call_method(x, "__divmod__", vec![y.clone()]);
        }
        if it.user_special(y, "__rdivmod__").is_some() && it.user_special(x, "__floordiv__").is_none() {
            return it.call_method(y, "__rdivmod__", vec![x.clone()]);
        }
        if to_num(x).is_none() || to_num(y).is_none() {
            let (ta, tb) = (it.type_name_of(x), it.type_name_of(y));
            return Err(it.type_error(&format!("unsupported operand type(s) for divmod(): '{}' and '{}'", ta, tb)));
        }
        let q = it.binary_op(BinOp::FloorDiv, x, y)?;
        let r = it.binary_op(BinOp::Mod, x, y)?;
        Ok(Value::tuple(vec![q, r]))
    }

    /// Return type(value).__format__(value, format_spec)
    ///
    /// Many built-in types implement format_spec according to the
    /// Format Specification Mini-language. See help('FORMATTING').
    ///
    /// If type(value) does not supply a method named __format__
    /// and format_spec is empty, then str(value) is returned.
    /// See also help('SPECIALMETHODS').
    #[op(hint(py(arg_style = "unpack")))]
    fn format(it: &mut Interp, value: &Value, #[default("")] format_spec: &str) -> R<String> {
        it.format_value(value, format_spec)
    }

    /// Return the dictionary containing the current scope's global variables.
    ///
    /// NOTE: Updates to this dictionary *will* affect name lookups in the current
    /// global scope and vice-versa.
    #[op]
    fn globals(it: &mut Interp) -> Obj {
        frame_globals(it)
    }

    /// Return a dictionary containing the current scope's local variables.
    ///
    /// NOTE: Whether or not updates to this dictionary will affect name lookups in
    /// the local scope and vice-versa is *implementation dependent* and not
    /// covered by any backwards compatibility guarantees.
    #[op]
    fn locals(it: &mut Interp) -> Value {
        locals_value(it)
    }

    /// vars([object]) -> dictionary
    ///
    /// Without arguments, equivalent to locals().
    /// With an argument, equivalent to object.__dict__.
    #[op(hint(py(text_signature = "")))]
    fn vars(it: &mut Interp, object: Passed<&Value>) -> R<Value> {
        let Some(obj) = object.0 else {
            return Ok(locals_value(it));
        };
        match it.get_attr_str(obj, "__dict__") {
            Err(e) if it.exc_is(&e, "AttributeError") => Err(it.type_error("vars() argument must have __dict__ attribute")),
            r => r,
        }
    }

    /// Return the hash value for the given object.
    ///
    /// Two objects that compare equal must also have the same hash value, but the
    /// reverse is not necessarily true.
    #[op]
    fn hash(it: &mut Interp, obj: &Value) -> R<i64> {
        it.hash_value(obj)
    }

    /// Return the identity of an object.
    ///
    /// This is guaranteed to be unique among simultaneously existing objects.
    /// (CPython uses the object's memory address.)
    #[op]
    fn id(it: &mut Interp, obj: &Value) -> i64 {
        it.id_of(obj) as i64
    }

    /// Read a string from standard input.  The trailing newline is stripped.
    ///
    /// The prompt string, if given, is printed to standard output without a
    /// trailing newline before reading input.
    ///
    /// If the user hits EOF (*nix: Ctrl-D, Windows: Ctrl-Z+Return), raise EOFError.
    /// On *nix systems, readline is used if available.
    #[op(hint(py(text_signature = "($module, prompt='', /)")))]
    fn input(it: &mut Interp, prompt: Passed<&Value>) -> R<Value> {
        let mut streams = Vec::new();
        for name in ["stdin", "stdout", "stderr"] {
            match it.sys_attr(name) {
                Some(f) if !f.is_none() => streams.push(f),
                _ => return Err(it.new_exc_str("RuntimeError", &format!("input(): lost sys.{}", name))),
            }
        }
        let (fin, fout, ferr) = (&streams[0], &streams[1], &streams[2]);
        let _ = it.call_method(ferr, "flush", Vec::new());
        if let Some(p) = prompt.0 {
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

    /// Return whether an object is an instance of a class or of a subclass thereof.
    ///
    /// A tuple, as in ``isinstance(x, (A, B, ...))``, may be given as the target to
    /// check against. This is equivalent to ``isinstance(x, A) or isinstance(x, B)
    /// or ...`` etc.
    #[op]
    fn isinstance(it: &mut Interp, obj: &Value, class_or_tuple: &Value) -> R<bool> {
        it.isinstance_value(obj, class_or_tuple)
    }

    /// Return whether 'cls' is derived from another class or is the same class.
    ///
    /// A tuple, as in ``issubclass(x, (A, B, ...))``, may be given as the target to
    /// check against. This is equivalent to ``issubclass(x, A) or issubclass(x, B)
    /// or ...``.
    #[op]
    fn issubclass(it: &mut Interp, cls: &Value, class_or_tuple: &Value) -> R<bool> {
        it.issubclass_value(cls, class_or_tuple)
    }

    /// iter(iterable) -> iterator
    /// iter(callable, sentinel) -> iterator
    ///
    /// Get an iterator from an object.  In the first form, the argument must
    /// supply its own iterator, or be a sequence.
    /// In the second form, the callable is called until it returns the sentinel.
    #[op(hint(py(text_signature = "")))]
    fn iter(it: &mut Interp, object: &Value, sentinel: Passed<&Value>) -> R<Value> {
        let Some(sentinel) = sentinel.0 else {
            return it.get_iter(object);
        };
        let callable = match object {
            Value::Obj(o) => {
                matches!(o.kind, Kind::Function(_) | Kind::Method(..) | Kind::Native(_) | Kind::Type(_))
                    || it.lookup_mro(&it.type_of_obj(o), "__call__").is_some()
            }
            _ => false,
        };
        if !callable {
            return Err(it.type_error("iter(v, w): v must be callable"));
        }
        Ok(it.mk_iter(IterState::CallIter { f: object.clone(), sentinel: sentinel.clone(), done: false }))
    }

    /// next(iterator[, default])
    ///
    /// Return the next item from the iterator. If default is given and the iterator
    /// is exhausted, it is returned instead of raising StopIteration.
    #[op(hint(py(text_signature = "")))]
    fn next(it: &mut Interp, iterator: &Value, default: Passed<&Value>) -> R<Value> {
        it.ret_val = Value::None;
        match it.iter_next(iterator)? {
            Some(v) => Ok(v),
            None => match default.0 {
                Some(d) => Ok(d.clone()),
                None => {
                    let v = std::mem::replace(&mut it.ret_val, Value::None);
                    Err(it.stop_iteration(v))
                }
            },
        }
    }

    /// max(iterable, *[, default=obj, key=func]) -> value
    /// max(arg1, arg2, *args, *[, key=func]) -> value
    ///
    /// With a single iterable argument, return its biggest item. The
    /// default keyword-only argument specifies an object to return if
    /// the provided iterable is empty.
    /// With two or more arguments, return the largest argument.
    #[op(hint(py(text_signature = "")))]
    fn max(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs<'_>) -> R<Value> {
        minmax(it, args, &kw, "max", true)
    }

    /// min(iterable, *[, default=obj, key=func]) -> value
    /// min(arg1, arg2, *args, *[, key=func]) -> value
    ///
    /// With a single iterable argument, return its smallest item. The
    /// default keyword-only argument specifies an object to return if
    /// the provided iterable is empty.
    /// With two or more arguments, return the smallest argument.
    #[op(hint(py(text_signature = "")))]
    fn min(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs<'_>) -> R<Value> {
        minmax(it, args, &kw, "min", false)
    }

    /// Equivalent to base**exp with 2 arguments or base**exp % mod with 3 arguments
    ///
    /// Some types, such as ints, are able to use a more efficient algorithm when
    /// invoked using the three argument form.
    #[op(hint(py(text_signature = "($module, /, base, exp, mod=None)")))]
    fn pow(it: &mut Interp, #[kw] base: &Value, #[kw] exp: &Value, #[kw] r#mod: Option<&Value>) -> R<Value> {
        let Some(m) = r#mod else {
            return it.binary_op(BinOp::Pow, base, exp);
        };
        if let (Some(x), Some(e), Some(md)) = (base.as_bigint(), exp.as_bigint(), m.as_bigint()) {
            return it.int_pow_mod(&x, &e, &md);
        }
        if it.user_special(base, "__pow__").is_some() {
            return it.call_method(base, "__pow__", vec![exp.clone(), m.clone()]);
        }
        let is_complex = |v: &Value| matches!(v, Value::Obj(o) if matches!(o.kind, Kind::Complex(..)));
        if is_complex(base) || is_complex(exp) {
            return Err(it.value_error("complex modulo"));
        }
        Err(it.type_error("pow() 3rd argument not allowed unless all arguments are integers"))
    }

    /// Return the canonical string representation of the object.
    ///
    /// For many object types, including most builtins, eval(repr(obj)) == obj.
    #[op]
    fn repr(it: &mut Interp, obj: &Value) -> R<String> {
        it.repr_of(obj)
    }

    /// Round a number to a given precision in decimal digits.
    ///
    /// The return value is an integer if ndigits is omitted or None.  Otherwise
    /// the return value has the same type as the number.  ndigits may be negative.
    #[op(hint(py(text_signature = "($module, /, number, ndigits=None)")))]
    fn round(it: &mut Interp, #[kw] number: &Value, #[kw] ndigits: Option<&Value>) -> R<Value> {
        round_number(it, number, ndigits)
    }

    /// Return a new list containing all items from the iterable in ascending order.
    ///
    /// A custom key function can be supplied to customize the sort order, and the
    /// reverse flag can be set to request the result in descending order.
    #[op(hint(py(text_signature = "($module, iterable, /, *, key=None, reverse=False)")))]
    fn sorted(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs<'_>) -> R<Value> {
        let [iterable] = args else {
            return Err(it.type_error(&format!("sorted expected 1 argument, got {}", args.len())));
        };
        let mut items = it.iterate_to_vec(iterable)?;
        let (mut key, mut reverse) = (None, false);
        for (k, v) in kw.iter() {
            match k {
                "key" => key = Some(v.clone()).filter(|v| !v.is_none()),
                "reverse" => reverse = it.truthy(v)?,
                other => return Err(it.type_error(&format!("'{}' is an invalid keyword argument for sort()", other))),
            }
        }
        it.sort_values(&mut items, key, reverse)?;
        Ok(Value::list(items))
    }

    /// Return the sum of a 'start' value (default: 0) plus an iterable of numbers
    ///
    /// When the iterable is empty, return the start value.
    /// This function is intended specifically for use with numeric values and may
    /// reject non-numeric types.
    #[op(hint(py(text_signature = "($module, iterable, /, start=0)")))]
    fn sum(it: &mut Interp, iterable: &Value, #[kw] start: Passed<&Value>) -> R<Value> {
        sum_values(it, iterable, start.0.cloned().unwrap_or(Value::Int(0)))
    }

    #[op(hint(py(text_signature = "", aliases = "quit")))]
    fn exit(it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let cls = it.exc_type("SystemExit");
        Err(it.new_exc(&cls, args.to_vec()))
    }

    /// Import a module.
    ///
    /// Because this function is meant for use by the Python
    /// interpreter and not for general use, it is better to use
    /// importlib.import_module() to programmatically import a module.
    ///
    /// The globals argument is only used to determine the context;
    /// they are not modified.  The locals argument is unused.  The fromlist
    /// should be a list of names to emulate ``from name import ...``, or an
    /// empty list to emulate ``import name``.
    /// When importing a module from a package, note that __import__('A.B', ...)
    /// returns package A when fromlist is empty, but its submodule B when
    /// fromlist is not empty.  The level argument is used to determine whether to
    /// perform absolute or relative imports: 0 is absolute, while a positive number
    /// is the number of parent directories to search relative to the current module.
    #[op(name = "__import__", hint(py(text_signature = "($module, /, name, globals=None, locals=None, fromlist=(),\n           level=0)")))]
    fn import(
        it: &mut Interp,
        #[kw] name: &Value,
        #[kw] _globals: Option<&Value>,
        #[kw] _locals: Option<&Value>,
        #[kw] fromlist: Option<&Value>,
        #[kw] level: Option<&Value>,
    ) -> R<Value> {
        let Some(name) = name.as_str() else {
            return Err(it.type_error("module name must be a string"));
        };
        let level = match level {
            Some(v) => it.index_of(v)?,
            None => 0,
        };
        if level < 0 {
            return Err(it.value_error("level must be >= 0"));
        }
        if level == 0 && name.is_empty() {
            return Err(it.value_error("Empty module name"));
        }
        let name = name.to_string();
        it.import_name(&name, level as usize, fromlist.unwrap_or(&Value::None))
    }

    /// __build_class__(func, name, /, *bases, [metaclass], **kwds) -> class
    ///
    /// Internal helper function used by the class statement.
    #[op(name = "__build_class__", hint(py(text_signature = "")))]
    fn build_class(it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs<'_>) -> R<Value> {
        it.build_class(args.to_vec(), kw.to_vec())
    }

    /// Return an AsyncIterator for an AsyncIterable object.
    #[op]
    fn aiter(it: &mut Interp, async_iterable: &Value) -> R<Value> {
        async_call(it, async_iterable, "__aiter__", "an async iterable")
    }

    /// async anext(aiterator[, default])
    ///
    /// Return the next item from the async iterator.  If default is given and the async
    /// iterator is exhausted, it is returned instead of raising StopAsyncIteration.
    #[op(hint(py(text_signature = "($module, aiterator, default=<unrepresentable>, /)")))]
    fn anext(it: &mut Interp, aiterator: &Value, _default: Passed<&Value>) -> R<Value> {
        async_call(it, aiterator, "__anext__", "an async iterator")
    }

    /// Evaluate the given source in the context of globals and locals.
    ///
    /// The source may be a string representing a Python expression
    /// or a code object as returned by compile().
    /// The globals must be a dictionary and locals can be any mapping,
    /// defaulting to the current globals and locals.
    /// If only globals is given, locals defaults to it.
    #[op(hint(py(text_signature = "($module, source, globals=None, locals=None, /)")))]
    fn eval(it: &mut Interp, source: &Value, globals: Option<&Value>, locals: Option<&Value>) -> R<Value> {
        eval_exec(it, source, globals, locals, None, true)
    }

    /// Execute the given source in the context of globals and locals.
    ///
    /// The source may be a string representing one or more Python statements
    /// or a code object as returned by compile().
    /// The globals must be a dictionary and locals can be any mapping,
    /// defaulting to the current globals and locals.
    /// If only globals is given, locals defaults to it.
    /// The closure must be a tuple of cellvars, and can only be used
    /// when source is a code object requiring exactly that many cellvars.
    #[op(hint(py(text_signature = "($module, source, globals=None, locals=None, /, *, closure=None)")))]
    fn exec(it: &mut Interp, source: &Value, globals: Option<&Value>, locals: Option<&Value>, #[kwonly] closure: Option<&Value>) -> R<Value> {
        eval_exec(it, source, globals, locals, closure, false)
    }

    /// Compile source into a code object that can be executed by exec() or eval().
    ///
    /// The source code may represent a Python module, statement or expression.
    /// The filename will be used for run-time error messages.
    /// The mode must be 'exec' to compile a module, 'single' to compile a
    /// single (interactive) statement, or 'eval' to compile an expression.
    /// The flags argument, if present, controls which future statements influence
    /// the compilation of the code.
    /// The dont_inherit argument, if true, stops the compilation inheriting
    /// the effects of any future statements in effect in the code calling
    /// compile; if absent or false these statements do influence the compilation,
    /// in addition to any features explicitly specified.
    #[op(hint(py(
        text_signature = "($module, /, source, filename, mode, flags=0,\n        dont_inherit=False, optimize=-1, *, _feature_version=-1)"
    )))]

    fn compile(
        it: &mut Interp,
        #[kw] source: &Value,
        #[kw] filename: &Value,
        #[kw] mode: &str,
        #[kw] flags: Option<&Value>,
        #[kw] _dont_inherit: Option<&Value>,
        #[kw] _optimize: Option<&Value>,
        #[kwonly] #[name("_feature_version")] _feature_version: Option<&Value>,
    ) -> R<Value> {
        let flags = match flags {
            Some(v) => it.index_of(v)?,
            None => 0,
        };
        compile_source(it, source.clone(), filename, mode, flags)
    }

    /// breakpoint(*args, **kws)
    ///
    /// Call sys.breakpointhook(*args, **kws).  sys.breakpointhook() must accept
    /// whatever arguments are passed.
    ///
    /// By default, this drops you into the pdb debugger.
    #[op(hint(py(text_signature = "")))]
    fn breakpoint(#[varargs] _args: &[Value], #[varkw] _kws: KwArgs<'_>) {}
}
