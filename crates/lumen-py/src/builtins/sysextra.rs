//! Struct sequences, `types.SimpleNamespace` and the frame objects handed out by
//! `sys._getframe` and tracebacks.

use super::native::*;
use crate::bind::{Inst, KwArgs, PyCx, PyHost, This};
use crate::bytecode::Code;
use crate::object::*;
use crate::vm::*;
use lumen_bind::{FromArg, Slot};
use std::rc::Rc;

// ---- struct sequences ---------------------------------------------------------------------------

const HIDDEN: &str = "_structseq_hidden";

/// The fields of a struct sequence past `n_sequence_fields` (reachable by name only).
pub fn structseq_hidden(v: &Value) -> Vec<Value> {
    let Value::Obj(o) = v else { return Vec::new() };
    let d = o.dict.borrow().clone();
    d.and_then(|d| dict_get_str(&d, HIDDEN)).and_then(|t| t.tuple_items().map(|t| t.to_vec())).unwrap_or_default()
}

fn field(it: &mut Interp, seq: &Value, index: usize) -> R<Value> {
    match seq.tuple_items() {
        Some(items) if index < items.len() => Ok(items[index].clone()),
        Some(items) => Ok(structseq_hidden(seq).get(index - items.len()).cloned().unwrap_or(Value::None)),
        None => Err(it.type_error("descriptor requires a struct sequence")),
    }
}

/// The getters of the struct sequence fields by index, installed under the field names.
#[lumen_bind::class(name = "structseq_fields", hint(py(shared)))]
struct Fields;

#[lumen_bind::methods]
impl Fields {
    #[getter(name = "0")]
    fn f0(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 0)
    }
    #[getter(name = "1")]
    fn f1(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 1)
    }
    #[getter(name = "2")]
    fn f2(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 2)
    }
    #[getter(name = "3")]
    fn f3(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 3)
    }
    #[getter(name = "4")]
    fn f4(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 4)
    }
    #[getter(name = "5")]
    fn f5(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 5)
    }
    #[getter(name = "6")]
    fn f6(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 6)
    }
    #[getter(name = "7")]
    fn f7(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 7)
    }
    #[getter(name = "8")]
    fn f8(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 8)
    }
    #[getter(name = "9")]
    fn f9(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 9)
    }
    #[getter(name = "10")]
    fn f10(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 10)
    }
    #[getter(name = "11")]
    fn f11(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 11)
    }
    #[getter(name = "12")]
    fn f12(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 12)
    }
    #[getter(name = "13")]
    fn f13(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 13)
    }
    #[getter(name = "14")]
    fn f14(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 14)
    }
    #[getter(name = "15")]
    fn f15(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 15)
    }
    #[getter(name = "16")]
    fn f16(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 16)
    }
    #[getter(name = "17")]
    fn f17(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 17)
    }
    #[getter(name = "18")]
    fn f18(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 18)
    }
    #[getter(name = "19")]
    fn f19(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 19)
    }
    #[getter(name = "20")]
    fn f20(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 20)
    }
    #[getter(name = "21")]
    fn f21(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 21)
    }
    #[getter(name = "22")]
    fn f22(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 22)
    }
    #[getter(name = "23")]
    fn f23(slf: This<&Value>, it: &mut Interp) -> R<Value> {
        field(it, &slf, 23)
    }
}

/// Installs field `index` of the struct sequence type `ty` as a `member_descriptor` named `name`.
fn install_field(it: &mut Interp, ty: &Obj, name: &'static str, index: usize) {
    let mut items = Vec::new();
    <Fields as lumen_bind::Methods<PyHost>>::members(&mut items);
    let Some(item) = items.get(index) else { return };
    let fget = crate::bind::native_value(item);
    super::descr::install_member(it, ty, name, fget);
}

/// An instance of a struct sequence type (a tuple subclass instance).
#[derive(Clone, Copy)]
pub struct SeqRef<'a>(pub &'a Value, pub &'a [Value]);

impl<'a> FromArg<'a, PyHost> for SeqRef<'a> {
    #[inline]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match (v, v.tuple_items()) {
            (Value::Obj(o), Some(items)) if o.cls.is_some() => Ok(SeqRef(v, items)),
            _ => Err(cx.arg_error(at, "structseq", v)),
        }
    }
}

/// The members of the struct sequence types, installed into each one.
#[lumen_bind::class(name = "structseq", hint(py(shared)))]
pub struct StructSeq;

#[lumen_bind::methods]
impl StructSeq {
    // `T(sequence, dict=None)`.
    #[constructor]
    fn new(cls: This<Value>, it: &mut Interp, #[kw] sequence: &Value, #[kw] dict: Option<&Value>) -> R<Value> {
        let Value::Obj(ty) = &*cls else { unreachable!("checked by the entry") };
        let seq = it.iterate_to_vec(sequence)?;
        let (min, max) = (type_int(ty, "n_sequence_fields"), type_int(ty, "n_fields"));
        let tname = it.type_display(ty);
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
        let dict = match dict {
            None => None,
            Some(Value::Obj(d)) if dict_of(&Value::Obj(d.clone())).is_some() => Some(d.clone()),
            Some(_) => return Err(it.type_error(&format!("{}() takes a dict as second arg, if any", tname))),
        };
        let mut vals = seq[..min].to_vec();
        for (i, name) in hidden_names(ty).iter().enumerate() {
            let v = match seq.get(min + i) {
                Some(v) => v.clone(),
                None => match &dict {
                    Some(d) => it.dict_get(d, name)?.unwrap_or(Value::None),
                    None => Value::None,
                },
            };
            vals.push(v);
        }
        Ok(structseq_full(ty, vals))
    }

    #[proto(repr)]
    fn repr(slf: This<SeqRef<'_>>, it: &mut Interp) -> R<String> {
        let ty = it.type_of(slf.0 .0);
        let names = it.lookup_mro(&ty, "_fields");
        let mut parts = Vec::new();
        if let Some(Value::Obj(n)) = names {
            if let Kind::Tuple(names) = &n.kind {
                for (name, v) in names.iter().zip(slf.0 .1.iter()) {
                    parts.push(format!("{}={}", name.as_str().unwrap_or("?"), it.repr_of(v)?));
                }
            }
        }
        Ok(format!("{}({})", it.type_display(&ty), parts.join(", ")))
    }

    #[method(name = "__reduce__", hint(py(text_signature = "")))]
    fn reduce(slf: This<SeqRef<'_>>, it: &mut Interp) -> R<Value> {
        let SeqRef(v, visible) = slf.0;
        let ty = it.type_of(v);
        let d = it.new_dict();
        for (name, x) in hidden_names(&ty).into_iter().zip(structseq_hidden(v)) {
            if name.as_str().is_some_and(|n| !n.is_empty()) {
                it.dict_set(&d, name, x)?;
            }
        }
        Ok(Value::tuple(vec![Value::Obj(ty), Value::tuple(vec![Value::tuple(visible.to_vec()), Value::Obj(d)])]))
    }
}

/// A tuple subclass with named read-only fields, like CPython's `sys.version_info`.
pub fn new_structseq_type(it: &mut Interp, module: &str, name: &str, fields: &[&'static str]) -> Obj {
    let tuple = it.types.tuple.clone();
    let ty = new_type(it, module, name, Some(&tuple), Layout::Tuple);
    for (i, f) in fields.iter().enumerate() {
        install_field(it, &ty, f, i);
    }
    crate::bind::install_into::<StructSeq>(&ty, &["__repr__"]);
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
            install_field(it, &ty, f, i);
        }
    }
    crate::bind::install_into::<StructSeq>(&ty, &["__new__", "__repr__", "__reduce__"]);
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

// ---- SimpleNamespace ----------------------------------------------------------------------------

#[lumen_bind::class(name = "SimpleNamespace", module = "types")]
pub struct SimpleNamespace;

type Ns<'a> = Inst<'a, SimpleNamespace>;

#[lumen_bind::methods]
impl SimpleNamespace {
    #[proto(init)]
    fn init(slf: This<Ns<'_>>, it: &mut Interp, #[varargs] args: &[Value], #[varkw] kwargs: KwArgs) -> R<()> {
        if !args.is_empty() {
            return Err(it.type_error("no positional arguments expected"));
        }
        let d = it.instance_dict(slf.0 .0);
        for (k, v) in kwargs.iter() {
            dict_set_str(&d, k, v.clone());
        }
        Ok(())
    }

    #[proto(repr)]
    fn repr(slf: This<Ns<'_>>, it: &mut Interp) -> R<String> {
        let o = slf.0 .0;
        if it.repr_enter(o) {
            return Ok("namespace(...)".into());
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
        let ty = it.type_of_obj(o);
        let exact = it.simple_namespace_type().is_some_and(|t| Rc::ptr_eq(&t, &ty));
        let name = if exact { "namespace".to_string() } else { it.type_name(&ty) };
        Ok(format!("{}({})", name, parts.join(", ")))
    }

    #[proto(eq)]
    fn eq(slf: This<Ns<'_>>, it: &mut Interp, value: &Value) -> R<Value> {
        let Value::Obj(y) = value else { return Ok(Value::NotImplemented) };
        let nt = it.simple_namespace_type();
        if !nt.is_some_and(|t| it.is_subtype(&it.type_of_obj(y), &t)) {
            return Ok(Value::NotImplemented);
        }
        let (dx, dy) = (Value::Obj(it.instance_dict(slf.0 .0)), Value::Obj(it.instance_dict(y)));
        it.compare_op(crate::ast::CmpOp::Eq, &dx, &dy)
    }

    /// Return state information for pickling
    #[method(name = "__reduce__", hint(py(text_signature = "")))]
    fn reduce(slf: This<Ns<'_>>, it: &mut Interp) -> Value {
        let o = slf.0 .0;
        let ty = it.type_of_obj(o);
        let d = Value::Obj(it.instance_dict(o));
        Value::tuple(vec![Value::Obj(ty), Value::tuple(Vec::new()), d])
    }
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
        crate::bind::extend_type::<SimpleNamespace>(self, &ty);
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

/// A frame object. While its frame runs it finds the frame on the interpreter stack by `serial`
/// (or inside its suspended generator); a frame that finished moves into `dead`, so `f_locals`,
/// `f_trace` and tracebacks keep working. Frames only known from a traceback entry keep the
/// snapshot fields alone.
#[lumen_bind::class(name = "frame")]
pub struct FrameObj {
    serial: u64,
    gen: Option<std::rc::Weak<Object>>,
    dead: Option<Box<Frame>>,
    back: Option<Value>,
    code: Rc<Code>,
    globals: Obj,
    line: u32,
    lasti: u32,
    trace: Option<Value>,
    trace_lines: bool,
    trace_opcodes: bool,
}

fn frame_line(fr: &Frame) -> i64 {
    if fr.pc == 0 {
        fr.code.first_line as i64
    } else {
        fr.code.line_at(fr.pc - 1) as i64
    }
}

fn frame_lasti(fr: &Frame) -> i64 {
    if fr.pc == 0 {
        -1
    } else {
        2 * (fr.pc as i64 - 1)
    }
}

/// The generator or coroutine whose body is the executing frame `v`, else `None`.
pub fn frame_generator(it: &Interp, v: &Value) -> Option<Value> {
    let live = crate::builtins::native::with_opaque::<FrameObj, _>(v, |f| f.live_depth(it))?;
    let owner = it.frames.get(live?)?.generator.as_ref()?.upgrade()?;
    Some(Value::Obj(owner))
}

impl FrameObj {
    fn of(f: &Frame) -> FrameObj {
        FrameObj {
            serial: f.serial,
            gen: f.generator.clone(),
            dead: None,
            back: None,
            code: f.code.clone(),
            globals: f.globals.clone(),
            line: frame_line(f) as u32,
            lasti: f.pc.saturating_sub(1) as u32,
            trace: f.trace.clone(),
            trace_lines: f.trace_lines,
            trace_opcodes: f.trace_opcodes,
        }
    }

    /// Reads the frame this object stands for, wherever it currently lives.
    fn peek<X>(&self, it: &Interp, f: impl FnOnce(&Frame) -> X) -> Option<X> {
        if let Some(fr) = it.frames.iter().rev().find(|fr| fr.serial == self.serial) {
            return Some(f(fr));
        }
        if let Some(g) = self.gen.as_ref().and_then(|g| g.upgrade()) {
            if let Kind::Generator(gd) = &g.kind {
                if let Ok(st) = gd.state.try_borrow() {
                    if let GenState::Created(fr) | GenState::Suspended(fr) = &*st {
                        if fr.serial == self.serial {
                            return Some(f(&**fr));
                        }
                    }
                }
            }
        }
        self.dead.as_deref().map(f)
    }

    fn poke<X>(&mut self, it: &mut Interp, f: impl FnOnce(&mut Frame) -> X) -> Option<X> {
        let serial = self.serial;
        if let Some(fr) = it.frames.iter_mut().rev().find(|fr| fr.serial == serial) {
            return Some(f(fr));
        }
        if let Some(g) = self.gen.as_ref().and_then(|g| g.upgrade()) {
            if let Kind::Generator(gd) = &g.kind {
                if let Ok(mut st) = gd.state.try_borrow_mut() {
                    if let GenState::Created(fr) | GenState::Suspended(fr) = &mut *st {
                        if fr.serial == serial {
                            return Some(f(&mut **fr));
                        }
                    }
                }
            }
        }
        self.dead.as_deref_mut().map(f)
    }

    /// The position on the interpreter stack of the frame while it is running.
    fn running_at(&self, it: &Interp) -> Option<usize> {
        it.frames.iter().rposition(|fr| fr.serial == self.serial)
    }
}

impl Interp {
    /// The frame object of the frame at `depth` on the stack, the same one every time.
    pub fn frame_object(&mut self, depth: usize) -> Value {
        if let Some(o) = &self.frames[depth].fobj {
            return Value::Obj(o.clone());
        }
        let data = FrameObj::of(&self.frames[depth]);
        let v = new_opaque(&self.types.frame.clone(), data);
        if let Value::Obj(o) = &v {
            self.frames[depth].fobj = Some(o.clone());
        }
        v
    }

    /// `gi_frame` / `cr_frame` / `ag_frame` of a generator that is not finished.
    pub fn gen_frame_object(&mut self, g: &Obj) -> Value {
        let Kind::Generator(gd) = &g.kind else { return Value::None };
        if let Ok(mut st) = gd.state.try_borrow_mut() {
            if let GenState::Created(f) | GenState::Suspended(f) = &mut *st {
                if let Some(o) = &f.fobj {
                    return Value::Obj(o.clone());
                }
                f.generator = Some(Rc::downgrade(g));
                let v = new_opaque(&self.types.frame, FrameObj::of(f));
                if let Value::Obj(o) = &v {
                    f.fobj = Some(o.clone());
                }
                return v;
            }
        }
        let running = self.frames.iter().rposition(|f| f.generator.as_ref().and_then(|w| w.upgrade()).is_some_and(|x| Rc::ptr_eq(&x, g)));
        match running {
            Some(d) => self.frame_object(d),
            None => Value::None,
        }
    }

    /// The innermost of `frames` (a suspended thread's stack, outermost first) as a snapshot
    /// chained to its callers through `f_back`.
    pub fn frame_chain_snapshot(&self, frames: &[Frame]) -> Value {
        let mut back = None;
        for f in frames {
            let mut data = FrameObj::of(f);
            data.serial = 0;
            data.gen = None;
            data.back = back;
            back = Some(new_opaque(&self.types.frame, data));
        }
        back.unwrap_or(Value::None)
    }

    pub fn dead_frame_object(&self, code: Rc<Code>, globals: Obj, line: u32, lasti: u32) -> Value {
        let data = FrameObj { serial: 0, gen: None, dead: None, back: None, code, globals, line, lasti, trace: None, trace_lines: true, trace_opcodes: false };
        new_opaque(&self.types.frame, data)
    }
}

/// Moves a frame that left the stack into its frame object (`back` is the frame below it).
pub fn bury_frame(mut frame: Frame, back: Option<Value>) {
    let Some(fo) = frame.fobj.take() else { return };
    with_opaque::<FrameObj, _>(&Value::Obj(fo), move |d| {
        d.line = frame_line(&frame) as u32;
        d.lasti = frame.pc.saturating_sub(1) as u32;
        d.back = back;
        d.dead = Some(Box::new(frame));
    });
}

/// The code object a frame object runs.
pub fn frame_code(frame: &Value) -> Option<Rc<Code>> {
    with_opaque::<FrameObj, _>(frame, |d| d.code.clone())
}

enum Locals {
    Namespace(Obj),
    Entries(Vec<(String, Value)>),
}

fn collect_locals(f: &Frame) -> Locals {
    if let Some(names) = &f.names {
        return Locals::Namespace(names.clone());
    }
    let code = &f.code;
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
    Locals::Entries(entries)
}

#[lumen_bind::methods]
impl FrameObj {
    #[getter]
    fn f_code(&self) -> Value {
        Value::Obj(Code::object(&self.code))
    }

    #[getter]
    fn f_globals(&self) -> Value {
        Value::Obj(self.globals.clone())
    }

    #[getter]
    fn f_builtins(&self, it: &mut Interp) -> Value {
        Value::Obj(it.builtins.clone())
    }

    #[getter]
    fn f_lineno(&self, it: &mut Interp) -> i64 {
        self.peek(it, frame_line).unwrap_or(self.line as i64)
    }

    #[setter]
    fn set_f_lineno(&mut self, it: &mut Interp, value: &Value) -> R<()> {
        let Value::Int(line) = value else { return Err(it.value_error("lineno must be an integer")) };
        let at = self.running_at(it);
        crate::jump::set_lineno(it, at, *line)
    }

    #[getter]
    fn f_lasti(&self, it: &mut Interp) -> i64 {
        self.peek(it, frame_lasti).unwrap_or(2 * self.lasti as i64)
    }

    #[getter]
    fn f_back(&self, it: &mut Interp) -> Value {
        if let Some(d) = self.running_at(it) {
            return if d > 0 { it.frame_object(d - 1) } else { Value::None };
        }
        self.back.clone().unwrap_or(Value::None)
    }

    #[getter]
    fn f_locals(&self, it: &mut Interp) -> Value {
        match self.peek(it, collect_locals) {
            Some(Locals::Namespace(n)) => Value::Obj(n),
            Some(Locals::Entries(entries)) => {
                let d = it.new_dict();
                for (k, v) in entries {
                    dict_set_str(&d, &k, v);
                }
                Value::Obj(d)
            }
            None => Value::Obj(it.new_dict()),
        }
    }

    #[getter]
    fn f_trace(&self, it: &mut Interp) -> Value {
        self.peek(it, |fr| fr.trace.clone()).unwrap_or_else(|| self.trace.clone()).unwrap_or(Value::None)
    }

    #[setter]
    fn set_f_trace(&mut self, it: &mut Interp, value: &Value) {
        let v = if value.is_none() { None } else { Some(value.clone()) };
        let mine = v.clone();
        if self.poke(it, |fr| fr.trace = v).is_none() {
            self.trace = mine;
        }
    }

    #[getter]
    fn f_trace_lines(&self, it: &mut Interp) -> bool {
        self.peek(it, |fr| fr.trace_lines).unwrap_or(self.trace_lines)
    }

    #[setter]
    fn set_f_trace_lines(&mut self, it: &mut Interp, value: &Value) -> R<()> {
        let Value::Bool(b) = value else { return Err(it.type_error("attribute value type must be bool")) };
        let b = *b;
        if self.poke(it, |fr| fr.trace_lines = b).is_none() {
            self.trace_lines = b;
        }
        Ok(())
    }

    #[getter]
    fn f_trace_opcodes(&self, it: &mut Interp) -> bool {
        self.peek(it, |fr| fr.trace_opcodes).unwrap_or(self.trace_opcodes)
    }

    #[setter]
    fn set_f_trace_opcodes(&mut self, it: &mut Interp, value: &Value) -> R<()> {
        let Value::Bool(b) = value else { return Err(it.type_error("attribute value type must be bool")) };
        let b = *b;
        if self.poke(it, |fr| fr.trace_opcodes = b).is_none() {
            self.trace_opcodes = b;
        }
        if b {
            it.want_opcode_events();
        }
        Ok(())
    }

    /// F.clear(): clear most references held by the frame
    #[method(hint(py(text_signature = "")))]
    fn clear(&mut self, it: &mut Interp) -> R<()> {
        if self.running_at(it).is_some() {
            return Err(it.new_exc_str("RuntimeError", "cannot clear an executing frame"));
        }
        if let Some(g) = self.gen.as_ref().and_then(|g| g.upgrade()) {
            it.gen_close(&g)?;
        }
        self.poke(it, |fr| {
            fr.stack.clear();
            fr.blocks.clear();
            fr.locals.iter_mut().for_each(|l| *l = None);
            fr.trace = None;
        });
        Ok(())
    }

    #[proto(repr)]
    fn repr(slf: This<&Value>, it: &mut Interp) -> R<String> {
        let Some((code, line)) = with_opaque::<FrameObj, _>(&slf, |d| (d.code.clone(), d.peek(it, frame_line).unwrap_or(d.line as i64))) else {
            return Err(it.self_state_err("frame"));
        };
        Ok(format!("<frame at {:#x}, file '{}', line {}, code {}>", it.id_of(&slf), code.filename, line, code.name))
    }
}

// ---- code objects ---------------------------------------------------------------------------------

/// A code object: the receiver of the code type's methods.
pub struct CodeRef(pub Rc<Code>);

impl<'a> FromArg<'a, PyHost> for CodeRef {
    #[inline]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v {
            Value::Obj(o) => match &o.kind {
                Kind::Code(c) => Ok(CodeRef(c.clone())),
                _ => Err(cx.arg_error(at, "code", v)),
            },
            _ => Err(cx.arg_error(at, "code", v)),
        }
    }
}

#[lumen_bind::class(name = "code")]
pub struct CodeType;

#[lumen_bind::methods]
impl CodeType {
    /// `code.co_positions()`: (lineno, end_lineno, col, end_col) per instruction; columns are not
    /// tracked, so they are None (tracebacks then print no carets).
    #[method(hint(py(text_signature = "")))]
    fn co_positions(slf: This<CodeRef>, it: &mut Interp) -> R<Value> {
        let code = &slf.0 .0;
        let items = (0..code.ops.len())
            .map(|i| {
                let line = Value::Int(code.line_at(i) as i64);
                Value::tuple(vec![line.clone(), line, Value::None, Value::None])
            })
            .collect();
        it.get_iter(&Value::list(items))
    }

    /// `code.co_lines()`: (start, end, lineno) byte ranges of consecutive instructions on one line.
    #[method(hint(py(text_signature = "")))]
    fn co_lines(slf: This<CodeRef>, it: &mut Interp) -> R<Value> {
        let code = &slf.0 .0;
        let items = lumen_common::lineno::line_ranges(&code.lines)
            .into_iter()
            .map(|(start, end, line)| Value::tuple(vec![Value::Int(2 * start as i64), Value::Int(2 * end as i64), Value::Int(line as i64)]))
            .collect();
        it.get_iter(&Value::list(items))
    }
}

pub fn init_code_type(it: &mut Interp) {
    let ty = it.types.code.clone();
    crate::bind::extend_type::<CodeType>(it, &ty);
}

pub fn init_frame_type(it: &mut Interp) {
    let ty = it.types.frame.clone();
    crate::bind::extend_type::<FrameObj>(it, &ty);
    if let Kind::Type(td) = &ty.kind {
        td.flags.set(td.flags.get() | TF_DISPATCH);
    }
}
