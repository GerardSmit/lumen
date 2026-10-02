//! `Context`, its signal dictionaries and the context manager, plus the per-interpreter state of
//! the module: the signal exception classes, the current-context variable and the templates.

use super::decimal::{convert_op_raise, wrap, PyDecimal};
use super::support::*;
use crate::bind::{opaque_instance, KwArgs, Py, This};
use crate::object::*;
use crate::vm::Interp;
use lumen_common::decimal as dec;
use lumen_bind::Passed;
use lumen_common::decimal::{flag, Rounding};
use std::cell::Cell;
use std::rc::Rc;

pub const SIGNAL_NAMES: [&str; 9] =
    ["InvalidOperation", "FloatOperation", "DivisionByZero", "Overflow", "Underflow", "Subnormal", "Inexact", "Rounded", "Clamped"];

pub const SIGNAL_FLAGS: [u32; 9] = [
    flag::IEEE_INVALID_OPERATION,
    flag::FLOAT_OPERATION,
    flag::DIVISION_BY_ZERO,
    flag::OVERFLOW,
    flag::UNDERFLOW,
    flag::SUBNORMAL,
    flag::INEXACT,
    flag::ROUNDED,
    flag::CLAMPED,
];

const SIGNAL_MAP_ERR: &str = "valid values for signals are:\n  [InvalidOperation, FloatOperation, DivisionByZero,\n   Overflow, Underflow, Subnormal, Inexact, Rounded,\n   Clamped]";

const ROUND_ERR: &str = "valid values for rounding are:\n  [ROUND_CEILING, ROUND_FLOOR, ROUND_UP, ROUND_DOWN,\n   ROUND_HALF_UP, ROUND_HALF_DOWN, ROUND_HALF_EVEN,\n   ROUND_05UP]";

const SETTING_NAMES: [&str; 8] = ["prec", "rounding", "Emin", "Emax", "capitals", "clamp", "flags", "traps"];

/// The exception classes of the signals: `signals` in the order of `SIGNAL_NAMES`, `conds` the
/// finer invalid-operation conditions that `flags_as_list` reports before them.
pub struct Excs {
    pub decimal_exception: Obj,
    pub signals: Vec<Obj>,
    pub conds: Vec<(u32, Obj)>,
}

#[derive(Default)]
pub struct State {
    pub excs: Option<Rc<Excs>>,
    pub get: Option<Value>,
    pub set: Option<Value>,
    pub templates: Option<[Py<PyContext>; 3]>,
    pub signal_dict: Option<Obj>,
    pub tuple: Option<Value>,
    pub rational: Option<Value>,
}

pub fn excs(it: &mut Interp) -> Rc<Excs> {
    it.native_state::<State>().excs.clone().expect("_decimal is initialised before any of its objects exist")
}

fn template(it: &mut Interp, i: usize) -> R<Py<PyContext>> {
    match it.native_state::<State>().templates.as_ref().map(|t| t[i].clone()) {
        Some(t) => Ok(t),
        None => Err(it.runtime_error("_decimal is not initialised")),
    }
}

// A decimal context: the arithmetic parameters plus shared cells for the flag and trap words,
// which the `SignalDict` views of `flags` and `traps` hold on to.
#[lumen_bind::class(name = "Context", module = "decimal")]
pub struct PyContext {
    pub prec: i64,
    pub emax: i64,
    pub emin: i64,
    pub round: Rounding,
    pub clamp: bool,
    pub capitals: bool,
    pub flags: Rc<Cell<u32>>,
    pub traps: Rc<Cell<u32>>,
    flags_obj: Option<Value>,
    traps_obj: Option<Value>,
}

impl PyContext {
    pub fn with_traps(prec: i64, round: Rounding, traps: u32) -> PyContext {
        PyContext {
            prec,
            emax: 999_999,
            emin: -999_999,
            round,
            clamp: false,
            capitals: true,
            flags: Rc::new(Cell::new(0)),
            traps: Rc::new(Cell::new(traps)),
            flags_obj: None,
            traps_obj: None,
        }
    }

    pub fn core(&self) -> dec::Context {
        dec::Context {
            prec: self.prec,
            emax: self.emax,
            emin: self.emin,
            round: self.round,
            clamp: self.clamp,
            capitals: self.capitals,
            traps: self.traps.get(),
            flags: self.flags.get(),
        }
    }

    fn duplicate(&self, clear_flags: bool) -> PyContext {
        PyContext {
            prec: self.prec,
            emax: self.emax,
            emin: self.emin,
            round: self.round,
            clamp: self.clamp,
            capitals: self.capitals,
            flags: Rc::new(Cell::new(if clear_flags { 0 } else { self.flags.get() })),
            traps: Rc::new(Cell::new(self.traps.get())),
            flags_obj: None,
            traps_obj: None,
        }
    }
}

/// A new context holding the settings (and, unless cleared, the flags) of `src`.
pub fn copy_context(it: &mut Interp, src: &Py<PyContext>, clear_flags: bool) -> R<Py<PyContext>> {
    let n = src.borrow(it)?.duplicate(clear_flags);
    Ok(Py::new(it, n))
}

/// The three template contexts, in the order `DefaultContext`, `BasicContext`, `ExtendedContext`.
pub fn make_templates(it: &mut Interp) -> [Py<PyContext>; 3] {
    let default = Py::new(it, PyContext::with_traps(28, Rounding::HalfEven, flag::IEEE_INVALID_OPERATION | flag::DIVISION_BY_ZERO | flag::OVERFLOW));
    let basic_traps = flag::IEEE_INVALID_OPERATION | flag::DIVISION_BY_ZERO | flag::OVERFLOW | flag::CLAMPED | flag::UNDERFLOW;
    let basic = Py::new(it, PyContext::with_traps(9, Rounding::HalfUp, basic_traps));
    let extended = Py::new(it, PyContext::with_traps(9, Rounding::HalfEven, 0));
    [default, basic, extended]
}

// ---- the current context -----------------------------------------------------------------------

/// The context of the running thread / task, created from `DefaultContext` on first use.
pub fn current(it: &mut Interp) -> R<Py<PyContext>> {
    let get = it.native_state::<State>().get.clone();
    let Some(get) = get else { return Err(it.runtime_error("_decimal is not initialised")) };
    let v = it.call(&get, vec![Value::None], Vec::new())?;
    if let Some(c) = Py::<PyContext>::from_value(it, &v) {
        return Ok(c);
    }
    let tmpl = template(it, 0)?;
    let c = copy_context(it, &tmpl, true)?;
    set_current(it, &c)?;
    Ok(c)
}

pub fn set_current(it: &mut Interp, c: &Py<PyContext>) -> R<()> {
    let set = it.native_state::<State>().set.clone();
    let Some(set) = set else { return Err(it.runtime_error("_decimal is not initialised")) };
    it.call(&set, vec![c.value().clone()], Vec::new())?;
    Ok(())
}

/// The optional `context` argument: the current context when absent or `None`.
pub fn ctx_arg(it: &mut Interp, v: Option<&Value>) -> R<Py<PyContext>> {
    match v {
        None | Some(Value::None) => current(it),
        Some(v) => match Py::<PyContext>::from_value(it, v) {
            Some(c) => Ok(c),
            None => Err(it.type_error("optional argument must be a context")),
        },
    }
}

// ---- signals ---------------------------------------------------------------------------------

pub fn flags_as_list(it: &mut Interp, flags: u32) -> Value {
    let e = excs(it);
    let mut out = Vec::new();
    for (f, cls) in &e.conds {
        if flags & f != 0 {
            out.push(Value::Obj(cls.clone()));
        }
    }
    for i in 1..SIGNAL_FLAGS.len() {
        if flags & SIGNAL_FLAGS[i] != 0 {
            out.push(Value::Obj(e.signals[i].clone()));
        }
    }
    Value::list(out)
}

fn signals_as_list(it: &mut Interp, flags: u32) -> Value {
    let e = excs(it);
    let out = (0..SIGNAL_FLAGS.len()).filter(|&i| flags & SIGNAL_FLAGS[i] != 0).map(|i| Value::Obj(e.signals[i].clone())).collect();
    Value::list(out)
}

fn signal_names(flags: u32) -> String {
    let names: Vec<&str> = (0..SIGNAL_FLAGS.len()).filter(|&i| flags & SIGNAL_FLAGS[i] != 0).map(|i| SIGNAL_NAMES[i]).collect();
    format!("[{}]", names.join(", "))
}

fn signal_flag(it: &mut Interp, key: &Value) -> R<u32> {
    let e = excs(it);
    if let Value::Obj(k) = key {
        if let Some(i) = e.signals.iter().position(|s| Rc::ptr_eq(s, k)) {
            return Ok(SIGNAL_FLAGS[i]);
        }
    }
    Err(key_error(it, SIGNAL_MAP_ERR))
}

fn signal_error(it: &mut Interp, trapped: u32) -> Obj {
    let e = excs(it);
    let cls = SIGNAL_FLAGS.iter().position(|f| trapped & f != 0).map(|i| e.signals[i].clone()).unwrap_or_else(|| e.decimal_exception.clone());
    let list = flags_as_list(it, trapped);
    it.new_exc(&cls, vec![list])
}

/// Records the conditions of an operation in the flags of `cx` and raises the trapped one.
pub fn finish(it: &mut Interp, cx: &Py<PyContext>, st: u32) -> R<()> {
    if st == 0 {
        return Ok(());
    }
    let (flags, traps) = {
        let b = cx.borrow(it)?;
        (b.flags.clone(), b.traps.get())
    };
    flags.set(flags.get() | (st & flag::SIGNALS));
    let trapped = st & (traps | flag::MALLOC_ERROR);
    if trapped == 0 {
        return Ok(());
    }
    if trapped & flag::MALLOC_ERROR != 0 {
        return Err(memory_error(it));
    }
    Err(signal_error(it, trapped))
}

/// Runs `f` against a snapshot of `cx`, then records and raises what it signalled.
pub fn run<T>(it: &mut Interp, cx: &Py<PyContext>, f: impl FnOnce(&dec::Context, &mut dec::Status) -> T) -> R<T> {
    let c = cx.borrow(it)?.core();
    let mut st: dec::Status = 0;
    let out = f(&c, &mut st);
    finish(it, cx, st)?;
    Ok(out)
}

/// Sets the flag without raising (the silent `FloatOperation` of equality comparisons).
pub fn mark(it: &mut Interp, cx: &Py<PyContext>, bits: u32) -> R<()> {
    let flags = cx.borrow(it)?.flags.clone();
    flags.set(flags.get() | bits);
    Ok(())
}

fn list_as_flags(it: &mut Interp, items: &[Value]) -> R<u32> {
    let mut flags = 0;
    for x in items {
        flags |= signal_flag(it, x)?;
    }
    Ok(flags)
}

fn dict_as_flags(it: &mut Interp, v: &Value) -> R<u32> {
    let (Some(cell), Value::Obj(d)) = (dict_of(v), v) else {
        return Err(it.type_error("argument must be a signal dict"));
    };
    if cell.borrow().len() != SIGNAL_FLAGS.len() {
        return Err(key_error(it, "invalid signal dict"));
    }
    let e = excs(it);
    let mut flags = 0;
    for (i, cls) in e.signals.iter().enumerate() {
        match it.dict_get(d, &Value::Obj(cls.clone()))? {
            None => return Err(key_error(it, "invalid signal dict")),
            Some(x) => {
                if it.truthy(&x)? {
                    flags |= SIGNAL_FLAGS[i];
                }
            }
        }
    }
    Ok(flags)
}

fn signal_bits(it: &mut Interp, v: &Value, allow_list: bool) -> R<u32> {
    if let Some(sd) = Py::<SignalDict>::from_value(it, v) {
        let cell = sd.borrow(it)?.cell.clone();
        return Ok(cell.get());
    }
    if allow_list {
        if let Some(l) = list_of(v) {
            let items = l.borrow().clone();
            return list_as_flags(it, &items);
        }
    }
    dict_as_flags(it, v)
}

fn signal_dict_of(it: &mut Interp, c: &Py<PyContext>, traps: bool) -> R<Value> {
    let (cached, cell) = {
        let b = c.borrow(it)?;
        if traps {
            (b.traps_obj.clone(), b.traps.clone())
        } else {
            (b.flags_obj.clone(), b.flags.clone())
        }
    };
    if let Some(v) = cached {
        return Ok(v);
    }
    let cls = match it.native_state::<State>().signal_dict.clone() {
        Some(c) => c,
        None => return Err(it.runtime_error("_decimal is not initialised")),
    };
    let v = opaque_instance(&cls, SignalDict { cell, traps });
    c.with(it, |b| {
        if traps {
            b.traps_obj = Some(v.clone());
        } else {
            b.flags_obj = Some(v.clone());
        }
    })?;
    Ok(v)
}

// ---- settings ------------------------------------------------------------------------------

pub fn rounding_of(it: &mut Interp, v: &Value) -> R<Rounding> {
    match v.as_str().and_then(Rounding::from_name) {
        Some(r) => Ok(r),
        None => Err(it.type_error(ROUND_ERR)),
    }
}

fn zero_or_one(it: &mut Interp, v: &Value, what: &str) -> R<bool> {
    match ssize(it, v)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(it.value_error(&format!("valid values for {} are 0 or 1", what))),
    }
}

fn assign_prec(it: &mut Interp, c: &Py<PyContext>, v: &Value) -> R<()> {
    let x = ssize(it, v)?;
    if !(1..=dec::MAX_PREC).contains(&x) {
        return Err(it.value_error("valid range for prec is [1, MAX_PREC]"));
    }
    c.with(it, |c| c.prec = x)
}

fn assign_emax(it: &mut Interp, c: &Py<PyContext>, v: &Value) -> R<()> {
    let x = ssize(it, v)?;
    if !(0..=dec::MAX_EMAX).contains(&x) {
        return Err(it.value_error("valid range for Emax is [0, MAX_EMAX]"));
    }
    c.with(it, |c| c.emax = x)
}

fn assign_emin(it: &mut Interp, c: &Py<PyContext>, v: &Value) -> R<()> {
    let x = ssize(it, v)?;
    if !(dec::MIN_EMIN..=0).contains(&x) {
        return Err(it.value_error("valid range for Emin is [MIN_EMIN, 0]"));
    }
    c.with(it, |c| c.emin = x)
}

fn assign_rounding(it: &mut Interp, c: &Py<PyContext>, v: &Value) -> R<()> {
    let r = rounding_of(it, v)?;
    c.with(it, |c| c.round = r)
}

fn assign_capitals(it: &mut Interp, c: &Py<PyContext>, v: &Value) -> R<()> {
    let x = zero_or_one(it, v, "capitals")?;
    c.with(it, |c| c.capitals = x)
}

fn assign_clamp(it: &mut Interp, c: &Py<PyContext>, v: &Value) -> R<()> {
    let x = zero_or_one(it, v, "clamp")?;
    c.with(it, |c| c.clamp = x)
}

fn assign_signals(it: &mut Interp, c: &Py<PyContext>, v: &Value, traps: bool, allow_list: bool) -> R<()> {
    let bits = signal_bits(it, v, allow_list)?;
    let cell = {
        let b = c.borrow(it)?;
        if traps {
            b.traps.clone()
        } else {
            b.flags.clone()
        }
    };
    cell.set(bits);
    Ok(())
}

/// Applies `Context(...)`-style settings in the order `prec, rounding, Emin, Emax, capitals,
/// clamp, flags, traps`; absent and `None` entries keep the current setting.
pub fn apply_settings(it: &mut Interp, c: &Py<PyContext>, vals: &[Option<Value>]) -> R<()> {
    for (i, v) in vals.iter().enumerate() {
        let Some(v) = v else { continue };
        if v.is_none() {
            continue;
        }
        match i {
            0 => assign_prec(it, c, v)?,
            1 => assign_rounding(it, c, v)?,
            2 => assign_emin(it, c, v)?,
            3 => assign_emax(it, c, v)?,
            4 => assign_capitals(it, c, v)?,
            5 => assign_clamp(it, c, v)?,
            6 => assign_signals(it, c, v, false, true)?,
            _ => assign_signals(it, c, v, true, true)?,
        }
    }
    Ok(())
}

// ---- operand helpers -----------------------------------------------------------------------

type Un = fn(&dec::Decimal, &dec::Context, &mut dec::Status) -> dec::Decimal;
type Bin = fn(&dec::Decimal, &dec::Decimal, &dec::Context, &mut dec::Status) -> dec::Decimal;

fn cx_un(it: &mut Interp, cx: &Py<PyContext>, a: &Value, f: Un) -> R<Value> {
    let x = convert_op_raise(it, a)?;
    let r = run(it, cx, |c, st| f(&x, c, st))?;
    Ok(wrap(it, r))
}

fn cx_bin(it: &mut Interp, cx: &Py<PyContext>, a: &Value, b: &Value, f: Bin) -> R<Value> {
    let x = convert_op_raise(it, a)?;
    let y = convert_op_raise(it, b)?;
    let r = run(it, cx, |c, st| f(&x, &y, c, st))?;
    Ok(wrap(it, r))
}

fn cx_pred(it: &mut Interp, a: &Value, f: impl FnOnce(&dec::Decimal) -> bool) -> R<bool> {
    let x = convert_op_raise(it, a)?;
    Ok(f(&x))
}

fn cx_ctx_pred(it: &mut Interp, cx: &Py<PyContext>, a: &Value, f: impl FnOnce(&dec::Decimal, &dec::Context) -> bool) -> R<bool> {
    let x = convert_op_raise(it, a)?;
    let c = cx.borrow(it)?.core();
    Ok(f(&x, &c))
}

#[lumen_bind::methods]
impl PyContext {
    #[constructor]
    fn new(cls: This<Value>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        let _ = (args, kw);
        let Value::Obj(cls) = cls.0 else { return Err(it.type_error("Context.__new__(X): X is not a type object")) };
        let tmpl = template(it, 0)?;
        let base = tmpl.borrow(it)?.duplicate(true);
        Ok(opaque_instance(&cls, base))
    }

    #[proto(init)]
    fn init(slf: This<Py<Self>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<()> {
        let v = parse_args(it, args, kw, &SETTING_NAMES, 0)?;
        apply_settings(it, &slf.0, &v)
    }

    #[getter]
    fn prec(&self) -> i64 {
        self.prec
    }

    #[setter]
    fn set_prec(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
        assign_prec(it, &slf.0, value)
    }

    #[getter(name = "Emax")]
    fn emax(&self) -> i64 {
        self.emax
    }

    #[setter(name = "Emax")]
    fn set_emax(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
        assign_emax(it, &slf.0, value)
    }

    #[getter(name = "Emin")]
    fn emin(&self) -> i64 {
        self.emin
    }

    #[setter(name = "Emin")]
    fn set_emin(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
        assign_emin(it, &slf.0, value)
    }

    #[getter]
    fn rounding(&self) -> &'static str {
        self.round.name()
    }

    #[setter]
    fn set_rounding(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
        assign_rounding(it, &slf.0, value)
    }

    #[getter]
    fn capitals(&self) -> i64 {
        self.capitals as i64
    }

    #[setter]
    fn set_capitals(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
        assign_capitals(it, &slf.0, value)
    }

    #[getter]
    fn clamp(&self) -> i64 {
        self.clamp as i64
    }

    #[setter]
    fn set_clamp(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
        assign_clamp(it, &slf.0, value)
    }

    #[getter]
    fn flags(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        signal_dict_of(it, &slf.0, false)
    }

    #[setter]
    fn set_flags(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
        assign_signals(it, &slf.0, value, false, false)
    }

    #[getter]
    fn traps(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        signal_dict_of(it, &slf.0, true)
    }

    #[setter]
    fn set_traps(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
        assign_signals(it, &slf.0, value, true, false)
    }

    #[proto(repr)]
    fn repr(&self) -> String {
        format!(
            "Context(prec={}, rounding={}, Emin={}, Emax={}, capitals={}, clamp={}, flags={}, traps={})",
            self.prec,
            self.round.name(),
            self.emin,
            self.emax,
            self.capitals as i32,
            self.clamp as i32,
            signal_names(self.flags.get()),
            signal_names(self.traps.get())
        )
    }

    #[proto(reduce)]
    fn reduce(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let (prec, round, emin, emax, caps, clamp, flags, traps) = {
            let b = slf.0.borrow(it)?;
            (b.prec, b.round, b.emin, b.emax, b.capitals, b.clamp, b.flags.get(), b.traps.get())
        };
        let flags = signals_as_list(it, flags);
        let traps = signals_as_list(it, traps);
        let cls = Value::Obj(it.type_of(slf.0.value()));
        let args = Value::tuple(vec![
            Value::Int(prec),
            Value::str(round.name()),
            Value::Int(emin),
            Value::Int(emax),
            Value::Int(caps as i64),
            Value::Int(clamp as i64),
            flags,
            traps,
        ]);
        Ok(Value::tuple(vec![cls, args]))
    }

    /// Reset all flags to False.
    #[method]
    fn clear_flags(&self) {
        self.flags.set(0);
    }

    /// Set all traps to False.
    #[method]
    fn clear_traps(&self) {
        self.traps.set(0);
    }

    /// Return a duplicate of the context.
    #[method]
    fn copy(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        Ok(copy_context(it, &slf.0, false)?.into_value())
    }

    #[proto(copy)]
    fn dunder_copy(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        Ok(copy_context(it, &slf.0, false)?.into_value())
    }

    /// Return Etiny (= Emin - prec + 1).
    #[method(name = "Etiny")]
    fn etiny(&self) -> i64 {
        self.emin - self.prec + 1
    }

    /// Return Etop (= Emax - prec + 1).
    #[method(name = "Etop")]
    fn etop(&self) -> i64 {
        self.emax - self.prec + 1
    }

    #[method(name = "_apply")]
    fn apply(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        let x = convert_op_raise(it, a)?;
        let r = run(it, &slf.0, |c, st| x.fit(c, st))?;
        Ok(wrap(it, r))
    }

    #[method(name = "_unsafe_setprec")]
    fn unsafe_setprec(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
        let x = ssize(it, value)?;
        slf.0.with(it, |c| c.prec = x)
    }

    #[method(name = "_unsafe_setemin")]
    fn unsafe_setemin(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
        let x = ssize(it, value)?;
        slf.0.with(it, |c| c.emin = x)
    }

    #[method(name = "_unsafe_setemax")]
    fn unsafe_setemax(slf: This<Py<Self>>, it: &mut Interp, value: &Value) -> R<()> {
        let x = ssize(it, value)?;
        slf.0.with(it, |c| c.emax = x)
    }

    /// Create a new Decimal instance from num, using self as the context.
    ///
    /// Unlike the Decimal constructor, this function observes the context limits.
    #[method]
    fn create_decimal(slf: This<Py<Self>>, it: &mut Interp, num: Passed<&Value>) -> R<Value> {
        let src = match num.0 {
            Some(v) => v.clone(),
            None => Value::str("0"),
        };
        let d = super::decimal::from_object(it, &src, &slf.0, false)?;
        let r = run(it, &slf.0, |c, st| d.fit(c, st))?;
        Ok(wrap(it, r))
    }

    /// Create a new Decimal instance from float f.
    ///
    /// Unlike the Decimal.from_float() class method, this function observes the context limits.
    #[method]
    fn create_decimal_from_float(slf: This<Py<Self>>, it: &mut Interp, f: &Value) -> R<Value> {
        let d = super::decimal::exact_from_number(it, f)?;
        let r = run(it, &slf.0, |c, st| d.fit(c, st))?;
        Ok(wrap(it, r))
    }

    /// Return the absolute value of the operand.
    #[method]
    fn abs(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, dec::Decimal::abs)
    }

    /// Return the sum of the two operands.
    #[method]
    fn add(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::add)
    }

    /// Return a new instance of the operand.
    #[method]
    fn canonical(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        let _ = slf;
        if Py::<PyDecimal>::from_value(it, a).is_none() {
            return Err(it.type_error("argument must be a Decimal"));
        }
        Ok(a.clone())
    }

    /// Compare the values of the two operands numerically.
    #[method]
    fn compare(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::compare)
    }

    /// Compare two operands numerically, signalling on any NaN.
    #[method]
    fn compare_signal(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::compare_signal)
    }

    /// Compare two operands using their abstract representation.
    #[method]
    fn compare_total(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, |x, y, _, _| x.compare_total(y))
    }

    /// Compare two operands using their abstract representation, ignoring sign.
    #[method]
    fn compare_total_mag(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, |x, y, _, _| x.compare_total_mag(y))
    }

    /// Return a copy of the operand with the sign set to 0.
    #[method]
    fn copy_abs(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        let _ = slf;
        let x = convert_op_raise(it, a)?;
        Ok(wrap(it, x.copy_abs()))
    }

    /// Return a copy of the Decimal.
    #[method]
    fn copy_decimal(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        let _ = slf;
        if Py::<PyDecimal>::from_value(it, a).is_some() {
            return Ok(a.clone());
        }
        let x = convert_op_raise(it, a)?;
        Ok(wrap(it, x))
    }

    /// Return a copy of the operand with the sign inverted.
    #[method]
    fn copy_negate(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        let _ = slf;
        let x = convert_op_raise(it, a)?;
        Ok(wrap(it, x.copy_negate()))
    }

    /// Copy the sign from the second operand to the first.
    #[method]
    fn copy_sign(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        let _ = slf;
        let x = convert_op_raise(it, a)?;
        let y = convert_op_raise(it, b)?;
        Ok(wrap(it, x.copy_sign(&y)))
    }

    /// Return the quotient of the two operands.
    #[method]
    fn divide(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::div)
    }

    /// Return the integer part of the quotient of the two operands.
    #[method]
    fn divide_int(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::divint)
    }

    /// Return (a // b, a % b).
    #[method]
    fn divmod(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        let x = convert_op_raise(it, a)?;
        let y = convert_op_raise(it, b)?;
        let (q, r) = run(it, &slf.0, |c, st| x.divmod(&y, c, st))?;
        let (q, r) = (wrap(it, q), wrap(it, r));
        Ok(Value::tuple(vec![q, r]))
    }

    /// Return e ** a.
    #[method]
    fn exp(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, dec::Decimal::exp)
    }

    /// Return a * b + c.
    #[method]
    fn fma(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value, c: &Value) -> R<Value> {
        let x = convert_op_raise(it, a)?;
        let y = convert_op_raise(it, b)?;
        let z = convert_op_raise(it, c)?;
        let r = run(it, &slf.0, |ctx, st| x.fma(&y, &z, ctx, st))?;
        Ok(wrap(it, r))
    }

    /// Return True if the operand is canonical.
    #[method]
    fn is_canonical(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<bool> {
        let _ = slf;
        if Py::<PyDecimal>::from_value(it, a).is_none() {
            return Err(it.type_error("argument must be a Decimal"));
        }
        Ok(true)
    }

    /// Return True if the operand is finite.
    #[method]
    fn is_finite(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<bool> {
        let _ = slf;
        cx_pred(it, a, |x| x.is_finite())
    }

    /// Return True if the operand is infinite.
    #[method]
    fn is_infinite(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<bool> {
        let _ = slf;
        cx_pred(it, a, |x| x.is_infinite())
    }

    /// Return True if the operand is a qNaN or sNaN.
    #[method]
    fn is_nan(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<bool> {
        let _ = slf;
        cx_pred(it, a, |x| x.is_nan())
    }

    /// Return True if the operand is a normal number.
    #[method]
    fn is_normal(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<bool> {
        cx_ctx_pred(it, &slf.0, a, |x, c| x.is_normal(c))
    }

    /// Return True if the operand is a quiet NaN.
    #[method]
    fn is_qnan(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<bool> {
        let _ = slf;
        cx_pred(it, a, |x| x.is_qnan())
    }

    /// Return True if the operand is negative.
    #[method]
    fn is_signed(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<bool> {
        let _ = slf;
        cx_pred(it, a, |x| x.is_negative())
    }

    /// Return True if the operand is a signaling NaN.
    #[method]
    fn is_snan(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<bool> {
        let _ = slf;
        cx_pred(it, a, |x| x.is_snan())
    }

    /// Return True if the operand is subnormal.
    #[method]
    fn is_subnormal(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<bool> {
        cx_ctx_pred(it, &slf.0, a, |x, c| x.is_subnormal(c))
    }

    /// Return True if the operand is a zero.
    #[method]
    fn is_zero(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<bool> {
        let _ = slf;
        cx_pred(it, a, |x| x.is_zero())
    }

    /// Return the natural (base e) logarithm of the operand.
    #[method]
    fn ln(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, dec::Decimal::ln)
    }

    /// Return the base 10 logarithm of the operand.
    #[method]
    fn log10(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, dec::Decimal::log10)
    }

    /// Return the exponent of the magnitude of the operand's MSD.
    #[method]
    fn logb(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, dec::Decimal::logb)
    }

    /// Digit-wise and of the two (logical) operands.
    #[method]
    fn logical_and(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::logical_and)
    }

    /// Invert all digits of the operand.
    #[method]
    fn logical_invert(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, dec::Decimal::logical_invert)
    }

    /// Digit-wise or of the two (logical) operands.
    #[method]
    fn logical_or(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::logical_or)
    }

    /// Digit-wise xor of the two (logical) operands.
    #[method]
    fn logical_xor(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::logical_xor)
    }

    /// Compare the values numerically and return the maximum.
    #[method]
    fn max(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::max)
    }

    /// Compare the values numerically with their sign ignored and return the maximum.
    #[method]
    fn max_mag(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::max_mag)
    }

    /// Compare the values numerically and return the minimum.
    #[method]
    fn min(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::min)
    }

    /// Compare the values numerically with their sign ignored and return the minimum.
    #[method]
    fn min_mag(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::min_mag)
    }

    /// Minus (prefix) operation: -a rounded to the context.
    #[method]
    fn minus(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, dec::Decimal::minus)
    }

    /// Return the product of the two operands.
    #[method]
    fn multiply(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::mul)
    }

    /// Return the largest representable number smaller than a.
    #[method]
    fn next_minus(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, dec::Decimal::next_minus)
    }

    /// Return the smallest representable number larger than a.
    #[method]
    fn next_plus(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, dec::Decimal::next_plus)
    }

    /// Return the number closest to a, in the direction towards b.
    #[method]
    fn next_toward(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::next_toward)
    }

    /// Reduce the operand to its simplest form.
    #[method]
    fn normalize(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, dec::Decimal::normalize)
    }

    /// Return an indication of the class of the operand.
    #[method]
    fn number_class(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<&'static str> {
        let x = convert_op_raise(it, a)?;
        let c = slf.0.borrow(it)?.core();
        Ok(dec::class_name(x.number_class(&c)))
    }

    /// Plus (prefix) operation: +a rounded to the context.
    #[method]
    fn plus(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, dec::Decimal::plus)
    }

    /// Compute a**b, or (a**b) % modulo when modulo is given.
    #[method]
    fn power(slf: This<Py<Self>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kw: KwArgs) -> R<Value> {
        let v = parse_args(it, args, kw, &["a", "b", "modulo"], 2)?;
        let x = convert_op_raise(it, v[0].as_ref().unwrap_or(&Value::None))?;
        let y = convert_op_raise(it, v[1].as_ref().unwrap_or(&Value::None))?;
        let r = match v[2].as_ref() {
            None | Some(Value::None) => run(it, &slf.0, |c, st| x.pow(&y, c, st))?,
            Some(m) => {
                let m = convert_op_raise(it, m)?;
                run(it, &slf.0, |c, st| x.pow_mod(&y, &m, c, st))?
            }
        };
        Ok(wrap(it, r))
    }

    /// Return a value equal to a (rounded), having the exponent of b.
    #[method]
    fn quantize(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, |x, y, c, st| x.quantize(y, c.round, c, st))
    }

    /// Return 10.
    #[method]
    fn radix(slf: This<Py<Self>>, it: &mut Interp) -> Value {
        let _ = slf;
        wrap(it, dec::Decimal::radix())
    }

    /// Return the remainder from integer division.
    #[method]
    fn remainder(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::rem)
    }

    /// Return x - y * n, where n is the integer nearest the exact value of x / y.
    #[method]
    fn remainder_near(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::rem_near)
    }

    /// Return a copy of a with the digits rotated by b places.
    #[method]
    fn rotate(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::rotate)
    }

    /// Return True if the two operands have the same exponent.
    #[method]
    fn same_quantum(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<bool> {
        let _ = slf;
        let x = convert_op_raise(it, a)?;
        let y = convert_op_raise(it, b)?;
        Ok(x.same_quantum(&y))
    }

    /// Return the first operand with the exponent adjusted by the second.
    #[method]
    fn scaleb(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::scaleb)
    }

    /// Return a copy of a with the digits shifted by b places.
    #[method]
    fn shift(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::shift)
    }

    /// Square root of a non-negative number to context precision.
    #[method]
    fn sqrt(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, dec::Decimal::sqrt)
    }

    /// Return the difference between the two operands.
    #[method]
    fn subtract(slf: This<Py<Self>>, it: &mut Interp, a: &Value, b: &Value) -> R<Value> {
        cx_bin(it, &slf.0, a, b, dec::Decimal::sub)
    }

    /// Convert a number to a string, using engineering notation if an exponent is needed.
    #[method]
    fn to_eng_string(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<String> {
        let x = convert_op_raise(it, a)?;
        let caps = slf.0.borrow(it)?.capitals;
        Ok(x.to_eng_string(caps))
    }

    /// Convert a number to a string using scientific notation.
    #[method]
    fn to_sci_string(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<String> {
        let x = convert_op_raise(it, a)?;
        let caps = slf.0.borrow(it)?.capitals;
        Ok(x.to_sci_string(caps))
    }

    /// Round to an integer.
    #[method]
    fn to_integral(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, |x, c, st| x.to_integral_value(c.round, c, st))
    }

    /// Round to an integer, signaling Inexact and Rounded as appropriate.
    #[method]
    fn to_integral_exact(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, |x, c, st| x.to_integral_exact(c.round, c, st))
    }

    /// Round to an integer.
    #[method]
    fn to_integral_value(slf: This<Py<Self>>, it: &mut Interp, a: &Value) -> R<Value> {
        cx_un(it, &slf.0, a, |x, c, st| x.to_integral_value(c.round, c, st))
    }
}

/// The dictionary view of the flags or traps of a context.
#[lumen_bind::class(name = "SignalDictMixin", module = "decimal")]
pub struct SignalDict {
    cell: Rc<Cell<u32>>,
    traps: bool,
}

impl SignalDict {
    fn as_dict(&self, it: &mut Interp) -> Value {
        let d = it.new_dict();
        let e = excs(it);
        for (i, cls) in e.signals.iter().enumerate() {
            let on = self.cell.get() & SIGNAL_FLAGS[i] != 0;
            let _ = it.dict_set(&d, Value::Obj(cls.clone()), Value::Bool(on));
        }
        Value::Obj(d)
    }
}

#[lumen_bind::methods]
impl SignalDict {
    #[proto(len)]
    fn len(&self) -> usize {
        SIGNAL_FLAGS.len()
    }

    #[proto(iter)]
    fn iter(&self, it: &mut Interp) -> R<Value> {
        let e = excs(it);
        let keys = Value::list(e.signals.iter().map(|c| Value::Obj(c.clone())).collect());
        it.get_iter(&keys)
    }

    #[proto(getitem)]
    fn getitem(&self, it: &mut Interp, key: &Value) -> R<bool> {
        let f = signal_flag(it, key)?;
        Ok(self.cell.get() & f != 0)
    }

    #[proto(setitem)]
    fn setitem(&self, it: &mut Interp, key: &Value, value: &Value) -> R<()> {
        let f = signal_flag(it, key)?;
        let on = it.truthy(value)?;
        let cur = self.cell.get();
        self.cell.set(if on { cur | f } else { cur & !f });
        Ok(())
    }

    #[proto(delitem)]
    fn delitem(&self, it: &mut Interp, key: &Value) -> R<()> {
        let _ = key;
        Err(it.value_error("signal keys cannot be deleted"))
    }

    #[proto(repr)]
    fn repr(&self) -> String {
        let items: Vec<String> = (0..SIGNAL_FLAGS.len())
            .map(|i| format!("<class 'decimal.{}'>:{}", SIGNAL_NAMES[i], if self.cell.get() & SIGNAL_FLAGS[i] != 0 { "True" } else { "False" }))
            .collect();
        format!("{{{}}}", items.join(", "))
    }

    /// Return a plain dict of the signals.
    #[method]
    fn copy(&self, it: &mut Interp) -> Value {
        self.as_dict(it)
    }

    #[proto(eq)]
    fn eq(&self, it: &mut Interp, other: &Value) -> R<Value> {
        self.compare(it, other, false)
    }

    #[proto(ne)]
    fn ne(&self, it: &mut Interp, other: &Value) -> R<Value> {
        self.compare(it, other, true)
    }
}

impl SignalDict {
    fn compare(&self, it: &mut Interp, other: &Value, negate: bool) -> R<Value> {
        if let Some(o) = Py::<SignalDict>::from_value(it, other) {
            let theirs = o.borrow(it)?.cell.get();
            return Ok(Value::Bool((theirs == self.cell.get()) != negate));
        }
        if dict_of(other).is_some() {
            let mine = self.as_dict(it);
            let equal = it.values_eq(&mine, other)?;
            return Ok(Value::Bool(equal != negate));
        }
        Ok(Value::NotImplemented)
    }
}

// The object `localcontext()` returns: installs its context on entry, restores the previous one
// on exit.
#[lumen_bind::class(name = "ContextManager", module = "decimal", hint(py(final)))]
pub struct ContextManager {
    pub local: Py<PyContext>,
    pub global: Option<Py<PyContext>>,
}

#[lumen_bind::methods]
impl ContextManager {
    #[proto(enter)]
    fn enter(slf: This<Py<Self>>, it: &mut Interp) -> R<Value> {
        let previous = current(it)?;
        let local = slf.0.borrow(it)?.local.clone();
        set_current(it, &local)?;
        slf.0.with(it, |m| m.global = Some(previous))?;
        Ok(local.into_value())
    }

    #[proto(exit)]
    fn exit(slf: This<Py<Self>>, it: &mut Interp, #[varargs] args: &[Value]) -> R<Value> {
        let _ = args;
        let previous = slf.0.with(it, |m| m.global.take())?;
        if let Some(p) = previous {
            set_current(it, &p)?;
        }
        Ok(Value::None)
    }
}
