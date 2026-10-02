//! The `Decimal` class: construction from Python values, the numeric protocol, and its methods.

use super::context::*;
use super::support::*;
use crate::bind::{opaque_instance, type_object, KwArgs, Py, This};
use crate::object::*;
use crate::vm::Interp;
use lumen_common::decimal as dec;
use lumen_common::decimal::{flag, Rounding};
use std::cmp::Ordering;
use std::rc::Rc;

type Un = fn(&dec::Decimal, &dec::Context, &mut dec::Status) -> dec::Decimal;
type Bin = fn(&dec::Decimal, &dec::Decimal, &dec::Context, &mut dec::Status) -> dec::Decimal;

/// An immutable decimal floating-point number.
#[lumen_bind::class(name = "Decimal", module = "decimal")]
pub struct PyDecimal {
    pub v: dec::Decimal,
}

pub fn wrap(it: &mut Interp, v: dec::Decimal) -> Value {
    Py::new(it, PyDecimal { v }).into_value()
}

/// `v` as a decimal if it is a `Decimal` or an `int`.
pub fn convert_op(it: &mut Interp, v: &Value) -> R<Option<dec::Decimal>> {
    if let Some(p) = Py::<PyDecimal>::from_value(it, v) {
        let d = p.borrow(it)?.v.clone();
        return Ok(Some(d));
    }
    if let Some(b) = v.as_bigint() {
        return Ok(Some(dec::Decimal::from_bigint(&b)));
    }
    Ok(None)
}

pub fn convert_op_raise(it: &mut Interp, v: &Value) -> R<dec::Decimal> {
    match convert_op(it, v)? {
        Some(d) => Ok(d),
        None => {
            let t = it.tp_name_of(v);
            Err(it.type_error(&format!("conversion from {} to Decimal is not supported", t)))
        }
    }
}

fn exact_float(it: &mut Interp, f: f64) -> R<dec::Decimal> {
    match dec::Decimal::from_f64(f) {
        Ok(d) => Ok(d),
        Err(_) => Err(memory_error(it)),
    }
}

/// The exact value of an `int` or a `float`.
pub fn exact_from_number(it: &mut Interp, v: &Value) -> R<dec::Decimal> {
    if let Some(b) = v.as_bigint() {
        return Ok(dec::Decimal::from_bigint(&b));
    }
    if let Some(f) = float_of(v) {
        return exact_float(it, f);
    }
    Err(it.type_error("argument must be int or float"))
}

/// The ASCII text `PyDecType_FromUnicode` parses: Unicode decimal digits as ASCII digits, Unicode
/// white space as blanks, anything else non-ASCII as `?`; with `strip`, underscores are dropped and
/// the blanks around the number are stripped.
fn ascii_text(s: &str, strip: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if strip && ch == '_' {
            continue;
        }
        let cp = ch as u32;
        if crate::unicode::is_space(cp) {
            out.push(' ');
        } else if cp < 128 {
            out.push(ch);
        } else if let Some(d) = lumen_common::ucd::props(cp, lumen_common::ucd::Version::Current).decimal {
            out.push((b'0' + d) as char);
        } else {
            out.push('?');
        }
    }
    if strip {
        out.trim_matches(' ').to_string()
    } else {
        out
    }
}

/// An exact conversion of numeric-string `text`; a malformed or inexact value signals.
fn from_text(it: &mut Interp, cx: &Py<PyContext>, text: &str) -> R<dec::Decimal> {
    let (d, st) = match dec::Decimal::parse(text) {
        Ok(d) => {
            let mut probe: dec::Status = 0;
            d.fit(&dec::Context::max(), &mut probe);
            let inexact = flag::INEXACT | flag::ROUNDED | flag::CLAMPED | flag::OVERFLOW | flag::UNDERFLOW;
            if probe & inexact != 0 {
                (dec::Decimal::quiet_nan(), flag::INVALID_OPERATION)
            } else {
                (d, 0)
            }
        }
        Err(_) => (dec::Decimal::quiet_nan(), flag::CONVERSION_SYNTAX),
    };
    finish(it, cx, st)?;
    Ok(d)
}

fn from_sequence(it: &mut Interp, cx: &Py<PyContext>, items: &[Value]) -> R<dec::Decimal> {
    if items.len() != 3 {
        return Err(it.value_error("argument must be a sequence of length 3"));
    }
    let sign = match items[0].as_bigint().and_then(|b| b.to_i64()) {
        Some(0) => "",
        Some(1) => "-",
        _ => return Err(it.value_error("sign must be an integer with the value 0 or 1")),
    };
    let digits: Vec<Value> = if let Some(t) = items[1].tuple_items() {
        t.to_vec()
    } else if let Some(l) = list_of(&items[1]) {
        l.borrow().clone()
    } else {
        return Err(it.value_error("coefficient must be a tuple of digits"));
    };
    let mut coef = String::with_capacity(digits.len());
    for d in &digits {
        match d.as_bigint().and_then(|b| b.to_i64()) {
            Some(n @ 0..=9) => coef.push((b'0' + n as u8) as char),
            _ => return Err(it.value_error("coefficient must be a tuple of digits")),
        }
    }
    let tag = &items[2];
    let text = if let Some(t) = tag.as_str() {
        match t {
            "F" => format!("{}Infinity", sign),
            "n" => format!("{}NaN{}", sign, coef),
            "N" => format!("{}sNaN{}", sign, coef),
            _ => return Err(it.value_error("string argument in the third position must be 'F', 'n' or 'N'")),
        }
    } else if tag.is_int_like() {
        let e = ssize(it, tag)?;
        format!("{}{}E{}", sign, if coef.is_empty() { "0" } else { &coef }, e)
    } else {
        return Err(it.value_error("exponent must be an integer"));
    };
    from_text(it, cx, &text)
}

/// The exact value of a Python object, as the `Decimal` constructor reads it. `strip`: strings
/// may carry blanks and underscores.
pub fn from_object(it: &mut Interp, v: &Value, cx: &Py<PyContext>, strip: bool) -> R<dec::Decimal> {
    if let Some(p) = Py::<PyDecimal>::from_value(it, v) {
        let d = p.borrow(it)?.v.clone();
        return Ok(d);
    }
    if let Some(b) = v.as_bigint() {
        return Ok(dec::Decimal::from_bigint(&b));
    }
    if let Some(s) = v.as_str() {
        let text = ascii_text(s, strip);
        return from_text(it, cx, &text);
    }
    if let Some(f) = float_of(v) {
        finish(it, cx, flag::FLOAT_OPERATION)?;
        return exact_float(it, f);
    }
    let items = match v.tuple_items() {
        Some(t) => Some(t.to_vec()),
        None => list_of(v).map(|l| l.borrow().clone()),
    };
    if let Some(items) = items {
        return from_sequence(it, cx, &items);
    }
    let t = it.tp_name_of(v);
    Err(it.type_error(&format!("conversion from {} to Decimal is not supported", t)))
}

// ---- operator helpers --------------------------------------------------------------------------

fn arith(it: &mut Interp, a: &Value, b: &Value, f: Bin) -> R<Value> {
    let cx = current(it)?;
    let Some(x) = convert_op(it, a)? else { return Ok(Value::NotImplemented) };
    let Some(y) = convert_op(it, b)? else { return Ok(Value::NotImplemented) };
    let r = run(it, &cx, |c, st| f(&x, &y, c, st))?;
    Ok(wrap(it, r))
}

fn divmod_op(it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
    let cx = current(it)?;
    let Some(x) = convert_op(it, a)? else { return Ok(Value::NotImplemented) };
    let Some(y) = convert_op(it, b)? else { return Ok(Value::NotImplemented) };
    let (q, r) = run(it, &cx, |c, st| x.divmod(&y, c, st))?;
    let (q, r) = (wrap(it, q), wrap(it, r));
    Ok(Value::tuple(vec![q, r]))
}

fn power_op(it: &mut Interp, base: &Value, exp: &Value, modulo: Option<&Value>) -> R<Value> {
    let cx = current(it)?;
    let Some(x) = convert_op(it, base)? else { return Ok(Value::NotImplemented) };
    let Some(y) = convert_op(it, exp)? else { return Ok(Value::NotImplemented) };
    let r = match modulo {
        None | Some(Value::None) => run(it, &cx, |c, st| x.pow(&y, c, st))?,
        Some(m) => {
            let Some(m) = convert_op(it, m)? else { return Ok(Value::NotImplemented) };
            run(it, &cx, |c, st| x.pow_mod(&y, &m, c, st))?
        }
    };
    Ok(wrap(it, r))
}

fn unary_current(it: &mut Interp, a: &Value, f: Un) -> R<Value> {
    let cx = current(it)?;
    let x = convert_op_raise(it, a)?;
    let r = run(it, &cx, |c, st| f(&x, c, st))?;
    Ok(wrap(it, r))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// The operands of a comparison with `w`: `a` and the decimal `w` stands for, or `None` when `w`
/// cannot be compared. Floats and complex numbers compare exactly and set `FloatOperation`;
/// rationals compare by cross-multiplication.
fn cmp_operands(it: &mut Interp, cx: &Py<PyContext>, a: &dec::Decimal, w: &Value, op: Op) -> R<Option<(dec::Decimal, dec::Decimal)>> {
    if let Some(b) = convert_op(it, w)? {
        return Ok(Some((a.clone(), b)));
    }
    let eq_like = matches!(op, Op::Eq | Op::Ne);
    if let Some(f) = float_of(w) {
        if eq_like {
            mark(it, cx, flag::FLOAT_OPERATION)?;
        } else {
            finish(it, cx, flag::FLOAT_OPERATION)?;
        }
        let b = exact_float(it, f)?;
        return Ok(Some((a.clone(), b)));
    }
    if eq_like {
        if let Some((re, im)) = complex_of(w) {
            if im != 0.0 {
                return Ok(None);
            }
            mark(it, cx, flag::FLOAT_OPERATION)?;
            let b = exact_float(it, re)?;
            return Ok(Some((a.clone(), b)));
        }
    }
    let rational = it.native_state::<State>().rational.clone();
    if let Some(r) = rational {
        if it.isinstance_value(w, &r)? {
            let n = it.get_attr_str(w, "numerator")?;
            let d = it.get_attr_str(w, "denominator")?;
            let (Some(n), Some(d)) = (n.as_bigint(), d.as_bigint()) else { return Ok(None) };
            let mut st: dec::Status = 0;
            let scaled = a.mul(&dec::Decimal::from_bigint(&d), &dec::Context::max(), &mut st);
            return Ok(Some((scaled, dec::Decimal::from_bigint(&n))));
        }
    }
    Ok(None)
}

fn richcmp(it: &mut Interp, slf: &Value, other: &Value, op: Op) -> R<Value> {
    let cx = current(it)?;
    let a = convert_op_raise(it, slf)?;
    let Some((a, b)) = cmp_operands(it, &cx, &a, other, op)? else { return Ok(Value::NotImplemented) };
    match a.compare_numeric(&b) {
        None => {
            if a.is_snan() || b.is_snan() || !matches!(op, Op::Eq | Op::Ne) {
                finish(it, &cx, flag::INVALID_OPERATION)?;
            }
            Ok(Value::Bool(op == Op::Ne))
        }
        Some(o) => Ok(Value::Bool(match op {
            Op::Eq => o == Ordering::Equal,
            Op::Ne => o != Ordering::Equal,
            Op::Lt => o == Ordering::Less,
            Op::Le => o != Ordering::Greater,
            Op::Gt => o == Ordering::Greater,
            Op::Ge => o != Ordering::Less,
        })),
    }
}

// ---- method helpers ----------------------------------------------------------------------------

fn method_un(it: &mut Interp, slf: &Value, args: &[Value], kw: KwArgs<'_>, f: Un) -> R<Value> {
    let v = parse_args(it, args, kw, &["context"], 0)?;
    let cx = ctx_arg(it, v[0].as_ref())?;
    let x = convert_op_raise(it, slf)?;
    let r = run(it, &cx, |c, st| f(&x, c, st))?;
    Ok(wrap(it, r))
}

fn method_bin(it: &mut Interp, slf: &Value, args: &[Value], kw: KwArgs<'_>, f: Bin) -> R<Value> {
    let v = parse_args(it, args, kw, &["other", "context"], 1)?;
    let cx = ctx_arg(it, v[1].as_ref())?;
    let y = convert_op_raise(it, v[0].as_ref().unwrap_or(&Value::None))?;
    let x = convert_op_raise(it, slf)?;
    let r = run(it, &cx, |c, st| f(&x, &y, c, st))?;
    Ok(wrap(it, r))
}

fn method_ctx(it: &mut Interp, args: &[Value], kw: KwArgs<'_>) -> R<Py<PyContext>> {
    let v = parse_args(it, args, kw, &["context"], 0)?;
    ctx_arg(it, v[0].as_ref())
}

fn integer_of(it: &mut Interp, d: &dec::Decimal, mode: Option<Rounding>) -> R<Value> {
    if d.is_nan() {
        return Err(it.value_error("cannot convert NaN to integer"));
    }
    if d.is_infinite() {
        return Err(it.overflow_err("cannot convert Infinity to integer"));
    }
    let r = match mode {
        None => d.to_bigint_trunc(),
        Some(m) => d.to_bigint_rounded(m),
    };
    match r {
        Ok(b) => Ok(Value::big(b)),
        Err(_) => Err(memory_error(it)),
    }
}

fn digit_values(d: &dec::Decimal) -> Vec<Value> {
    d.coefficient().to_string_radix(10).bytes().map(|b| Value::Int((b - b'0') as i64)).collect()
}

fn locale_of(it: &mut Interp, ovr: &Value) -> R<dec::Locale> {
    let (Some(_), Value::Obj(d)) = (dict_of(ovr), ovr) else {
        return Err(it.type_error("optional argument must be a dict"));
    };
    let mut loc = dec::Locale::default();
    for key in ["decimal_point", "thousands_sep", "grouping"] {
        let k = Value::str(key);
        let Some(v) = it.dict_get(d, &k)? else { continue };
        let Some(s) = v.as_str() else { return Err(it.type_error("bad argument type for built-in operation")) };
        match key {
            "decimal_point" => loc.decimal_point = s.to_string(),
            "thousands_sep" => loc.thousands_sep = s.to_string(),
            _ => loc.grouping = s.chars().map(|c| c as i32).collect(),
        }
    }
    Ok(loc)
}

#[lumen_bind::methods]
impl PyDecimal {
    /// Construct a new Decimal object. 'value' can be an integer, string, tuple, or another Decimal
    /// object. If no value is given, return Decimal('0'). The context does not affect the
    /// conversion and is only passed to determine if the InvalidOperation trap is active.
    #[constructor]
    fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        let v = parse_args(it, args, kw, &["value", "context"], 0)?;
        let cx = ctx_arg(it, v[1].as_ref())?;
        let Value::Obj(cls) = cls.0 else { return Err(it.type_error("Decimal.__new__(X): X is not a type object")) };
        let value = v[0].clone().unwrap_or_else(|| Value::str("0"));
        let exact = type_object::<PyDecimal>(it);
        if Rc::ptr_eq(&cls, &exact) && it.is_exact(&value, &exact) {
            return Ok(value);
        }
        let d = from_object(it, &value, &cx, true)?;
        Ok(opaque_instance(&cls, PyDecimal { v: d }))
    }

    /// Class method that converts a real number to a decimal number, exactly.
    ///
    ///     >>> Decimal.from_float(0.1)
    ///     Decimal('0.1000000000000000055511151231257827021181583404541015625')
    #[classmethod]
    fn from_float(cls: This<Value>, it: &mut Interp, f: &Value) -> R<Value> {
        let d = exact_from_number(it, f)?;
        let v = wrap(it, d);
        let exact = type_object::<PyDecimal>(it);
        if let Value::Obj(c) = &cls.0 {
            if Rc::ptr_eq(c, &exact) {
                return Ok(v);
            }
        }
        it.call(&cls.0, vec![v], Vec::new())
    }

    /// Class method that converts a real number to a decimal number, exactly.
    ///
    ///     >>> Decimal.from_number(314)              # int
    ///     Decimal('314')
    ///     >>> Decimal.from_number(0.1)              # float
    ///     Decimal('0.1000000000000000055511151231257827021181583404541015625')
    ///     >>> Decimal.from_number(Decimal('3.14'))  # another decimal instance
    ///     Decimal('3.14')
    #[classmethod]
    fn from_number(cls: This<Value>, it: &mut Interp, number: &Value) -> R<Value> {
        let exact = type_object::<PyDecimal>(it);
        let is_exact_cls = matches!(&cls.0, Value::Obj(c) if Rc::ptr_eq(c, &exact));
        let v = if let Some(p) = Py::<PyDecimal>::from_value(it, number) {
            if is_exact_cls && matches!(number, Value::Obj(o) if o.cls.is_none()) {
                return Ok(number.clone());
            }
            let d = p.borrow(it)?.v.clone();
            wrap(it, d)
        } else if number.as_bigint().is_some() || float_of(number).is_some() {
            let d = exact_from_number(it, number)?;
            wrap(it, d)
        } else {
            let t = it.tp_name_of(number);
            return Err(it.type_error(&format!("conversion from {} to Decimal is not supported", t)));
        };
        if is_exact_cls {
            return Ok(v);
        }
        it.call(&cls.0, vec![v], Vec::new())
    }

    #[getter]
    fn real(slf: This<Value>) -> Value {
        slf.0
    }

    #[getter]
    fn imag(slf: This<Value>, it: &mut Interp) -> Value {
        let _ = slf;
        wrap(it, dec::Decimal::zero())
    }

    #[proto(repr)]
    fn repr(slf: This<Value>, it: &mut Interp) -> R<String> {
        let caps = current(it)?.borrow(it)?.capitals;
        let d = convert_op_raise(it, &slf.0)?;
        Ok(format!("Decimal('{}')", d.to_sci_string(caps)))
    }

    #[proto(str)]
    fn to_str(slf: This<Value>, it: &mut Interp) -> R<String> {
        let caps = current(it)?.borrow(it)?.capitals;
        let d = convert_op_raise(it, &slf.0)?;
        Ok(d.to_sci_string(caps))
    }

    #[proto(hash)]
    fn hash(slf: This<Value>, it: &mut Interp) -> R<i64> {
        let d = convert_op_raise(it, &slf.0)?;
        if d.is_snan() {
            return Err(it.type_error("Cannot hash a signaling NaN value"));
        }
        match d.numeric_hash() {
            Some(h) => Ok(h),
            None => Ok(it.id_of(&slf.0).rotate_right(4) as i64),
        }
    }

    #[proto(bool)]
    fn truth(&self) -> bool {
        !self.v.is_zero()
    }

    #[proto(eq)]
    fn eq(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        richcmp(it, &slf.0, other, Op::Eq)
    }

    #[proto(ne)]
    fn ne(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        richcmp(it, &slf.0, other, Op::Ne)
    }

    #[proto(lt)]
    fn lt(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        richcmp(it, &slf.0, other, Op::Lt)
    }

    #[proto(le)]
    fn le(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        richcmp(it, &slf.0, other, Op::Le)
    }

    #[proto(gt)]
    fn gt(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        richcmp(it, &slf.0, other, Op::Gt)
    }

    #[proto(ge)]
    fn ge(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        richcmp(it, &slf.0, other, Op::Ge)
    }

    #[proto(add)]
    fn add(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        arith(it, &slf.0, other, dec::Decimal::add)
    }

    #[proto(radd)]
    fn radd(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        arith(it, other, &slf.0, dec::Decimal::add)
    }

    #[proto(sub)]
    fn sub(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        arith(it, &slf.0, other, dec::Decimal::sub)
    }

    #[proto(rsub)]
    fn rsub(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        arith(it, other, &slf.0, dec::Decimal::sub)
    }

    #[proto(mul)]
    fn mul(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        arith(it, &slf.0, other, dec::Decimal::mul)
    }

    #[proto(rmul)]
    fn rmul(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        arith(it, other, &slf.0, dec::Decimal::mul)
    }

    #[proto(truediv)]
    fn truediv(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        arith(it, &slf.0, other, dec::Decimal::div)
    }

    #[proto(rtruediv)]
    fn rtruediv(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        arith(it, other, &slf.0, dec::Decimal::div)
    }

    #[proto(floordiv)]
    fn floordiv(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        arith(it, &slf.0, other, dec::Decimal::divint)
    }

    #[proto(rfloordiv)]
    fn rfloordiv(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        arith(it, other, &slf.0, dec::Decimal::divint)
    }

    #[proto(mod)]
    fn r#mod(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        arith(it, &slf.0, other, dec::Decimal::rem)
    }

    #[proto(rmod)]
    fn rmod(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        arith(it, other, &slf.0, dec::Decimal::rem)
    }

    #[proto(divmod)]
    fn divmod(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        divmod_op(it, &slf.0, other)
    }

    #[proto(rdivmod)]
    fn rdivmod(slf: This<Value>, it: &mut Interp, other: &Value) -> R<Value> {
        divmod_op(it, other, &slf.0)
    }

    #[proto(pow)]
    fn pow(slf: This<Value>, it: &mut Interp, other: &Value, modulo: Option<&Value>) -> R<Value> {
        power_op(it, &slf.0, other, modulo)
    }

    #[proto(rpow)]
    fn rpow(slf: This<Value>, it: &mut Interp, other: &Value, modulo: Option<&Value>) -> R<Value> {
        power_op(it, other, &slf.0, modulo)
    }

    #[proto(neg)]
    fn neg(slf: This<Value>, it: &mut Interp) -> R<Value> {
        unary_current(it, &slf.0, dec::Decimal::minus)
    }

    #[proto(pos)]
    fn pos(slf: This<Value>, it: &mut Interp) -> R<Value> {
        unary_current(it, &slf.0, dec::Decimal::plus)
    }

    #[proto(abs)]
    fn abs(slf: This<Value>, it: &mut Interp) -> R<Value> {
        unary_current(it, &slf.0, dec::Decimal::abs)
    }

    #[proto(int)]
    fn to_int_value(&self, it: &mut Interp) -> R<Value> {
        integer_of(it, &self.v, None)
    }

    #[proto(float)]
    fn to_float_value(&self, it: &mut Interp) -> R<f64> {
        if self.v.is_snan() {
            return Err(it.value_error("cannot convert signaling NaN to float"));
        }
        Ok(self.v.to_f64().unwrap_or(f64::NAN))
    }

    #[method(name = "__complex__")]
    fn complex(&self, it: &mut Interp) -> R<Value> {
        let f = self.to_float_value(it)?;
        Ok(Value::Obj(Object::new(Kind::Complex(f, 0.0))))
    }

    #[method(name = "__trunc__")]
    fn trunc(&self, it: &mut Interp) -> R<Value> {
        integer_of(it, &self.v, None)
    }

    #[method(name = "__floor__")]
    fn floor(&self, it: &mut Interp) -> R<Value> {
        integer_of(it, &self.v, Some(Rounding::Floor))
    }

    #[method(name = "__ceil__")]
    fn ceil(&self, it: &mut Interp) -> R<Value> {
        integer_of(it, &self.v, Some(Rounding::Ceiling))
    }

    /// Round self to the nearest integer, or to a given precision in decimal digits (default 0
    /// digits). This always returns an integer when ndigits is omitted or None.
    #[method(name = "__round__")]
    fn round(slf: This<Value>, it: &mut Interp, ndigits: Option<&Value>) -> R<Value> {
        let d = convert_op_raise(it, &slf.0)?;
        let Some(n) = ndigits.filter(|n| !n.is_none()) else {
            return integer_of(it, &d, Some(Rounding::HalfEven));
        };
        if !n.is_int_like() {
            return Err(it.type_error("optional arg must be an integer"));
        }
        let y = ssize(it, n)?;
        let cx = current(it)?;
        let q = dec::Decimal::finite(false, dec::Decimal::one().coefficient().clone(), y.saturating_neg());
        let r = run(it, &cx, |c, st| d.quantize(&q, c.round, c, st))?;
        Ok(wrap(it, r))
    }

    #[method(name = "__format__")]
    fn format(slf: This<Value>, it: &mut Interp, spec: &Value, ovr: Option<&Value>) -> R<String> {
        let Some(text) = spec.as_str() else { return Err(it.type_error("format arg must be str")) };
        let text = text.to_string();
        let locale = match ovr {
            None | Some(Value::None) => None,
            Some(o) => Some(locale_of(it, o)?),
        };
        let d = convert_op_raise(it, &slf.0)?;
        let cx = current(it)?;
        let core = cx.borrow(it)?.core();
        let result = dec::parse_format_spec(&text).and_then(|s| d.format(&s, locale.as_ref(), &core));
        match result {
            Ok(s) => Ok(s),
            Err(dec::FormatError::Invalid) => Err(it.value_error("invalid format string")),
            Err(dec::FormatError::Overflow) => Err(it.overflow_err("cannot fit 'int' into an index-sized integer")),
            Err(dec::FormatError::Grouping(m)) => Err(it.value_error(m)),
            Err(dec::FormatError::Memory) => Err(memory_error(it)),
        }
    }

    #[proto(reduce)]
    fn reduce(slf: This<Value>, it: &mut Interp) -> R<Value> {
        let caps = current(it)?.borrow(it)?.capitals;
        let d = convert_op_raise(it, &slf.0)?;
        let cls = Value::Obj(it.type_of(&slf.0));
        Ok(Value::tuple(vec![cls, Value::tuple(vec![Value::string(d.to_sci_string(caps))])]))
    }

    #[proto(copy)]
    fn dunder_copy(slf: This<Value>) -> Value {
        slf.0
    }

    #[proto(deepcopy)]
    fn dunder_deepcopy(slf: This<Value>, memo: &Value) -> Value {
        let _ = memo;
        slf.0
    }

    #[proto(sizeof)]
    fn sizeof(&self) -> usize {
        48 + 8 * (self.v.digits() as usize / 19 + 1)
    }

    /// Return a tuple of the sign, digits and exponent.
    #[method]
    fn as_tuple(slf: This<Value>, it: &mut Interp) -> R<Value> {
        let d = convert_op_raise(it, &slf.0)?;
        let (digits, exp) = match d.kind() {
            dec::Kind::Infinity => (vec![Value::Int(0)], Value::str("F")),
            dec::Kind::NaN | dec::Kind::SNaN => {
                let digits = if d.coefficient().is_zero() { Vec::new() } else { digit_values(&d) };
                (digits, Value::str(if d.is_snan() { "N" } else { "n" }))
            }
            dec::Kind::Finite => (digit_values(&d), Value::Int(d.exponent())),
        };
        let cls = it.native_state::<State>().tuple.clone();
        let Some(cls) = cls else { return Err(it.runtime_error("_decimal is not initialised")) };
        it.call(&cls, vec![Value::Int(d.is_negative() as i64), Value::tuple(digits), exp], Vec::new())
    }

    /// Decimal.as_integer_ratio() -> (int, int)
    ///
    /// Return a pair of integers, whose ratio is exactly equal to the original Decimal and with a
    /// positive denominator. The ratio is in lowest terms. Raise OverflowError on infinities and a
    /// ValueError on NaNs.
    #[method]
    fn as_integer_ratio(&self, it: &mut Interp) -> R<Value> {
        if self.v.is_nan() {
            return Err(it.value_error("cannot convert NaN to integer ratio"));
        }
        if self.v.is_infinite() {
            return Err(it.overflow_err("cannot convert Infinity to integer ratio"));
        }
        match self.v.as_integer_ratio() {
            Ok((n, d)) => Ok(Value::tuple(vec![Value::big(n), Value::big(d)])),
            Err(_) => Err(memory_error(it)),
        }
    }

    /// Return self.
    #[method]
    fn conjugate(slf: This<Value>) -> Value {
        slf.0
    }

    /// Return the adjusted exponent of the number.
    #[method]
    fn adjusted(&self) -> i64 {
        self.v.adjusted() as i64
    }

    /// Return self.
    #[method]
    fn canonical(slf: This<Value>) -> Value {
        slf.0
    }

    /// Return True if the argument is canonical and False otherwise.
    #[method]
    fn is_canonical(&self) -> bool {
        true
    }

    #[method]
    fn is_finite(&self) -> bool {
        self.v.is_finite()
    }

    #[method]
    fn is_infinite(&self) -> bool {
        self.v.is_infinite()
    }

    #[method]
    fn is_nan(&self) -> bool {
        self.v.is_nan()
    }

    #[method]
    fn is_qnan(&self) -> bool {
        self.v.is_qnan()
    }

    #[method]
    fn is_snan(&self) -> bool {
        self.v.is_snan()
    }

    #[method]
    fn is_signed(&self) -> bool {
        self.v.is_negative()
    }

    #[method]
    fn is_zero(&self) -> bool {
        self.v.is_zero()
    }

    #[method]
    fn is_normal(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<bool> {
        let cx = method_ctx(it, args, kw)?;
        let d = convert_op_raise(it, &slf.0)?;
        let c = cx.borrow(it)?.core();
        Ok(d.is_normal(&c))
    }

    #[method]
    fn is_subnormal(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<bool> {
        let cx = method_ctx(it, args, kw)?;
        let d = convert_op_raise(it, &slf.0)?;
        let c = cx.borrow(it)?.core();
        Ok(d.is_subnormal(&c))
    }

    /// Return an indication of the class of self.
    #[method]
    fn number_class(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<&'static str> {
        let cx = method_ctx(it, args, kw)?;
        let d = convert_op_raise(it, &slf.0)?;
        let c = cx.borrow(it)?.core();
        Ok(dec::class_name(d.number_class(&c)))
    }

    /// Return the value 10.
    #[method]
    fn radix(slf: This<Value>, it: &mut Interp) -> Value {
        let _ = slf;
        wrap(it, dec::Decimal::radix())
    }

    #[method]
    fn copy_abs(&self, it: &mut Interp) -> Value {
        wrap(it, self.v.copy_abs())
    }

    #[method]
    fn copy_negate(&self, it: &mut Interp) -> Value {
        wrap(it, self.v.copy_negate())
    }

    /// Return a copy of the first operand with the sign of the second.
    #[method]
    fn copy_sign(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        let v = parse_args(it, args, kw, &["other", "context"], 1)?;
        let _cx = ctx_arg(it, v[1].as_ref())?;
        let y = convert_op_raise(it, v[0].as_ref().unwrap_or(&Value::None))?;
        let x = convert_op_raise(it, &slf.0)?;
        Ok(wrap(it, x.copy_sign(&y)))
    }

    /// Return True if the two operands have the same exponent.
    #[method]
    fn same_quantum(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<bool> {
        let v = parse_args(it, args, kw, &["other", "context"], 1)?;
        let _cx = ctx_arg(it, v[1].as_ref())?;
        let y = convert_op_raise(it, v[0].as_ref().unwrap_or(&Value::None))?;
        let x = convert_op_raise(it, &slf.0)?;
        Ok(x.same_quantum(&y))
    }

    /// Convert to an engineering-type string.
    #[method]
    fn to_eng_string(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<String> {
        let cx = method_ctx(it, args, kw)?;
        let d = convert_op_raise(it, &slf.0)?;
        let caps = cx.borrow(it)?.capitals;
        Ok(d.to_eng_string(caps))
    }

    /// Return a value equal to the first operand after rounding and having the exponent of the
    /// second operand.
    #[method]
    fn quantize(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        let v = parse_args(it, args, kw, &["exp", "rounding", "context"], 1)?;
        let cx = ctx_arg(it, v[2].as_ref())?;
        let q = convert_op_raise(it, v[0].as_ref().unwrap_or(&Value::None))?;
        let d = convert_op_raise(it, &slf.0)?;
        let mode = match v[1].as_ref() {
            None | Some(Value::None) => cx.borrow(it)?.round,
            Some(r) => rounding_of(it, r)?,
        };
        let r = run(it, &cx, |c, st| d.quantize(&q, mode, c, st))?;
        Ok(wrap(it, r))
    }

    /// Round to an integer.
    #[method]
    fn to_integral(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        to_integral_with(it, &slf.0, args, kw, false)
    }

    /// Round to an integer.
    #[method]
    fn to_integral_value(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        to_integral_with(it, &slf.0, args, kw, false)
    }

    /// Round to an integer, signaling Inexact and Rounded as appropriate.
    #[method]
    fn to_integral_exact(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        to_integral_with(it, &slf.0, args, kw, true)
    }

    /// Return the fused multiply-add of the three operands.
    #[method]
    fn fma(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        let v = parse_args(it, args, kw, &["other", "third", "context"], 2)?;
        let cx = ctx_arg(it, v[2].as_ref())?;
        let y = convert_op_raise(it, v[0].as_ref().unwrap_or(&Value::None))?;
        let z = convert_op_raise(it, v[1].as_ref().unwrap_or(&Value::None))?;
        let x = convert_op_raise(it, &slf.0)?;
        let r = run(it, &cx, |c, st| x.fma(&y, &z, c, st))?;
        Ok(wrap(it, r))
    }

    #[method]
    fn compare(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::compare)
    }

    #[method]
    fn compare_signal(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::compare_signal)
    }

    #[method]
    fn compare_total(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, |x, y, _, _| x.compare_total(y))
    }

    #[method]
    fn compare_total_mag(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, |x, y, _, _| x.compare_total_mag(y))
    }

    #[method]
    fn max(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::max)
    }

    #[method]
    fn max_mag(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::max_mag)
    }

    #[method]
    fn min(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::min)
    }

    #[method]
    fn min_mag(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::min_mag)
    }

    #[method]
    fn next_toward(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::next_toward)
    }

    #[method]
    fn remainder_near(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::rem_near)
    }

    #[method]
    fn rotate(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::rotate)
    }

    #[method]
    fn scaleb(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::scaleb)
    }

    #[method]
    fn shift(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::shift)
    }

    #[method]
    fn logical_and(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::logical_and)
    }

    #[method]
    fn logical_or(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::logical_or)
    }

    #[method]
    fn logical_xor(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_bin(it, &slf.0, args, kw, dec::Decimal::logical_xor)
    }

    #[method]
    fn logical_invert(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_un(it, &slf.0, args, kw, dec::Decimal::logical_invert)
    }

    #[method]
    fn exp(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_un(it, &slf.0, args, kw, dec::Decimal::exp)
    }

    #[method]
    fn ln(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_un(it, &slf.0, args, kw, dec::Decimal::ln)
    }

    #[method]
    fn log10(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_un(it, &slf.0, args, kw, dec::Decimal::log10)
    }

    #[method]
    fn logb(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_un(it, &slf.0, args, kw, dec::Decimal::logb)
    }

    #[method]
    fn next_minus(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_un(it, &slf.0, args, kw, dec::Decimal::next_minus)
    }

    #[method]
    fn next_plus(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_un(it, &slf.0, args, kw, dec::Decimal::next_plus)
    }

    #[method]
    fn normalize(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_un(it, &slf.0, args, kw, dec::Decimal::normalize)
    }

    #[method]
    fn sqrt(slf: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        method_un(it, &slf.0, args, kw, dec::Decimal::sqrt)
    }
}

fn to_integral_with(it: &mut Interp, slf: &Value, args: &[Value], kw: KwArgs<'_>, exact: bool) -> R<Value> {
    let v = parse_args(it, args, kw, &["rounding", "context"], 0)?;
    let cx = ctx_arg(it, v[1].as_ref())?;
    let mode = match v[0].as_ref() {
        None | Some(Value::None) => cx.borrow(it)?.round,
        Some(r) => rounding_of(it, r)?,
    };
    let d = convert_op_raise(it, slf)?;
    let r = run(it, &cx, |c, st| if exact { d.to_integral_exact(mode, c, st) } else { d.to_integral_value(mode, c, st) })?;
    Ok(wrap(it, r))
}
