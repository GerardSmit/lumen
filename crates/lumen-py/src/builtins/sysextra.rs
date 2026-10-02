//! Struct sequences, `types.SimpleNamespace` and the frame objects handed out by
//! `sys._getframe` and tracebacks.

use super::native::*;
use crate::bytecode::Code;
use crate::object::*;
use crate::vm::*;
use std::rc::Rc;

// ---- struct sequences ---------------------------------------------------------------------------

const HIDDEN: &str = "_structseq_hidden";

fn field<const N: usize>(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let Some(v) = a.first() else { return Err(it.type_error("descriptor requires a struct sequence")) };
    match v.tuple_items() {
        Some(items) if N < items.len() => Ok(items[N].clone()),
        Some(items) => Ok(structseq_hidden(v).get(N - items.len()).cloned().unwrap_or(Value::None)),
        None => Err(it.type_error("descriptor requires a struct sequence")),
    }
}

/// The fields of a struct sequence past `n_sequence_fields` (reachable by name only).
pub fn structseq_hidden(v: &Value) -> Vec<Value> {
    let Value::Obj(o) = v else { return Vec::new() };
    let d = o.dict.borrow().clone();
    d.and_then(|d| dict_get_str(&d, HIDDEN)).and_then(|t| t.tuple_items().map(|t| t.to_vec())).unwrap_or_default()
}

const GETTERS: [NativeFn; 24] = [
    field::<0>,
    field::<1>,
    field::<2>,
    field::<3>,
    field::<4>,
    field::<5>,
    field::<6>,
    field::<7>,
    field::<8>,
    field::<9>,
    field::<10>,
    field::<11>,
    field::<12>,
    field::<13>,
    field::<14>,
    field::<15>,
    field::<16>,
    field::<17>,
    field::<18>,
    field::<19>,
    field::<20>,
    field::<21>,
    field::<22>,
    field::<23>,
];

fn structseq_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let ty = it.type_of(&a[0]);
    let names = it.lookup_mro(&ty, "_fields");
    let items = a[0].tuple_items().map(|t| t.to_vec()).unwrap_or_default();
    let mut parts = Vec::new();
    if let Some(Value::Obj(n)) = names {
        if let Kind::Tuple(names) = &n.kind {
            for (name, v) in names.iter().zip(items.iter()) {
                parts.push(format!("{}={}", name.as_str().unwrap_or("?"), it.repr_of(v)?));
            }
        }
    }
    Ok(Value::string(format!("{}({})", it.type_display(&ty), parts.join(", "))))
}

/// A tuple subclass with named read-only fields, like CPython's `sys.version_info`.
pub fn new_structseq_type(it: &mut Interp, module: &str, name: &str, fields: &[&'static str]) -> Obj {
    let tuple = it.types.tuple.clone();
    let ty = new_type(it, module, name, Some(&tuple), Layout::Tuple);
    for (i, f) in fields.iter().enumerate() {
        it.reg_prop(&ty, f, GETTERS[i]);
    }
    it.reg(&ty, "__repr__", structseq_repr);
    if let Some(d) = ty.dict.borrow().as_ref() {
        dict_set_str(d, "_fields", Value::tuple(fields.iter().map(|f| Value::str(f)).collect()));
        dict_set_str(d, "n_fields", Value::Int(fields.len() as i64));
        dict_set_str(d, "n_sequence_fields", Value::Int(fields.len() as i64));
    }
    ty
}

pub fn structseq(ty: &Obj, vals: Vec<Value>) -> Value {
    Value::Obj(Object::with_cls(ty.clone(), Kind::Tuple(vals)))
}

fn type_int(ty: &Obj, name: &str) -> usize {
    ty.dict.borrow().as_ref().and_then(|d| dict_get_str(d, name)).and_then(|v| v.as_i64()).unwrap_or(0) as usize
}

fn hidden_names(ty: &Obj) -> Vec<Value> {
    let all = ty.dict.borrow().as_ref().and_then(|d| dict_get_str(d, "_structseq_fields"));
    let all = all.and_then(|t| t.tuple_items().map(|t| t.to_vec())).unwrap_or_default();
    all.get(type_int(ty, "n_sequence_fields")..).map(|t| t.to_vec()).unwrap_or_default()
}

/// A struct sequence type like CPython's `os.stat_result`: `fields` lists all `n_fields` names in
/// order (`""` for an unnamed field); the first `n_seq` are the tuple items, the rest are reachable
/// by name only. Instances can be built from Python as `T(sequence, dict=None)` and pickle.
pub fn new_structseq_type_ext(it: &mut Interp, module: &str, name: &str, fields: &[&'static str], n_seq: usize) -> Obj {
    let tuple = it.types.tuple.clone();
    let ty = new_type(it, module, name, Some(&tuple), Layout::Tuple);
    for (i, f) in fields.iter().enumerate() {
        if !f.is_empty() {
            it.reg_prop(&ty, f, GETTERS[i]);
        }
    }
    it.reg(&ty, "__repr__", structseq_repr);
    it.reg_new(&ty, structseq_new);
    it.reg(&ty, "__reduce__", structseq_reduce);
    let named: Vec<Value> = fields.iter().filter(|f| !f.is_empty()).map(|f| Value::str(f)).collect();
    let match_args: Vec<Value> = fields[..n_seq].iter().filter(|f| !f.is_empty()).map(|f| Value::str(f)).collect();
    if let Some(d) = ty.dict.borrow().as_ref() {
        dict_set_str(d, "_fields", Value::tuple(named));
        dict_set_str(d, "__match_args__", Value::tuple(match_args));
        dict_set_str(d, "_structseq_fields", Value::tuple(fields.iter().map(|f| Value::str(f)).collect()));
        dict_set_str(d, "n_fields", Value::Int(fields.len() as i64));
        dict_set_str(d, "n_sequence_fields", Value::Int(n_seq as i64));
        dict_set_str(d, "n_unnamed_fields", Value::Int(fields.iter().filter(|f| f.is_empty()).count() as i64));
    }
    ty
}

/// The interpreter's single [`new_structseq_type_ext`] type identified by the marker type `K`,
/// created on first use, so natives can build instances without a module lookup.
pub fn structseq_type<K: 'static>(it: &mut Interp, module: &str, name: &str, fields: &[&'static str], n_seq: usize) -> Obj {
    let key = std::any::TypeId::of::<K>();
    if let Some(t) = it.native_types.get(&key) {
        return t.clone();
    }
    let t = new_structseq_type_ext(it, module, name, fields, n_seq);
    it.native_types.insert(key, t.clone());
    t
}

/// An instance of a [`new_structseq_type_ext`] type from all of its field values.
pub fn structseq_full(ty: &Obj, mut vals: Vec<Value>) -> Value {
    let n_seq = type_int(ty, "n_sequence_fields").min(vals.len());
    let hidden = vals.split_off(n_seq);
    let v = structseq(ty, vals);
    if !hidden.is_empty() {
        set_structseq_hidden(&v, hidden);
    }
    v
}

pub fn set_structseq_hidden(v: &Value, hidden: Vec<Value>) {
    if let Value::Obj(o) = v {
        let mut slot = o.dict.borrow_mut();
        let d = slot.get_or_insert_with(|| Object::new(Kind::Dict(std::cell::RefCell::new(crate::dict::PyDict::new())))).clone();
        drop(slot);
        dict_set_str(&d, HIDDEN, Value::tuple(hidden));
    }
}

/// `T(sequence, dict=None)` for a struct sequence type.
pub fn structseq_new(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    let b = it.bind_args("structseq", a, kw, &["cls", "sequence", "dict"], 2)?;
    let Some(Value::Obj(ty)) = b[0].clone() else { return Err(it.type_error("structseq.__new__(X): X is not a type object")) };
    let seq = it.iterate_to_vec(b[1].as_ref().unwrap_or(&Value::None))?;
    let (min, max) = (type_int(&ty, "n_sequence_fields"), type_int(&ty, "n_fields"));
    let tname = it.type_display(&ty);
    if seq.len() < min || seq.len() > max {
        let msg = if min == max {
            format!("{}() takes a {}-sequence ({}-sequence given)", tname, min, seq.len())
        } else if seq.len() < min {
            format!("{}() takes an at least {}-sequence ({}-sequence given)", tname, min, seq.len())
        } else {
            format!("{}() takes an at most {}-sequence ({}-sequence given)", tname, max, seq.len())
        };
        return Err(it.type_error(&msg));
    }
    let dict = match &b[2] {
        None | Some(Value::None) => None,
        Some(d) if dict_of(d).is_some() => Some(d.clone()),
        Some(_) => return Err(it.type_error(&format!("{}() takes a dict as second arg, if any", tname))),
    };
    let mut vals = seq[..min].to_vec();
    for (i, name) in hidden_names(&ty).iter().enumerate() {
        let v = match seq.get(min + i) {
            Some(v) => v.clone(),
            None => match &dict {
                Some(Value::Obj(d)) => it.dict_get(d, name)?.unwrap_or(Value::None),
                _ => Value::None,
            },
        };
        vals.push(v);
    }
    Ok(structseq_full(&ty, vals))
}

fn structseq_reduce(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__reduce__", a, 1, 1)?;
    let ty = it.type_of(&a[0]);
    let visible = a[0].tuple_items().map(|t| t.to_vec()).unwrap_or_default();
    let d = it.new_dict();
    for (name, v) in hidden_names(&ty).into_iter().zip(structseq_hidden(&a[0])) {
        if name.as_str().is_some_and(|n| !n.is_empty()) {
            it.dict_set(&d, name, v)?;
        }
    }
    Ok(Value::tuple(vec![Value::Obj(ty), Value::tuple(vec![Value::tuple(visible), Value::Obj(d)])]))
}

// ---- SimpleNamespace ----------------------------------------------------------------------------

fn ns_init(it: &mut Interp, a: &[Value], kw: Kw) -> R<Value> {
    if a.len() > 1 {
        return Err(it.type_error("no positional arguments expected"));
    }
    if let Value::Obj(o) = &a[0] {
        let d = it.instance_dict(o);
        for (k, v) in kw {
            it.dict_set(&d, Value::Obj(k.clone()), v.clone())?;
        }
    }
    Ok(Value::None)
}

fn ns_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let Value::Obj(o) = &a[0] else { return Ok(Value::str("namespace()")) };
    if it.repr_enter(o) {
        return Ok(Value::str("namespace(...)"));
    }
    let items: Vec<(Value, Value)> = match o.dict.borrow().as_ref().map(|d| &d.kind) {
        Some(Kind::Dict(d)) => d.borrow().iter().map(|e| (e.key.clone(), e.val.clone())).collect(),
        _ => Vec::new(),
    };
    let mut parts = Vec::new();
    let mut err = None;
    for (k, v) in items {
        match it.repr_of(&v) {
            Ok(r) => parts.push(format!("{}={}", k.as_str().unwrap_or("?"), r)),
            Err(e) => {
                err = Some(e);
                break;
            }
        }
    }
    it.repr_leave();
    if let Some(e) = err {
        return Err(e);
    }
    let ty = it.type_of(&a[0]);
    let name = if Rc::ptr_eq(&ty, &it.simple_namespace_type().unwrap_or_else(|| ty.clone())) { "namespace".to_string() } else { it.type_name(&ty) };
    Ok(Value::string(format!("{}({})", name, parts.join(", "))))
}

fn ns_eq(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    it.check_args("__eq__", a, 2, 2)?;
    let (Value::Obj(x), Value::Obj(y)) = (&a[0], &a[1]) else { return Ok(Value::NotImplemented) };
    let nt = it.simple_namespace_type();
    let is_ns = |it: &Interp, o: &Obj| nt.as_ref().is_some_and(|t| it.is_subtype(&it.type_of_obj(o), t));
    if !is_ns(it, x) || !is_ns(it, y) {
        return Ok(Value::NotImplemented);
    }
    let (dx, dy) = (Value::Obj(it.instance_dict(x)), Value::Obj(it.instance_dict(y)));
    let r = it.compare_op(crate::ast::CmpOp::Eq, &dx, &dy)?;
    Ok(r)
}

fn ns_reduce(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let ty = it.type_of(&a[0]);
    let d = match &a[0] {
        Value::Obj(o) => Value::Obj(it.instance_dict(o)),
        _ => Value::None,
    };
    Ok(Value::tuple(vec![Value::Obj(ty), Value::tuple(Vec::new()), d]))
}

impl Interp {
    pub fn simple_namespace_type(&self) -> Option<Obj> {
        self.simple_namespace.clone()
    }

    pub fn ensure_simple_namespace(&mut self) -> Obj {
        if let Some(t) = &self.simple_namespace {
            return t.clone();
        }
        let ty = new_type(self, "types", "SimpleNamespace", None, Layout::Object);
        self.reg(&ty, "__init__", ns_init);
        self.reg(&ty, "__repr__", ns_repr);
        self.reg(&ty, "__eq__", ns_eq);
        self.reg(&ty, "__reduce__", ns_reduce);
        if let Some(d) = ty.dict.borrow().as_ref() {
            dict_set_str(d, "__hash__", Value::None);
        }
        self.simple_namespace = Some(ty.clone());
        ty
    }

    pub fn new_namespace(&mut self, items: Vec<(&str, Value)>) -> Value {
        let ty = self.ensure_simple_namespace();
        let o = Object::with_cls(ty, Kind::Instance);
        let d = self.instance_dict(&o);
        for (k, v) in items {
            dict_set_str(&d, k, v);
        }
        Value::Obj(o)
    }
}

// ---- frames -------------------------------------------------------------------------------------

pub struct FrameObj {
    depth: Option<usize>,
    code: Rc<Code>,
    globals: Obj,
    line: u32,
    lasti: u32,
}

impl Interp {
    pub fn frame_object(&mut self, depth: usize) -> Value {
        let f = &self.frames[depth];
        let lasti = f.pc.saturating_sub(1);
        let data = FrameObj { depth: Some(depth), code: f.code.clone(), globals: f.globals.clone(), line: f.code.line_at(lasti), lasti: lasti as u32 };
        new_opaque(&self.types.frame.clone(), data)
    }

    pub fn dead_frame_object(&self, code: Rc<Code>, globals: Obj, line: u32, lasti: u32) -> Value {
        new_opaque(&self.types.frame, FrameObj { depth: None, code, globals, line, lasti })
    }
}

fn frame_data<X>(it: &mut Interp, v: &Value, f: impl FnOnce(&FrameObj) -> X) -> R<X> {
    match with_opaque::<FrameObj, _>(v, |d| f(d)) {
        Some(x) => Ok(x),
        None => Err(it.self_state_err("frame")),
    }
}

fn f_code(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let c = frame_data(it, &a[0], |d| d.code.clone())?;
    Ok(Value::Obj(Object::new(Kind::Code(c))))
}

fn f_globals(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Obj(frame_data(it, &a[0], |d| d.globals.clone())?))
}

fn f_builtins(it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::Obj(it.builtins.clone()))
}

fn f_lineno(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let (live, line) = frame_data(it, &a[0], |d| (d.depth.map(|x| (x, d.code.clone())), d.line))?;
    if let Some((depth, code)) = live {
        if let Some(f) = it.frames.get(depth) {
            if Rc::ptr_eq(&f.code, &code) {
                return Ok(Value::Int(f.code.line_at(f.pc.saturating_sub(1)) as i64));
            }
        }
    }
    Ok(Value::Int(line as i64))
}

fn f_lasti(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let (live, lasti) = frame_data(it, &a[0], |d| (d.depth.map(|x| (x, d.code.clone())), d.lasti))?;
    if let Some((depth, code)) = live {
        if let Some(f) = it.frames.get(depth) {
            if Rc::ptr_eq(&f.code, &code) {
                return Ok(Value::Int(2 * f.pc.saturating_sub(1) as i64));
            }
        }
    }
    Ok(Value::Int(2 * lasti as i64))
}

/// `frame.clear()`: an executing frame refuses; a finished one keeps no locals here to drop.
fn f_clear(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let live = frame_data(it, &a[0], |d| d.depth.map(|x| (x, d.code.clone())))?;
    if let Some((depth, code)) = live {
        if it.frames.get(depth).is_some_and(|f| Rc::ptr_eq(&f.code, &code)) {
            return Err(it.new_exc_str("RuntimeError", "cannot clear an executing frame"));
        }
    }
    Ok(Value::None)
}

fn f_none(_it: &mut Interp, _a: &[Value], _kw: Kw) -> R<Value> {
    Ok(Value::None)
}

fn f_back(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let (depth, code) = frame_data(it, &a[0], |d| (d.depth, d.code.clone()))?;
    if let Some(depth) = depth {
        let live = it.frames.get(depth).is_some_and(|f| Rc::ptr_eq(&f.code, &code));
        if live && depth > 0 {
            return Ok(it.frame_object(depth - 1));
        }
    }
    Ok(Value::None)
}

fn f_locals(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let (live, code, globals) = frame_data(it, &a[0], |d| (d.depth, d.code.clone(), d.globals.clone()))?;
    let depth = live.filter(|&d| it.frames.get(d).is_some_and(|f| Rc::ptr_eq(&f.code, &code)));
    let Some(depth) = depth else { return Ok(Value::Obj(it.new_dict())) };
    let f = &it.frames[depth];
    if let Some(names) = &f.names {
        return Ok(Value::Obj(names.clone()));
    }
    let _ = globals;
    let mut entries: Vec<(String, Value)> = Vec::new();
    for (i, n) in code.varnames.iter().enumerate() {
        if let Some(Some(v)) = f.locals.get(i) {
            entries.push((n.to_string(), v.clone()));
        }
    }
    let mut cell_names = code.cellvars.iter().chain(code.freevars.iter());
    for c in f.cells.iter() {
        let Some(n) = cell_names.next() else { break };
        if let Kind::Cell(cell) = &c.kind {
            if let Some(v) = cell.borrow().clone() {
                entries.push((n.to_string(), v));
            }
        }
    }
    let d = it.new_dict();
    for (k, v) in entries {
        dict_set_str(&d, &k, v);
    }
    Ok(Value::Obj(d))
}

fn f_repr(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let (code, line) = frame_data(it, &a[0], |d| (d.code.clone(), d.line))?;
    Ok(Value::string(format!("<frame at {:#x}, file '{}', line {}, code {}>", it.id_of(&a[0]), code.filename, line, code.name)))
}

// ---- code objects ---------------------------------------------------------------------------------

fn code_of(it: &mut Interp, v: &Value) -> R<Rc<Code>> {
    match v {
        Value::Obj(o) => match &o.kind {
            Kind::Code(c) => Ok(c.clone()),
            _ => Err(it.self_state_err("code")),
        },
        _ => Err(it.self_state_err("code")),
    }
}

/// `code.co_positions()`: (lineno, end_lineno, col, end_col) per instruction; columns are not
/// tracked, so they are None (tracebacks then print no carets).
fn co_positions(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let code = code_of(it, &a[0])?;
    let items = (0..code.ops.len())
        .map(|i| {
            let line = Value::Int(code.line_at(i) as i64);
            Value::tuple(vec![line.clone(), line, Value::None, Value::None])
        })
        .collect();
    it.get_iter(&Value::list(items))
}

/// `code.co_lines()`: (start, end, lineno) byte ranges of consecutive instructions on one line.
fn co_lines(it: &mut Interp, a: &[Value], _kw: Kw) -> R<Value> {
    let code = code_of(it, &a[0])?;
    let mut items = Vec::new();
    let mut start = 0;
    for i in 1..=code.ops.len() {
        if i == code.ops.len() || code.line_at(i) != code.line_at(start) {
            let line = Value::Int(code.line_at(start) as i64);
            items.push(Value::tuple(vec![Value::Int(2 * start as i64), Value::Int(2 * i as i64), line]));
            start = i;
        }
    }
    it.get_iter(&Value::list(items))
}

pub fn init_code_type(it: &mut Interp) {
    let ty = it.types.code.clone();
    it.reg(&ty, "co_positions", co_positions);
    it.reg(&ty, "co_lines", co_lines);
}

pub fn init_frame_type(it: &mut Interp) {
    let ty = it.types.frame.clone();
    it.reg_prop(&ty, "f_code", f_code);
    it.reg_prop(&ty, "f_globals", f_globals);
    it.reg_prop(&ty, "f_builtins", f_builtins);
    it.reg_prop(&ty, "f_lineno", f_lineno);
    it.reg_prop(&ty, "f_lasti", f_lasti);
    it.reg_prop(&ty, "f_back", f_back);
    it.reg_prop(&ty, "f_locals", f_locals);
    it.reg_prop(&ty, "f_trace", f_none);
    it.reg(&ty, "__repr__", f_repr);
    it.reg(&ty, "clear", f_clear);
    if let Kind::Type(td) = &ty.kind {
        td.flags.set(td.flags.get() | TF_DISPATCH);
    }
}
