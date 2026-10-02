//! The Python host of Lumen's binding framework ([`lumen_bind`]): every `#[op]`, `#[class]` and
//! `#[module]` declared with `lumen_bind` runs in this interpreter through [`PyHost`]. The host
//! derives what CPython derives from a C signature: the name, the calling convention and its
//! exact argument errors, the keyword binder and `__text_signature__` ([`args`]).
//!
//! ```ignore
//! #[lumen_bind::module(name = "demo")]
//! pub mod demo {
//!     use lumen_py::bind::*;
//!
//!     /// `demo.isclose(a, b, /, *, rel_tol=1e-09)`
//!     #[op]
//!     pub fn isclose(a: f64, b: f64, #[kwonly] #[default(1e-09)] rel_tol: f64) -> bool { .. }
//! }
//! // lumen_py::bind::module_object::<demo::Module>(&mut interp)
//! ```
//!
//! Python-only parameter types: `Value` / `&Value` (any object), `This<Value>` / `This<Py<Self>>`
//! (the receiver, unborrowed), [`KwArgs`] (`#[varkw]`), `&mut Interp` (the interpreter). Results:
//! `Value`, `Obj`, [`Py<T>`], [`NativeIter`]; errors: `Obj` (an exception), `NativeError`,
//! `BufferError`.

pub mod args;
pub mod path;
mod class;
mod convert;

pub use crate::object::{Obj, Value, R};
pub use crate::pyint::BigInt;
pub use crate::vm::Interp;
pub use class::{extend_type, is_instance, module_object, native_value, opaque_instance, owner_of, type_object, NativeIter, Py};
pub use convert::{buffer_error, index, native_error};
pub use path::{bytes_path, convert_path, fspath, wrap_path, FsPath, PathArg, PathOrFd};
pub use lumen_bind::{ErrorKind, NativeError, NativeResult, This};

use crate::object::Kind;
use args::takes_receiver;
use class::{opaque_cell, reentrant};
use lumen_bind::{
    Class, CtorRet, Elem, FnDesc, FromArg, FromVarKw, Host, IntKind, IntoError, IntoRet, Methods, Module, Native,
    NextRet, Role, Slot,
};
use lumen_common::buffer::{BufferError, Export, Lend};
use std::any::{Any, TypeId};
use std::cell::{Cell, Ref, RefMut, UnsafeCell};
use std::marker::PhantomData;

/// The Python interpreter as a [`lumen_bind::Host`].
pub struct PyHost;

struct SyncValue(Value);
// SAFETY: only `Value::None` is stored, which holds no reference-counted data.
unsafe impl Sync for SyncValue {}
static NONE: SyncValue = SyncValue(Value::None);

/// The per-call context of a bound native.
pub struct PyCx<'s> {
    it: *mut Interp,
    recv: &'s Value,
    args: &'s [Value],
    kw: &'s [(Obj, Value)],
    desc: &'static FnDesc,
    in_ctx: Cell<bool>,
    /// The first guard (usually the receiver's borrow), kept inline so a method call allocates
    /// nothing; later guards go to `scratch`.
    first: UnsafeCell<Option<Guard>>,
    /// The first export taken in argument order ([`Host::reserve_bytes`]); later ones go to
    /// `scratch`.
    export: Cell<Option<Export>>,
    /// Lazily boxed side storage (null until a conversion needs it).
    scratch: Cell<*mut Scratch>,
    _it: PhantomData<&'s mut Interp>,
}

/// What converted arguments borrow from until the call returns. `guards` drop first.
#[derive(Default)]
struct Scratch {
    guards: Vec<Guard>,
    seqs: Vec<Box<[Value]>>,
    stores: Vec<std::rc::Rc<crate::object::ByteStore>>,
    exports: Vec<Export>,
}

/// A borrow held for the call. The `'static` is a lie told to store it: every guard borrows a
/// value that outlives the [`PyCx`] (an argument, or an element of `Scratch::seqs` /
/// `Scratch::stores`, which drop after the guards).
#[allow(dead_code)]
enum Guard {
    Ref(Ref<'static, Box<dyn Any>>),
    Mut(RefMut<'static, Box<dyn Any>>),
    /// A byte buffer argument: an export for the call (CPython's `Py_buffer`), so Python code
    /// the call runs cannot resize the buffer but may still write it in place.
    Lend(Lend<'static>),
}

impl Drop for PyCx<'_> {
    #[inline(always)]
    fn drop(&mut self) {
        // The inline guard may borrow a store in the scratch: release it first.
        drop(self.first.get_mut().take());
        let p = self.scratch.get();
        if !p.is_null() {
            drop_scratch(p);
        }
    }
}

#[cold]
#[inline(never)]
fn drop_scratch(p: *mut Scratch) {
    // SAFETY: `p` came from `Box::into_raw` in `PyCx::scratch` and is dropped once.
    drop(unsafe { Box::from_raw(p) });
}

struct ResetOnDrop<'a>(&'a Cell<bool>);
impl Drop for ResetOnDrop<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl<'s> PyCx<'s> {
    #[inline(always)]
    fn new(it: &'s mut Interp, recv: &'s Value, args: &'s [Value], kw: &'s [(Obj, Value)], desc: &'static FnDesc) -> PyCx<'s> {
        PyCx {
            it,
            recv,
            args,
            kw,
            desc,
            in_ctx: Cell::new(false),
            first: UnsafeCell::new(None),
            export: Cell::new(None),
            scratch: Cell::new(std::ptr::null_mut()),
            _it: PhantomData,
        }
    }

    /// The bound fn's descriptor.
    pub fn desc(&self) -> &'static FnDesc {
        self.desc
    }

    /// The interpreter, for a conversion. Conversions never nest, and nothing they return
    /// points into the interpreter, so this is the only live `&mut Interp`.
    #[inline(always)]
    #[allow(clippy::mut_from_ref)]
    fn it(&self) -> &mut Interp {
        assert!(!self.in_ctx.get(), "PyCx used while its interpreter is lent out");
        // SAFETY: see above; `in_ctx` rules out the lent-out `&mut` of `with_ctx`.
        unsafe { &mut *self.it }
    }

    #[allow(clippy::mut_from_ref)]
    fn scratch(&self) -> &mut Scratch {
        let mut p = self.scratch.get();
        if p.is_null() {
            p = Box::into_raw(Box::<Scratch>::default());
            self.scratch.set(p);
        }
        // SAFETY: single-threaded; callers finish with the `&mut Scratch` before any other call
        // that takes it, and hand out only pointers into heap data the scratch owns.
        unsafe { &mut *p }
    }

    #[inline(always)]
    fn hold(&self, g: Guard) {
        // SAFETY: single-threaded, and no reference to the slot itself is ever handed out (only
        // to the data its guard borrows, which does not move when the guard does).
        let first = unsafe { &mut *self.first.get() };
        if first.is_none() {
            *first = Some(g);
        } else {
            self.hold_more(g);
        }
    }

    #[cold]
    #[inline(never)]
    fn hold_more(&self, g: Guard) {
        self.scratch().guards.push(g);
    }

    /// Lend `store`'s bytes (or `range` of them) until the call returns: the slice the native
    /// sees. Python code the native runs meanwhile may write the buffer in place (the store
    /// moves to a copy rather than alias the slice) but not resize it, as with a CPython export.
    fn lend<'c>(
        &'c self,
        store: &'c crate::object::ByteStore,
        range: Option<std::ops::Range<usize>>,
        mutable: bool,
    ) -> Result<*mut [u8], Obj> {
        let mut l = store.lend(mutable).map_err(|e| buffer_error(self.it(), e))?;
        let all: *mut [u8] = if mutable { l.as_mut_slice() } else { l.as_slice() as *const [u8] as *mut [u8] };
        // SAFETY: see `Guard`; the memory stays valid and unaliased while the lend lives.
        self.hold(Guard::Lend(unsafe { std::mem::transmute::<Lend<'_>, Lend<'static>>(l) }));
        Ok(match range {
            // SAFETY: as above; `r` lies inside the view the caller checked against the store.
            Some(r) => {
                assert!(r.start <= r.end && r.end <= all.len());
                // SAFETY: `r` lies inside the lent range (asserted).
                std::ptr::slice_from_raw_parts_mut(unsafe { all.cast::<u8>().add(r.start) }, r.len())
            }
            None => all,
        })
    }

    /// `f() argument 'x' must be str, not int` (or the receiver / element variant).
    #[cold]
    #[inline(never)]
    fn arg_error(&self, at: Slot, what: &str, v: &Value) -> Obj {
        let it = self.it();
        let d = self.desc;
        if at.is_this() {
            let t = it.type_name_of(v);
            let owner = d.class().map(args::class_qualname).unwrap_or_default();
            if args::is_slot_wrapper(d) {
                let msg = format!("descriptor '{}' requires a '{}' object but received a '{}'", args::py_name(d), owner, t);
                return it.type_error(&msg);
            }
            let msg = format!("descriptor '{}' for '{}' objects doesn't apply to a '{}' object", args::py_name(d), owner, t);
            return it.type_error(&msg);
        }
        if at.element().is_some() {
            let t = it.type_name_of(v);
            return it.type_error(&format!("expected {}, not {}", what, t));
        }
        args::bad_argument(it, d, at.index().unwrap_or(u32::MAX) as usize, what, v)
    }
}

#[cold]
#[inline(never)]
fn not_a_type(it: &mut Interp, d: &'static FnDesc, v: &Value) -> Obj {
    let name = args::class_name(d);
    let t = it.type_name_of(v);
    it.type_error(&format!("{}.__new__(X): X is not a type object ({})", name, t))
}

/// The engine entry of `N`: CPython's `(self, *args, **kwargs)` convention.
fn py_entry<N: Native<PyHost>>(it: &mut Interp, a: &[Value], kw: &[(Obj, Value)]) -> R<Value> {
    let d = N::DESC;
    let (recv, rest) = if takes_receiver(d) {
        match a.split_first() {
            Some(x) => x,
            None => return Err(args::needs_self(it, d)),
        }
    } else {
        (&NONE.0, a)
    };
    if d.role == Role::Constructor && !recv.is_type() {
        return Err(not_a_type(it, d, recv));
    }
    let cx = PyCx::new(it, recv, rest, kw, d);
    N::call(&cx)
}

/// Bit set when the signature has a required keyword-only parameter (checked off the fast path).
#[inline]
fn needs_kwonly(d: &FnDesc, n: usize) -> bool {
    n > d.max_pos as usize && d.named().skip(d.max_pos as usize).any(|p| !p.optional())
}

impl Host for PyHost {
    const NAME: &'static str = "py";
    type Value = Value;
    type Error = Obj;
    type Ctx = Interp;
    type Cx<'s> = PyCx<'s>;
    type Entry = crate::object::NativeFn;

    fn entry<N: Native<Self>>() -> Self::Entry {
        py_entry::<N>
    }

    #[inline(always)]
    fn bind<'c, const N: usize>(cx: &'c PyCx<'_>) -> Result<[Option<&'c Value>; N], Obj> {
        let d = cx.desc;
        let a: &'c [Value] = cx.args;
        let n = a.len();
        let mut slots = [None; N];
        if cx.kw.is_empty()
            && n >= d.min_pos as usize
            && (n <= d.max_pos as usize || d.has_varargs())
            && !needs_kwonly(d, N)
        {
            for (s, v) in slots.iter_mut().zip(&a[..n.min(d.max_pos as usize)]) {
                *s = Some(v);
            }
            return Ok(slots);
        }
        args::bind_slow(cx.it(), d, a, cx.kw, &mut slots)?;
        Ok(slots)
    }

    #[inline]
    fn rest<'c>(cx: &'c PyCx<'_>) -> &'c [Value] {
        cx.args.get(cx.desc.max_pos as usize..).unwrap_or(&[])
    }

    fn varkw<'c>(cx: &'c PyCx<'_>) -> Vec<(&'c str, &'c Value)> {
        let d = cx.desc;
        cx.kw
            .iter()
            .map(|(k, v)| (k.as_str_kind().unwrap_or(""), v))
            .filter(|(k, _)| !args::is_named_keyword(d, k))
            .collect()
    }

    #[inline(always)]
    fn this<'c>(cx: &'c PyCx<'_>) -> &'c Value {
        cx.recv
    }

    fn absent() -> &'static Value {
        &NONE.0
    }

    #[inline]
    fn with_ctx<R>(cx: &PyCx<'_>, f: impl FnOnce(&mut Interp) -> R) -> R {
        let it = cx.it();
        cx.in_ctx.set(true);
        let reset = ResetOnDrop(&cx.in_ctx);
        let r = f(it);
        drop(reset);
        r
    }

    #[inline]
    fn ret<T: IntoRet<Self>>(cx: &PyCx<'_>, r: T) -> Result<Value, Obj> {
        T::into_ret(r, cx.it())
    }

    #[inline]
    fn ret_next<T: NextRet<Self>>(cx: &PyCx<'_>, r: T) -> Result<Value, Obj> {
        T::into_next(r, cx.it())
    }

    fn construct<T: Class>(cx: &PyCx<'_>, value: T) -> Result<Value, Obj> {
        match cx.recv {
            Value::Obj(cls) => Ok(opaque_instance(cls, value)),
            _ => unreachable!("checked by the entry"),
        }
    }

    // ---- arguments ---------------------------------------------------------------------------

    #[inline(always)]
    fn is_none(v: &Value) -> bool {
        matches!(v, Value::None)
    }

    #[inline(always)]
    fn to_f64(cx: &PyCx<'_>, v: &Value, _: Slot) -> Result<f64, Obj> {
        match v {
            Value::Float(f) => Ok(*f),
            Value::Int(i) => Ok(*i as f64),
            _ => cx.it().float_arg(v),
        }
    }

    #[inline(always)]
    fn to_int(cx: &PyCx<'_>, v: &Value, at: Slot, kind: IntKind) -> Result<i128, Obj> {
        if let Value::Int(i) = v {
            let n = *i as i128;
            if n >= kind.min() && n <= kind.max() {
                return Ok(n);
            }
        }
        if kind.size && kind.signed && at.index() == Some(0) && matches!(cx.desc.role, Role::Proto("getitem" | "setitem" | "delitem")) {
            // A subscript: `PyNumber_AsSsize_t(key, PyExc_IndexError)`.
            return cx.it().index_or(v, "IndexError").map(i128::from);
        }
        convert::to_int(cx.it(), v, kind)
    }

    fn to_bigint(cx: &PyCx<'_>, v: &Value, _: Slot) -> Result<BigInt, Obj> {
        let n = index(cx.it(), v)?;
        Ok(n.as_bigint().unwrap_or_else(|| BigInt::from_i64(0)))
    }

    #[inline]
    fn to_bool(cx: &PyCx<'_>, v: &Value, _: Slot) -> Result<bool, Obj> {
        match v {
            Value::Bool(b) => Ok(*b),
            _ => cx.it().truthy(v),
        }
    }

    #[inline(always)]
    fn to_str<'c>(cx: &'c PyCx<'_>, v: &'c Value, at: Slot) -> Result<&'c str, Obj> {
        match v.as_str() {
            Some(s) => Ok(s),
            None => Err(cx.arg_error(at, "str", v)),
        }
    }

    fn to_bytes<'c>(cx: &'c PyCx<'_>, v: &'c Value, at: Slot) -> Result<&'c [u8], Obj> {
        if let Value::Obj(o) = v {
            match &o.kind {
                Kind::Bytes(b) => return Ok(b),
                Kind::ByteArray(store) => return cx.lend(store, None, false).map(|p| unsafe { &*p }),
                Kind::Opaque(_) => {
                    if let Some(part) = crate::builtins::memview::contiguous_part(cx.it(), v)? {
                        return Ok(match part {
                            crate::builtins::memview::Part::Bytes(o, r) => match &o.kind {
                                // SAFETY: the memoryview `v` keeps its immutable `bytes` object alive.
                                Kind::Bytes(b) => unsafe { &*(&b[r] as *const [u8]) },
                                _ => &[],
                            },
                            crate::builtins::memview::Part::Store(store, r) => {
                                let s = cx.scratch();
                                s.stores.push(store);
                                let store: *const crate::object::ByteStore = &**s.stores.last().unwrap();
                                // SAFETY: the store lives in `Scratch::stores` until after the guard.
                                unsafe { &*cx.lend(&*store, Some(r), false)? }
                            }
                        });
                    }
                }
                _ => {}
            }
        }
        let _ = at;
        Err(not_buffer(cx, v))
    }

    /// CPython exports a `bytearray` argument while parsing the arguments, so a later argument's
    /// `__index__` cannot resize it.
    fn reserve_bytes(cx: &PyCx<'_>, v: &Value, _: Slot) -> Result<(), Obj> {
        if let Value::Obj(o) = v {
            if let Kind::ByteArray(store) = &o.kind {
                let e = store.export().map_err(|e| buffer_error(cx.it(), e))?;
                match cx.export.take() {
                    None => cx.export.set(Some(e)),
                    Some(first) => {
                        cx.export.set(Some(first));
                        cx.scratch().exports.push(e);
                    }
                }
            }
        }
        Ok(())
    }

    fn to_bytes_mut<'c>(cx: &'c PyCx<'_>, v: &'c Value, at: Slot) -> Result<&'c mut [u8], Obj> {
        if let Value::Obj(o) = v {
            if let Kind::ByteArray(store) = &o.kind {
                // SAFETY: the lent range is unaliased until the guard drops with `cx`.
                return cx.lend(store, None, true).map(|p| unsafe { &mut *p });
            }
            if let Some((_, store)) = crate::builtins::arraym::array::parts(cx.it(), v) {
                let s = cx.scratch();
                s.stores.push(store);
                let store: *const crate::object::ByteStore = &**s.stores.last().unwrap();
                // SAFETY: the store lives in `Scratch::stores` until after the guard, and the lent
                // range is unaliased until the guard drops with `cx`.
                return cx.lend(unsafe { &*store }, None, true).map(|p| unsafe { &mut *p });
            }
        }
        Err(cx.arg_error(at, "read-write bytes-like object", v))
    }

    fn to_byte_vec(cx: &PyCx<'_>, v: &Value, at: Slot) -> Result<Vec<u8>, Obj> {
        Self::to_bytes(cx, v, at).map(<[u8]>::to_vec)
    }

    fn to_seq<'c>(cx: &'c PyCx<'_>, v: &'c Value, _: Slot) -> Result<&'c [Value], Obj> {
        if let Some(t) = v.tuple_items() {
            return Ok(t);
        }
        let items: Box<[Value]> = match crate::object::list_of(v) {
            Some(l) => l.borrow().as_slice().into(),
            None => cx.it().iterate_to_vec(v)?.into_boxed_slice(),
        };
        let s = cx.scratch();
        s.seqs.push(items);
        let p: *const [Value] = &**s.seqs.last().unwrap();
        // SAFETY: the box's heap data lives (unmoved) as long as `cx`.
        Ok(unsafe { &*p })
    }

    #[inline(always)]
    fn class_ref<'c, T: Class>(cx: &'c PyCx<'_>, v: &'c Value, at: Slot) -> Result<&'c T, Obj> {
        if let Some(Ok(r)) = opaque_cell(v).map(|c| c.try_borrow()) {
            if let Some(x) = r.downcast_ref::<T>() {
                let p: *const T = x;
                // SAFETY: see `Guard`; the boxed value does not move while borrowed.
                cx.hold(Guard::Ref(unsafe { std::mem::transmute::<Ref<'_, Box<dyn Any>>, Ref<'static, Box<dyn Any>>>(r) }));
                return Ok(unsafe { &*p });
            }
        }
        Err(class_error::<T>(cx, v, at))
    }

    #[inline(always)]
    fn class_mut<'c, T: Class>(cx: &'c PyCx<'_>, v: &'c Value, at: Slot) -> Result<&'c mut T, Obj> {
        if let Some(Ok(mut r)) = opaque_cell(v).map(|c| c.try_borrow_mut()) {
            if let Some(x) = r.downcast_mut::<T>() {
                let p: *mut T = x;
                // SAFETY: see `Guard`; the boxed value does not move while borrowed.
                cx.hold(Guard::Mut(unsafe { std::mem::transmute::<RefMut<'_, Box<dyn Any>>, RefMut<'static, Box<dyn Any>>>(r) }));
                return Ok(unsafe { &mut *p });
            }
        }
        Err(class_error::<T>(cx, v, at))
    }

    // ---- results -----------------------------------------------------------------------------

    #[inline(always)]
    fn unit(_: &mut Interp) -> Value {
        Value::None
    }

    #[inline(always)]
    fn none(_: &mut Interp) -> Value {
        Value::None
    }

    #[inline(always)]
    fn from_bool(_: &mut Interp, b: bool) -> Value {
        Value::Bool(b)
    }

    #[inline(always)]
    fn from_f64(_: &mut Interp, x: f64) -> Value {
        Value::Float(x)
    }

    #[inline(always)]
    fn from_int(_: &mut Interp, n: i128) -> Result<Value, Obj> {
        Ok(match i64::try_from(n) {
            Ok(i) => Value::Int(i),
            Err(_) => Value::big(BigInt::from_i128(n)),
        })
    }

    fn from_bigint(_: &mut Interp, n: BigInt) -> Value {
        Value::big(n)
    }

    fn from_str(_: &mut Interp, s: &str) -> Value {
        Value::str(s)
    }

    fn from_string(_: &mut Interp, s: String) -> Value {
        Value::string(s)
    }

    fn from_bytes(_: &mut Interp, b: Vec<u8>) -> Value {
        Value::bytes(b)
    }

    fn from_list(_: &mut Interp, items: Vec<Value>) -> Value {
        Value::list(items)
    }

    fn from_tuple(_: &mut Interp, items: Vec<Value>) -> Value {
        Value::tuple(items)
    }

    fn new_instance<T: Class>(it: &mut Interp, value: T) -> Result<Value, Obj> {
        match it.native_types.get(&TypeId::of::<T>()) {
            Some(cls) => {
                let cls = cls.clone();
                Ok(opaque_instance(&cls, value))
            }
            None => {
                let msg = format!("native class '{}' is not registered with this interpreter", args::class_qualname(T::DESC));
                Err(it.type_error(&msg))
            }
        }
    }

    #[inline]
    fn iter_step(it: &mut Interp, item: Option<Value>) -> Result<Value, Obj> {
        match item {
            Some(v) => Ok(v),
            None => Err(it.new_exc_str("StopIteration", "")),
        }
    }

    fn error(it: &mut Interp, e: NativeError) -> Obj {
        native_error(it, e)
    }

    fn class_object<T: Methods<Self>>(it: &mut Interp) -> Result<Value, Obj> {
        Ok(Value::Obj(type_object::<T>(it)))
    }

    fn module_object<M: Module<Self>>(it: &mut Interp) -> Result<Value, Obj> {
        class::module_object::<M>(it).map(Value::Obj)
    }
}

/// Why `v` could not be borrowed as a `T`: already borrowed (re-entrant use of a `T`), or not
/// a `T` at all.
#[cold]
#[inline(never)]
fn class_error<T: Class>(cx: &PyCx<'_>, v: &Value, at: Slot) -> Obj {
    let busy = opaque_cell(v).is_some_and(|c| c.try_borrow_mut().is_err());
    if busy && is_instance::<T>(cx.it(), v) {
        return reentrant::<T>(cx.it());
    }
    cx.arg_error(at, &args::class_qualname(T::DESC), v)
}

#[cold]
#[inline(never)]
fn not_buffer(cx: &PyCx<'_>, v: &Value) -> Obj {
    let it = cx.it();
    let t = it.type_name_of(v);
    it.type_error(&format!("a bytes-like object is required, not '{}'", t))
}

// ---- Python's own types --------------------------------------------------------------------------

impl<'a> FromArg<'a, PyHost> for Value {
    #[inline(always)]
    fn from_arg(_: &'a PyCx<'_>, v: &'a Value, _: Slot) -> Result<Self, Obj> {
        Ok(v.clone())
    }
}

impl<'a> FromArg<'a, PyHost> for &'a Value {
    #[inline(always)]
    fn from_arg(_: &'a PyCx<'_>, v: &'a Value, _: Slot) -> Result<Self, Obj> {
        Ok(v)
    }
}

/// An exception instance (`BaseException` or a subclass), e.g. the receiver of a core
/// exception type's method.
#[derive(Clone, Copy)]
pub struct Exc<'a>(pub &'a Obj);

impl<'a> FromArg<'a, PyHost> for Exc<'a> {
    #[inline]
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        match v {
            Value::Obj(o) if matches!(o.kind, Kind::Exception(_)) => Ok(Exc(o)),
            _ => Err(cx.arg_error(at, "BaseException", v)),
        }
    }
}

impl<'a, T: Class> FromArg<'a, PyHost> for Py<T> {
    fn from_arg(cx: &'a PyCx<'_>, v: &'a Value, at: Slot) -> Result<Self, Obj> {
        if is_instance::<T>(cx.it(), v) {
            Ok(Py::from_value_unchecked(v.clone()))
        } else {
            Err(cx.arg_error(at, &args::class_qualname(T::DESC), v))
        }
    }
}

/// The keyword arguments a `#[varkw]` parameter collects: those not bound to a named
/// parameter. A view over the call's keyword slice (no allocation).
#[derive(Clone, Copy)]
pub struct KwArgs<'a> {
    kw: &'a [(Obj, Value)],
    desc: &'static FnDesc,
}

impl<'a> KwArgs<'a> {
    pub fn iter(&self) -> impl Iterator<Item = (&'a str, &'a Value)> + '_ {
        let d = self.desc;
        self.kw.iter().map(|(k, v)| (k.as_str_kind().unwrap_or(""), v)).filter(move |(k, _)| !args::is_named_keyword(d, k))
    }

    pub fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    pub fn len(&self) -> usize {
        self.iter().count()
    }

    pub fn get(&self, name: &str) -> Option<&'a Value> {
        self.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
    }

    /// The collected keywords as `(name, value)` pairs (e.g. to forward to `Interp::call`).
    pub fn to_vec(&self) -> Vec<(Obj, Value)> {
        let d = self.desc;
        self.kw.iter().filter(|(k, _)| !args::is_named_keyword(d, k.as_str_kind().unwrap_or(""))).cloned().collect()
    }
}

impl<'a> FromVarKw<'a, PyHost> for KwArgs<'a> {
    #[inline]
    fn from_varkw(cx: &'a PyCx<'_>) -> Result<Self, Obj> {
        Ok(KwArgs { kw: cx.kw, desc: cx.desc })
    }
}

impl IntoRet<PyHost> for Value {
    const MAY_RUN: bool = false;
    #[inline(always)]
    fn into_ret(self, _: &mut Interp) -> Result<Value, Obj> {
        Ok(self)
    }
}

impl IntoRet<PyHost> for &Value {
    const MAY_RUN: bool = false;
    #[inline(always)]
    fn into_ret(self, _: &mut Interp) -> Result<Value, Obj> {
        Ok(self.clone())
    }
}

impl IntoRet<PyHost> for Obj {
    const MAY_RUN: bool = false;
    #[inline(always)]
    fn into_ret(self, _: &mut Interp) -> Result<Value, Obj> {
        Ok(Value::Obj(self))
    }
}

impl<T: Class> IntoRet<PyHost> for Py<T> {
    const MAY_RUN: bool = false;
    #[inline(always)]
    fn into_ret(self, _: &mut Interp) -> Result<Value, Obj> {
        Ok(self.into_value())
    }
}

impl IntoRet<PyHost> for NativeIter {
    const MAY_RUN: bool = false;
    fn into_ret(self, it: &mut Interp) -> Result<Value, Obj> {
        Ok(it.native_iter(self.0))
    }
}

impl Elem for Value {}
impl<T> Elem for Py<T> {}

impl IntoError<PyHost> for Obj {
    #[inline(always)]
    fn into_error(self, _: &mut Interp) -> Obj {
        self
    }
}

impl IntoError<PyHost> for BufferError {
    fn into_error(self, it: &mut Interp) -> Obj {
        buffer_error(it, self)
    }
}

/// A `native_iter` class's constructor returns the step closure: an instance of the called
/// class (subclasses included) stepping it.
impl<T: Class> CtorRet<PyHost, T> for NativeIter {
    fn into_ctor(self, cx: &PyCx<'_>) -> Result<Value, Obj> {
        match cx.recv {
            Value::Obj(cls) => Ok(self.into_object(cls)),
            _ => unreachable!("checked by the entry"),
        }
    }
}

/// A constructor that builds its instance itself.
impl<T: Class> CtorRet<PyHost, T> for Value {
    fn into_ctor(self, _: &PyCx<'_>) -> Result<Value, Obj> {
        Ok(self)
    }
}
