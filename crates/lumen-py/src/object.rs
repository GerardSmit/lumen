//! Object model: values, heap objects and their payloads.

use crate::pyint::BigInt;
use crate::bytecode::Code;
use crate::dict::PyDict;
use crate::vm::{Frame, Interp};
pub use lumen_common::buffer::ByteStore;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub type Obj = Rc<Object>;
pub type R<T> = Result<T, Obj>;
pub type NativeFn = fn(&mut Interp, &[Value], &[(Obj, Value)]) -> R<Value>;

#[derive(Clone)]
pub enum Value {
    None,
    NotImplemented,
    Ellipsis,
    Bool(bool),
    Int(i64),
    Float(f64),
    Obj(Obj),
}

pub struct Object {
    /// `None` means the exact builtin type implied by `kind`.
    pub cls: Option<Obj>,
    pub dict: RefCell<Option<Obj>>,
    /// Lazily assigned identity number; recycled on drop so that short-lived temporaries reuse
    /// ids the way CPython reuses addresses.
    pub id: Cell<u32>,
    pub gc: GcCell,
    pub kind: Kind,
}

/// The cycle collector's per-object state: the slot in its generation list, the generation
/// (`gc::UNTRACKED` when not in any list), an allocation sequence number and flag bits.
pub struct GcCell {
    pub idx: Cell<u32>,
    pub seq: Cell<u32>,
    pub gen: Cell<u8>,
    pub flags: Cell<u8>,
}

/// `__del__` / generator finalization already ran (PEP 442: it runs once per object).
pub const GC_FINALIZED: u8 = 1;

impl GcCell {
    pub const fn new() -> GcCell {
        GcCell { idx: Cell::new(0), seq: Cell::new(0), gen: Cell::new(0), flags: Cell::new(0) }
    }
}

impl Default for GcCell {
    fn default() -> GcCell {
        GcCell::new()
    }
}

thread_local! {
    static FREE_IDS: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    static NEXT_ID: Cell<u32> = const { Cell::new(1) };
}

impl Object {
    pub fn identity(&self) -> u32 {
        let cur = self.id.get();
        if cur != 0 {
            return cur;
        }
        let recycled = FREE_IDS.try_with(|f| f.borrow_mut().pop()).ok().flatten();
        let id = recycled.unwrap_or_else(|| {
            NEXT_ID.with(|n| {
                let v = n.get();
                n.set(v + 1);
                v
            })
        });
        self.id.set(id);
        id
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        if self.gc.idx.get() != 0 {
            crate::gc::untrack(self);
        }
        if self.gc.flags.get() & GC_FINALIZED == 0 && crate::gc::defer_finalizer(self) {
            return;
        }
        let id = self.id.get();
        if id != 0 {
            crate::weak::on_object_drop(id);
            let _ = FREE_IDS.try_with(|f| f.borrow_mut().push(id));
        }
    }
}

pub fn hash_bytes(b: &[u8]) -> i64 {
    match lumen_common::fasthash::hash_bytes(b) as i64 {
        -1 => -2,
        h => h,
    }
}

pub fn hash_str(s: &str) -> i64 {
    hash_bytes(s.as_bytes())
}

pub struct PyStr {
    pub s: Box<str>,
    pub ascii: bool,
    pub nchars: usize,
    hash: Cell<i64>,
    hashed: Cell<bool>,
}

impl PyStr {
    pub fn new(s: &str) -> PyStr {
        PyStr::from_box(s.into())
    }

    pub fn from_box(s: Box<str>) -> PyStr {
        let ascii = s.is_ascii();
        let nchars = if ascii { s.len() } else { lumen_common::smuggle::count_code_points(&s) };
        PyStr { s, ascii, nchars, hash: Cell::new(0), hashed: Cell::new(false) }
    }

    pub fn hash(&self) -> i64 {
        if !self.hashed.get() {
            self.hash.set(hash_str(&self.s));
            self.hashed.set(true);
        }
        self.hash.get()
    }

    pub fn byte_offset(&self, char_idx: usize) -> usize {
        if self.ascii {
            char_idx
        } else {
            lumen_common::smuggle::code_point_offset(&self.s, char_idx)
        }
    }

    /// The code point at index `i`.
    pub fn char_at(&self, i: usize) -> Option<u32> {
        if self.ascii {
            self.s.as_bytes().get(i).map(|&b| b as u32)
        } else {
            lumen_common::smuggle::code_points(&self.s).nth(i)
        }
    }

    pub fn slice(&self, start: usize, end: usize) -> &str {
        if start >= end {
            return "";
        }
        let b = self.byte_offset(start);
        let e = self.byte_offset(end);
        &self.s[b..e]
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Layout {
    Object,
    Int,
    Float,
    Complex,
    Str,
    Tuple,
    List,
    Dict,
    Set,
    FrozenSet,
    Bytes,
    ByteArray,
    Exception,
    Type,
    Module,
    Function,
    Other,
}

pub const TF_HEAP: u32 = 1;
pub const TF_NO_INSTANCE_DICT: u32 = 2;
pub const TF_ABSTRACT: u32 = 4;
/// Special methods defined in the type's own dict are dispatched like those of a heap class.
pub const TF_DISPATCH: u32 = 8;
/// The type cannot be subclassed (no `Py_TPFLAGS_BASETYPE`).
pub const TF_FINAL: u32 = 16;
/// A core builtin type: setting or deleting its attributes is a TypeError.
pub const TF_IMMUTABLE: u32 = 32;

pub struct TypeData {
    pub name: RefCell<Rc<str>>,
    /// `__qualname__`, when it differs from `name` (taken out of the class namespace).
    pub qualname: RefCell<Option<Rc<str>>>,
    pub bases: RefCell<Vec<Obj>>,
    pub mro: RefCell<Vec<Obj>>,
    pub layout: Cell<Layout>,
    pub flags: Cell<u32>,
    pub hooks: Cell<(u64, u8)>,
    /// The (mangled) names a class statement's `__slots__` declares; `None` without `__slots__`.
    pub slots: RefCell<Option<Rc<[Rc<str>]>>>,
    /// Whether instances have a `__del__`, as of the type epoch in the first field.
    pub del_cache: Cell<(u64, bool)>,
}

pub struct Function {
    pub code: Rc<Code>,
    pub globals: Obj,
    pub defaults: RefCell<Vec<Value>>,
    pub kwdefaults: RefCell<Vec<(Obj, Value)>>,
    pub closure: Vec<Obj>,
    pub name: RefCell<Rc<str>>,
    pub qualname: RefCell<Rc<str>>,
    pub annotations: RefCell<Option<Obj>>,
    /// `__type_params__`; `None` means the empty tuple.
    pub type_params: RefCell<Option<Value>>,
}

pub struct NativeData {
    pub name: &'static str,
    pub f: NativeFn,
    pub method: bool,
    /// Set for natives bound with `lumen_bind` (`__text_signature__`, `__module__`, ...).
    pub desc: Option<&'static lumen_bind::FnDesc>,
    /// The module or class a hand-registered native belongs to (`desc` gives it for bound ones).
    pub owner: Option<NativeOwner>,
}

pub enum NativeOwner {
    Module(Rc<str>),
    Class(Obj),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum GenKind {
    Generator,
    Coroutine,
    AsyncGen,
}

pub enum GenState {
    Created(Box<Frame>),
    Suspended(Box<Frame>),
    Running,
    Done,
}

pub struct GenData {
    pub state: RefCell<GenState>,
    pub kind: GenKind,
    pub name: RefCell<Rc<str>>,
    pub qualname: RefCell<Rc<str>>,
    pub running_async: Cell<bool>,
    pub hooks_inited: Cell<bool>,
}

#[derive(Clone)]
pub struct TbEntry {
    pub file: Rc<str>,
    pub line: u32,
    /// Index of the instruction that raised (`tb_lasti` is twice this, as in CPython's
    /// 2-byte code units).
    pub lasti: u32,
    pub name: Rc<str>,
    pub code: Rc<Code>,
    pub globals: Obj,
}

pub struct ExcData {
    pub args: Value,
    pub cause: Option<Obj>,
    pub context: Option<Obj>,
    pub suppress_context: bool,
    pub ctx_set: bool,
    pub tb: Vec<TbEntry>,
}

pub struct PropData {
    pub fget: Value,
    pub fset: Value,
    pub fdel: Value,
    pub doc: Value,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ViewKind {
    Keys,
    Values,
    Items,
}

pub enum IterState {
    List { list: Obj, idx: usize },
    Tuple { tup: Obj, idx: usize },
    Str { s: Obj, pos: usize },
    Bytes { b: Obj, idx: usize },
    Range { cur: i64, stop: i64, step: i64 },
    Dict { dict: Obj, pos: usize, len: usize, kind: ViewKind },
    Set { set: Obj, pos: usize, len: usize },
    Seq { obj: Value, idx: i64 },
    CallIter { f: Value, sentinel: Value, done: bool },
    Reversed { seq: Value, idx: i64 },
    Enumerate { it: Value, idx: i64 },
    Zip { its: Vec<Value>, strict: bool },
    Map { f: Value, its: Vec<Value> },
    Filter { f: Value, it: Value },
    Native(Box<dyn FnMut(&mut Interp) -> R<Option<Value>>>),
    /// A `Native` iterator whose step is running (its closure is out of the cell).
    Running,
    Empty,
}

pub struct RangeData {
    pub start: i64,
    pub stop: i64,
    pub step: i64,
}

pub enum Kind {
    Instance,
    Str(PyStr),
    Int(BigInt),
    Float(f64),
    Complex(f64, f64),
    Tuple(Vec<Value>),
    List(RefCell<Vec<Value>>),
    Dict(RefCell<PyDict>),
    Set(RefCell<PyDict>),
    FrozenSet(RefCell<PyDict>),
    Bytes(Vec<u8>),
    /// A growable store, shared with the memoryviews that export it.
    ByteArray(Rc<ByteStore>),
    Type(TypeData),
    Function(Box<Function>),
    Method(Value, Value),
    Native(NativeData),
    Module,
    Cell(RefCell<Option<Value>>),
    Code(Rc<Code>),
    Generator(Box<GenData>),
    Exception(RefCell<ExcData>),
    Slice(Value, Value, Value),
    Range(RangeData),
    BigRange(Box<[BigInt; 3]>),
    Iter(RefCell<IterState>),
    Property(PropData),
    StaticMethod(Value),
    ClassMethod(Value),
    Super(Value, Value, Value),
    DictView(Obj, ViewKind),
    Frame,
    AsyncGenValue(Value),
    /// Native state owned by a builtin extension type (deque, partial, weakref, ...).
    Opaque(RefCell<Box<dyn std::any::Any>>),
}

impl Object {
    /// Allocates `o` and registers it with the cycle collector when it can be part of a cycle.
    #[inline]
    pub fn alloc(o: Object) -> Obj {
        let track = o.cls.is_some() || o.dict.borrow().is_some() || kind_tracked(&o.kind);
        let rc = Rc::new(o);
        if track {
            crate::gc::track(&rc);
        }
        rc
    }

    pub fn new(kind: Kind) -> Obj {
        Object::alloc(Object { cls: None, dict: RefCell::new(None), id: Cell::new(0), gc: GcCell::new(), kind })
    }

    pub fn with_cls(cls: Obj, kind: Kind) -> Obj {
        Object::alloc(Object { cls: Some(cls), dict: RefCell::new(None), id: Cell::new(0), gc: GcCell::new(), kind })
    }

    pub fn with_dict(kind: Kind, dict: Obj) -> Obj {
        Object::alloc(Object { cls: None, dict: RefCell::new(Some(dict)), id: Cell::new(0), gc: GcCell::new(), kind })
    }

    pub fn type_data(&self) -> Option<&TypeData> {
        match &self.kind {
            Kind::Type(t) => Some(t),
            _ => None,
        }
    }
}

/// Literal conversions (binding defaults: `#[default(0)] start: Value`).
impl From<i32> for Value {
    fn from(i: i32) -> Value {
        Value::Int(i as i64)
    }
}

impl From<i64> for Value {
    fn from(i: i64) -> Value {
        Value::Int(i)
    }
}

impl From<f64> for Value {
    fn from(x: f64) -> Value {
        Value::Float(x)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Value {
        Value::Bool(b)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Value {
        Value::str(s)
    }
}

impl Value {
    pub fn str(s: &str) -> Value {
        Value::Obj(Object::new(Kind::Str(PyStr::new(s))))
    }

    pub fn string(s: String) -> Value {
        Value::Obj(Object::new(Kind::Str(PyStr::from_box(s.into_boxed_str()))))
    }

    pub fn tuple(v: Vec<Value>) -> Value {
        Value::Obj(Object::new(Kind::Tuple(v)))
    }

    pub fn list(v: Vec<Value>) -> Value {
        Value::Obj(Object::new(Kind::List(RefCell::new(v))))
    }

    pub fn dict(d: PyDict) -> Value {
        Value::Obj(Object::new(Kind::Dict(RefCell::new(d))))
    }

    pub fn bytes(v: Vec<u8>) -> Value {
        Value::Obj(Object::new(Kind::Bytes(v)))
    }

    pub fn bytearray(v: Vec<u8>) -> Value {
        Value::Obj(Object::new(Kind::ByteArray(ba_store(v))))
    }

    pub fn big(b: BigInt) -> Value {
        match b.to_i64() {
            Some(i) => Value::Int(i),
            None => Value::Obj(Object::new(Kind::Int(b))),
        }
    }

    pub fn from_obj(o: Obj) -> Value {
        Value::Obj(o)
    }

    pub fn as_obj(&self) -> Option<&Obj> {
        match self {
            Value::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Obj(o) => match &o.kind {
                Kind::Str(s) => Some(&s.s),
                _ => None,
            },
            _ => None,
        }
    }

    /// The text of an exact `str` (not a subclass instance, whose `__str__` may differ).
    pub fn as_exact_str(&self) -> Option<&str> {
        match self {
            Value::Obj(o) if o.cls.is_none() => self.as_str(),
            _ => None,
        }
    }

    pub fn as_pystr(&self) -> Option<&PyStr> {
        match self {
            Value::Obj(o) => match &o.kind {
                Kind::Str(s) => Some(s),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn is_none(&self) -> bool {
        matches!(self, Value::None)
    }

    pub fn is(&self, o: &Value) -> bool {
        match (self, o) {
            (Value::None, Value::None) | (Value::NotImplemented, Value::NotImplemented) | (Value::Ellipsis, Value::Ellipsis) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a.to_bits() == b.to_bits(),
            (Value::Obj(a), Value::Obj(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }

    pub fn tuple_items(&self) -> Option<&[Value]> {
        match self {
            Value::Obj(o) => match &o.kind {
                Kind::Tuple(t) => Some(t),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn is_type(&self) -> bool {
        matches!(self, Value::Obj(o) if matches!(o.kind, Kind::Type(_)))
    }

    /// Integer payload for ints, bools and int subclasses.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    pub fn as_bigint(&self) -> Option<BigInt> {
        match self {
            Value::Int(i) => Some(BigInt::from_i64(*i)),
            Value::Bool(b) => Some(BigInt::from_i64(*b as i64)),
            Value::Obj(o) => match &o.kind {
                Kind::Int(b) => Some(b.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn is_int_like(&self) -> bool {
        match self {
            Value::Int(_) | Value::Bool(_) => true,
            Value::Obj(o) => matches!(o.kind, Kind::Int(_)),
            _ => false,
        }
    }
}

pub fn list_of(v: &Value) -> Option<&RefCell<Vec<Value>>> {
    match v {
        Value::Obj(o) => match &o.kind {
            Kind::List(l) => Some(l),
            _ => None,
        },
        _ => None,
    }
}

pub fn dict_of(v: &Value) -> Option<&RefCell<PyDict>> {
    match v {
        Value::Obj(o) => match &o.kind {
            Kind::Dict(d) => Some(d),
            _ => None,
        },
        _ => None,
    }
}

pub fn set_of(v: &Value) -> Option<&RefCell<PyDict>> {
    match v {
        Value::Obj(o) => match &o.kind {
            Kind::Set(d) | Kind::FrozenSet(d) => Some(d),
            _ => None,
        },
        _ => None,
    }
}

/// The store of a new `bytearray` holding `v`.
pub fn ba_store(v: Vec<u8>) -> Rc<ByteStore> {
    Rc::new(ByteStore::new(v).growable())
}

/// Whether objects of this kind can take part in a reference cycle (CPython's `PyObject_IS_GC`
/// types); tuples only when they hold something that can.
pub fn kind_tracked(k: &Kind) -> bool {
    match k {
        Kind::Str(_) | Kind::Int(_) | Kind::Float(_) | Kind::Complex(..) | Kind::Bytes(_) | Kind::ByteArray(_) | Kind::Code(_) | Kind::Range(_) | Kind::BigRange(_) | Kind::Frame => false,
        Kind::Tuple(items) => items.iter().any(|v| matches!(v, Value::Obj(_))),
        _ => true,
    }
}
