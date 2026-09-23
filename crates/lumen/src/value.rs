//! Runtime values and the object model. Objects are `Rc<RefCell<Object>>` ([`Gc`]); there is no
//! real garbage collector yet (reference counting, so cycles leak — acceptable for the test262
//! loop). Properties are stored in insertion order in a small map.

use crate::ast::Function;
use crate::interpreter::{Env, Interp};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

/// A handle to a heap object. Opaque on purpose: every engine site goes through this API rather
/// than `Rc`/`RefCell` directly, so the storage underneath can move to a traced heap without
/// touching call sites (see `docs/gc.md`).
///
/// The box is lumen's own, laid out like std's `RcBox` — `{strong, weak, value}`. Semantics match
/// `Rc`: the strong owners collectively
/// hold one implicit weak count, the value drops when `strong` reaches zero and the allocation is
/// freed when `weak` does.
#[repr(transparent)]
pub struct Gc(std::ptr::NonNull<GcBox>);

#[repr(C)]
pub(in crate::value) struct GcBox {
    pub(in crate::value) strong: Cell<usize>,
    pub(in crate::value) weak: Cell<usize>,
    value: RefCell<Object>,
}

/// Byte offset from the stored handle word to the object cell.
const GC_VALUE_OFFSET: usize = std::mem::offset_of!(GcBox, value);

/// A non-owning [`Gc`]: upgrades while the object is alive. `WeakGc::new()` holds a dangling
/// sentinel (never dereferenced), as `std::rc::Weak::new` does.
#[repr(transparent)]
pub struct WeakGc(std::ptr::NonNull<GcBox>);

const WEAK_DANGLING: usize = usize::MAX;

impl Gc {
    /// Place a new object in `state`'s slab.
    #[inline]
    fn alloc(state: &GcState, cell: RefCell<Object>) -> Gc {
        let p = state.heap.borrow_mut().alloc();
        unsafe {
            p.write(GcBox {
                strong: Cell::new(1),
                weak: Cell::new(1),
                value: cell,
            });
            Gc(std::ptr::NonNull::new_unchecked(p))
        }
    }
    #[inline]
    fn inner(&self) -> &GcBox {
        unsafe { self.0.as_ref() }
    }
    #[inline]
    pub fn ptr_eq(a: &Gc, b: &Gc) -> bool {
        a.0 == b.0
    }
    /// The address of the object cell — stable for the object's lifetime, usable as an identity.
    #[inline]
    pub fn as_ptr(this: &Gc) -> *const RefCell<Object> {
        unsafe { std::ptr::addr_of!((*this.0.as_ptr()).value) }
    }
    #[inline]
    pub fn downgrade(this: &Gc) -> WeakGc {
        let b = this.inner();
        b.weak.set(b.weak.get() + 1);
        WeakGc(this.0)
    }
    #[inline]
    pub fn strong_count(this: &Gc) -> usize {
        this.inner().strong.get()
    }
    /// Consume the handle into a raw cell pointer (see [`Gc::from_raw`]).
    #[inline]
    pub fn into_raw(this: Gc) -> *const RefCell<Object> {
        let p = Gc::as_ptr(&this);
        std::mem::forget(this);
        p
    }
    /// # Safety
    /// `ptr` must come from [`Gc::into_raw`] (or [`Gc::as_ptr`] with a matching strong count).
    #[inline]
    pub unsafe fn from_raw(ptr: *const RefCell<Object>) -> Gc {
        let base = (ptr as *const u8).sub(GC_VALUE_OFFSET) as *mut GcBox;
        Gc(std::ptr::NonNull::new_unchecked(base))
    }
    /// # Safety
    /// `ptr` must point at a live object cell obtained from [`Gc::as_ptr`] / [`Gc::into_raw`].
    #[inline]
    pub unsafe fn increment_strong_count(ptr: *const RefCell<Object>) {
        let g = std::mem::ManuallyDrop::new(Gc::from_raw(ptr));
        let b = g.inner();
        b.strong.set(b.strong.get() + 1);
    }
    /// # Safety
    /// As [`Gc::increment_strong_count`], and the count being released must be owned.
    #[inline]
    pub unsafe fn decrement_strong_count(ptr: *const RefCell<Object>) {
        drop(Gc::from_raw(ptr));
    }
    #[inline]
    pub fn get_mut(this: &mut Gc) -> Option<&mut RefCell<Object>> {
        let b = this.inner();
        if b.strong.get() == 1 && b.weak.get() == 1 {
            Some(unsafe { &mut (*this.0.as_ptr()).value })
        } else {
            None
        }
    }
    #[inline]
    pub fn try_unwrap(this: Gc) -> Result<RefCell<Object>, Gc> {
        if this.inner().strong.get() != 1 {
            return Err(this);
        }
        let this = std::mem::ManuallyDrop::new(this);
        unsafe {
            let b = this.0.as_ptr();
            (*b).strong.set(0);
            let value = std::ptr::read(std::ptr::addr_of!((*b).value));
            // Release the strong owners' implicit weak reference.
            drop(WeakGc(this.0));
            Ok(value)
        }
    }
}

impl Clone for Gc {
    #[inline]
    fn clone(&self) -> Gc {
        let b = self.inner();
        b.strong.set(b.strong.get() + 1);
        Gc(self.0)
    }
}

impl Drop for Gc {
    #[inline]
    fn drop(&mut self) {
        let b = self.inner();
        let s = b.strong.get() - 1;
        b.strong.set(s);
        if s == 0 {
            unsafe { gc_drop_slow(self.0) };
        }
    }
}

#[cold]
#[inline(never)]
unsafe fn gc_drop_slow(p: std::ptr::NonNull<GcBox>) {
    std::ptr::drop_in_place(std::ptr::addr_of_mut!((*p.as_ptr()).value));
    drop(WeakGc(p));
}

impl std::ops::Deref for Gc {
    type Target = RefCell<Object>;
    #[inline]
    fn deref(&self) -> &RefCell<Object> {
        &self.inner().value
    }
}

impl WeakGc {
    #[inline]
    pub fn new() -> WeakGc {
        WeakGc(unsafe { std::ptr::NonNull::new_unchecked(WEAK_DANGLING as *mut GcBox) })
    }
    #[inline]
    fn inner(&self) -> Option<&GcBox> {
        if self.0.as_ptr() as usize == WEAK_DANGLING {
            None
        } else {
            Some(unsafe { self.0.as_ref() })
        }
    }
    #[inline]
    pub fn upgrade(&self) -> Option<Gc> {
        let b = self.inner()?;
        let s = b.strong.get();
        if s == 0 {
            return None;
        }
        b.strong.set(s + 1);
        Some(Gc(self.0))
    }
    #[inline]
    pub fn as_ptr(&self) -> *const RefCell<Object> {
        if self.0.as_ptr() as usize == WEAK_DANGLING {
            return WEAK_DANGLING as *const RefCell<Object>;
        }
        unsafe { std::ptr::addr_of!((*self.0.as_ptr()).value) }
    }
    #[inline]
    pub fn ptr_eq(&self, other: &WeakGc) -> bool {
        self.0 == other.0
    }
    #[inline]
    pub fn strong_count(&self) -> usize {
        self.inner().map_or(0, |b| b.strong.get())
    }
}

impl Clone for WeakGc {
    #[inline]
    fn clone(&self) -> WeakGc {
        if let Some(b) = self.inner() {
            b.weak.set(b.weak.get() + 1);
        }
        WeakGc(self.0)
    }
}

impl Drop for WeakGc {
    #[inline]
    fn drop(&mut self) {
        let Some(b) = self.inner() else { return };
        let w = b.weak.get() - 1;
        b.weak.set(w);
        if w == 0 {
            unsafe { heap::free(self.0.as_ptr()) };
        }
    }
}

impl Default for WeakGc {
    fn default() -> WeakGc {
        WeakGc::new()
    }
}

/// A native (Rust-implemented) function. It can only throw (via `Err`), never break/return/continue,
/// so a plain `Result<Value, Value>` (Err = the thrown value) is the whole contract.
pub type NativeFn = fn(&mut Interp, Value, &[Value]) -> Result<Value, Value>;

/// A native function that carries captured state, unlike the bare-`fn` [`NativeFn`]. The embedder
/// uses this to wrap host callbacks that need associated data a function pointer can't hold — e.g.
/// an N-API C callback together with its `void*` and module handle.
pub type NativeClosure = dyn Fn(&mut Interp, Value, &[Value]) -> Result<Value, Value>;

/// The engine value. `repr(u8)` with fixed discriminants gives it a *defined* layout — tag byte
/// at offset 0, payload at offset 8. Tags 0..=4 are the trivially-copyable variants (no
/// refcount).
#[derive(Clone, Default)]
#[repr(u8)]
pub enum Value {
    #[default]
    Undefined = 0,
    /// The spec's EMPTY completion marker: produced only by *statement* evaluation (declarations
    /// and other value-less statements) so completion values thread per UpdateEmpty. Never a JS
    /// value — every engine boundary converts it to `Undefined` before a value escapes.
    Empty = 1,
    Null = 2,
    Bool(bool) = 3,
    Num(f64) = 4,
    /// BigInt, approximated with `i128` (exact within ±2^127; tests beyond that range fail rather
    /// than implementing arbitrary precision).
    BigInt(crate::bigint::JsBigInt) = 5,
    Str(crate::lstr::LStr) = 6,
    Sym(Rc<SymbolData>) = 7,
    Obj(Gc) = 8,
}

// NaN-boxed storage used for long-lived property values. Execution still uses the ergonomic
// `Value` enum while the migration is staged; packing at the heap boundary cuts each ordinary
// property by eight bytes without coupling the experiment to every interpreter pattern match.
#[repr(transparent)]
pub(crate) struct PackedValue(u64);

const PACK_PAYLOAD: u64 = 0x0000_ffff_ffff_ffff;
pub(crate) const PACK_UNDEFINED: u64 = 0x7ff9_0000_0000_0000;
pub(crate) const PACK_EMPTY: u64 = 0x7ffa_0000_0000_0000;
pub(crate) const PACK_NULL: u64 = 0x7ffb_0000_0000_0000;
pub(crate) const PACK_BOOL: u64 = 0x7ffc_0000_0000_0000;
pub(crate) const PACK_BIGINT: u64 = 0x7ffd_0000_0000_0000;
pub(crate) const PACK_STR: u64 = 0x7ffe_0000_0000_0000;
pub(crate) const PACK_SYM: u64 = 0x7fff_0000_0000_0000;
pub(crate) const PACK_OBJ: u64 = 0xfff9_0000_0000_0000;
pub(crate) const PACK_CANON_NAN: u64 = 0x7ff8_0000_0000_0000;

impl PackedValue {
    #[inline]
    fn tag(&self) -> u64 {
        self.0 & !PACK_PAYLOAD
    }

    unsafe fn into_word<T>(value: T) -> u64 {
        assert!(std::mem::size_of::<T>() <= std::mem::size_of::<usize>());
        let value = std::mem::ManuallyDrop::new(value);
        let mut word = 0usize;
        unsafe {
            std::ptr::copy_nonoverlapping(
                &*value as *const T as *const u8,
                &mut word as *mut usize as *mut u8,
                std::mem::size_of::<T>(),
            );
        }
        let word = word as u64;
        assert_eq!(
            word & !PACK_PAYLOAD,
            0,
            "pointer does not fit NaN-box payload"
        );
        word
    }

    unsafe fn read_word<T>(&self) -> T {
        assert!(std::mem::size_of::<T>() <= std::mem::size_of::<usize>());
        let word = (self.0 & PACK_PAYLOAD) as usize;
        let mut value = std::mem::MaybeUninit::<T>::uninit();
        unsafe {
            std::ptr::copy_nonoverlapping(
                &word as *const usize as *const u8,
                value.as_mut_ptr() as *mut u8,
                std::mem::size_of::<T>(),
            );
            value.assume_init()
        }
    }

    unsafe fn clone_word<T: Clone>(&self) -> T {
        let value = std::mem::ManuallyDrop::new(unsafe { self.read_word::<T>() });
        T::clone(&value)
    }

    unsafe fn drop_word<T>(&mut self) {
        assert!(std::mem::size_of::<T>() <= std::mem::size_of::<usize>());
        let word = (self.0 & PACK_PAYLOAD) as usize;
        let mut value = std::mem::MaybeUninit::<T>::uninit();
        unsafe {
            std::ptr::copy_nonoverlapping(
                &word as *const usize as *const u8,
                value.as_mut_ptr() as *mut u8,
                std::mem::size_of::<T>(),
            );
            value.assume_init_drop();
        }
    }

    pub(crate) fn pack(value: Value) -> PackedValue {
        let bits = match value {
            Value::Undefined => PACK_UNDEFINED,
            Value::Empty => PACK_EMPTY,
            Value::Null => PACK_NULL,
            Value::Bool(v) => PACK_BOOL | v as u64,
            Value::Num(v) => {
                if v.is_nan() {
                    PACK_CANON_NAN
                } else {
                    v.to_bits()
                }
            }
            Value::BigInt(v) => PACK_BIGINT | unsafe { Self::into_word(v) },
            Value::Str(v) => PACK_STR | unsafe { Self::into_word(v) },
            Value::Sym(v) => PACK_SYM | unsafe { Self::into_word(v) },
            Value::Obj(v) => PACK_OBJ | unsafe { Self::into_word(v) },
        };
        PackedValue(bits)
    }

    pub(crate) fn unpack(&self) -> Value {
        match self.tag() {
            PACK_UNDEFINED => Value::Undefined,
            PACK_EMPTY => Value::Empty,
            PACK_NULL => Value::Null,
            PACK_BOOL => Value::Bool(self.0 & 1 != 0),
            PACK_BIGINT => Value::BigInt(unsafe { self.clone_word() }),
            PACK_STR => Value::Str(unsafe { self.clone_word() }),
            PACK_SYM => Value::Sym(unsafe { self.clone_word() }),
            PACK_OBJ => Value::Obj(unsafe { self.clone_word() }),
            _ => Value::Num(f64::from_bits(self.0)),
        }
    }

    /// Consume the packed owner without a refcount round trip. Pointer payload bits become the
    /// returned `Value`'s ownership; `ManuallyDrop` prevents this container from releasing them.
    pub(crate) fn into_value(self) -> Value {
        let this = std::mem::ManuallyDrop::new(self);
        match this.tag() {
            PACK_UNDEFINED => Value::Undefined,
            PACK_EMPTY => Value::Empty,
            PACK_NULL => Value::Null,
            PACK_BOOL => Value::Bool(this.0 & 1 != 0),
            PACK_BIGINT => Value::BigInt(unsafe { this.read_word() }),
            PACK_STR => Value::Str(unsafe { this.read_word() }),
            PACK_SYM => Value::Sym(unsafe { this.read_word() }),
            PACK_OBJ => Value::Obj(unsafe { this.read_word() }),
            _ => Value::Num(f64::from_bits(this.0)),
        }
    }
}

impl Clone for PackedValue {
    fn clone(&self) -> Self {
        PackedValue::pack(self.unpack())
    }
}

impl Drop for PackedValue {
    fn drop(&mut self) {
        match self.tag() {
            PACK_BIGINT => unsafe { self.drop_word::<crate::bigint::JsBigInt>() },
            PACK_STR => unsafe { self.drop_word::<crate::lstr::LStr>() },
            PACK_SYM => unsafe { self.drop_word::<Rc<SymbolData>>() },
            PACK_OBJ => unsafe { self.drop_word::<Gc>() },
            _ => {}
        }
    }
}

#[cfg(test)]
mod packed_value_tests {
    use super::*;

    #[test]
    fn packed_value_is_one_word_and_round_trips_scalars() {
        assert_eq!(std::mem::size_of::<PackedValue>(), 8);
        assert!(matches!(
            PackedValue::pack(Value::Undefined).into_value(),
            Value::Undefined
        ));
        assert!(matches!(
            PackedValue::pack(Value::Empty).into_value(),
            Value::Empty
        ));
        assert!(matches!(
            PackedValue::pack(Value::Null).into_value(),
            Value::Null
        ));
        assert!(matches!(
            PackedValue::pack(Value::Bool(false)).into_value(),
            Value::Bool(false)
        ));
        assert!(matches!(
            PackedValue::pack(Value::Bool(true)).into_value(),
            Value::Bool(true)
        ));
        for n in [0.0f64, -0.0, 42.5, f64::INFINITY, f64::NAN] {
            let Value::Num(out) = PackedValue::pack(Value::Num(n)).into_value() else {
                panic!("number changed kind")
            };
            assert!(n.is_nan() && out.is_nan() || n.to_bits() == out.to_bits());
        }
    }

    #[test]
    fn packed_value_moves_reference_ownership_without_a_bump() {
        let obj = Object::new(None);
        let before = Gc::strong_count(&obj);
        let packed = PackedValue::pack(Value::Obj(obj.clone()));
        assert_eq!(Gc::strong_count(&obj), before + 1);
        let out = packed.into_value();
        assert_eq!(Gc::strong_count(&obj), before + 1);
        drop(out);
        assert_eq!(Gc::strong_count(&obj), before);
    }
}

/// A unique Symbol. Identity is the `id` (every `Symbol()` call gets a fresh one); `description` is
/// the optional label. Well-known symbols (`Symbol.iterator`, …) are just pre-allocated instances.
pub struct SymbolData {
    pub id: u64,
    pub description: Option<Rc<str>>,
}

impl Value {
    pub fn str(s: impl Into<crate::lstr::LStr>) -> Value {
        Value::Str(s.into())
    }
    pub fn from_string(s: String) -> Value {
        Value::Str(s.into())
    }
    /// A BigInt from an `i64` (for the embedder's 64-bit integer bridge, e.g. wasm i64).
    pub fn bigint_from_i64(v: i64) -> Value {
        Value::BigInt(crate::bigint::JsBigInt::from(v))
    }
    /// A BigInt from a `u64` (for the embedder's 64-bit bridge, e.g. an unsigned FFI return).
    pub fn bigint_from_u64(v: u64) -> Value {
        Value::BigInt(crate::bigint::JsBigInt::from_u64(v))
    }
    /// A BigInt from an `i128` (an FFI `int64_t` widened to preserve its sign).
    pub fn bigint_from_i128(v: i128) -> Value {
        Value::BigInt(crate::bigint::JsBigInt::from_i128(v))
    }
    /// Read a BigInt as an `i64` (wrapping past ±2^63), for the embedder's 64-bit bridge. `None`
    /// when the value isn't a BigInt.
    pub fn bigint_as_i64(&self) -> Option<i64> {
        match self {
            Value::BigInt(b) => Some(b.to_i128_wrapping() as i64),
            _ => None,
        }
    }
    pub fn as_obj(&self) -> Option<&Gc> {
        match self {
            Value::Obj(o) => Some(o),
            _ => None,
        }
    }
    /// The number, if this is a `Number` (an embedder convenience for reading op arguments).
    pub fn as_num_opt(&self) -> Option<f64> {
        match self {
            Value::Num(n) => Some(*n),
            _ => None,
        }
    }
    pub fn is_callable(&self) -> bool {
        matches!(self, Value::Obj(o) if !matches!(o.borrow().call, Callable::None))
    }
    pub fn type_of(&self) -> &'static str {
        match self {
            Value::Undefined | Value::Empty => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Num(_) => "number",
            Value::BigInt(_) => "bigint",
            Value::Str(_) => "string",
            Value::Sym(_) => "symbol",
            Value::Obj(o) => {
                if matches!(o.borrow().call, Callable::None) {
                    "object"
                } else {
                    "function"
                }
            }
        }
    }
}

/// How an object can be called. Most objects are not callable (`None`).
#[derive(Clone)]
pub enum Callable {
    None,
    Native(NativeFn),
    /// A native function carrying captured state (see [`NativeClosure`]).
    NativeData(Rc<NativeCallable>),
    /// An interpreted function: its AST plus the lexical environment it closed over.
    User(Rc<UserCallable>),
    /// The result of `Function.prototype.bind`.
    Bound(Box<BoundCallable>),
    /// A ShadowRealm wrapped function: `target` is a callable inside the sub-realm identified by
    /// `realm` (its pointer). Calls marshal primitive args in and the primitive result out.
    WrappedShadow(Rc<WrappedShadowCallable>),
    /// The inverse: a function living *inside* a ShadowRealm whose `target` is a callable of the
    /// host realm. `realm` is this sub-realm's key in the host's map and `parent` is the host
    /// interpreter's stable address (hosts are either the engine root or boxed sub-realms, both
    /// pinned in memory while any of their sub-realm objects exist).
    WrappedCross(Box<WrappedCrossCallable>),
    /// An auto-accessor's synthesized getter: reads the private backing field (brand-checked) off
    /// the receiver.
    AccessorGet(Rc<Rc<str>>),
    /// An auto-accessor's synthesized setter: writes the private backing field (brand-checked).
    AccessorSet(Rc<Rc<str>>),
    /// A decorator `context.access.get`: returns `args[0][name]`.
    PropGet(Rc<Rc<str>>),
    /// A decorator `context.access.set`: performs `args[0][name] = args[1]`.
    PropSet(Rc<Rc<str>>),
}

/// Cold payloads boxed out of [`Callable`], so every non-callable ordinary object does not pay
/// for the largest function variants inline.
#[derive(Clone)]
pub struct BoundCallable {
    pub(crate) target: Gc,
    pub(crate) this: Value,
    pub(crate) args: Vec<Value>,
}

pub struct NativeCallable {
    pub(crate) func: Rc<NativeClosure>,
}

#[derive(Clone)]
pub struct UserCallable {
    pub(crate) func: Rc<Function>,
    pub(crate) env: Env,
}

#[derive(Clone)]
pub struct WrappedShadowCallable {
    pub(crate) realm: usize,
    pub(crate) target: Box<Value>,
}

#[derive(Clone)]
pub struct WrappedCrossCallable {
    pub(crate) realm: usize,
    pub(crate) parent: usize,
    pub(crate) target: Box<Value>,
}

impl Callable {
    pub(crate) fn user(func: Rc<Function>, env: Env) -> Callable {
        Callable::User(Rc::new(UserCallable { func, env }))
    }

    pub(crate) fn wrapped_shadow(realm: usize, target: Value) -> Callable {
        Callable::WrappedShadow(Rc::new(WrappedShadowCallable {
            realm,
            target: Box::new(target),
        }))
    }

    pub(crate) fn bound(target: Gc, this: Value, args: Vec<Value>) -> Callable {
        Callable::Bound(Box::new(BoundCallable { target, this, args }))
    }

    pub(crate) fn wrapped_cross(realm: usize, parent: usize, target: Value) -> Callable {
        Callable::WrappedCross(Box::new(WrappedCrossCallable {
            realm,
            parent,
            target: Box::new(target),
        }))
    }
}

/// Exotic internal-slot kind of a built-in object. A primitive wrapper's [[NumberData]] /
/// [[StringData]] / ... and an error's captured stack live in one hidden own entry
/// ([`EXOTIC_SLOT`]; private-keyed, so no reflection sees it), which keeps the tag one byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Exotic {
    None,
    Array,
    BoolWrap,
    NumWrap,
    StrWrap,
    SymWrap,
    BigIntWrap,
    /// An error object. Its slot carries the captured call-stack frames as a preformatted string
    /// (the `\n    at <fn>` lines, empty when thrown at top level), snapshotted at construction;
    /// the `Error.prototype.stack` getter prepends the live `name: message` head. name/message
    /// live as ordinary properties, and the tag lets `Error.prototype.toString` / the test262
    /// runner recognise an error cheaply.
    Error,
    /// An `arguments` exotic object (mapped index/parameter aliasing lives in
    /// `Interp::mapped_arguments`).
    Arguments,
}

/// The hidden own entry holding an exotic object's internal-slot value (see [`Exotic`]).
pub(crate) const EXOTIC_SLOT: &str = "#\u{0}exotic";

/// 72 bytes (96 allocated with the `Rc<RefCell<_>>` header).
pub struct Object {
    pub(crate) proto: Option<Gc>,
    pub(crate) props: Props,
    pub(crate) call: Callable,
    pub(crate) exotic: Exotic,
    pub(crate) extensible: bool,
    /// `false` for objects whose behavior lives in an interpreter side table — proxies, typed
    /// arrays, module namespaces — which the `exotic` tag can't reveal. Inline property/element
    /// caches check this byte on the receiver and take the checked path when clear, so ONE proxy
    /// existing somewhere doesn't disable the caches for every plain object in the program.
    pub(crate) ic_plain: Cell<bool>,
    /// The construct-time prototype handed to instances (`F.prototype`), cached for `new`.
    pub(crate) is_constructor: bool,
    /// GC scratch: the internal-reference count (low bits) and mark bit ([`GC_MARK`]) while
    /// collection runs; zero between collections.
    gc_internal: Cell<u32>,
}

const GC_MARK: u32 = 1 << 31;

impl Object {
    #[inline]
    pub(crate) fn gc_marked(&self) -> bool {
        self.gc_internal.get() & GC_MARK != 0
    }
    #[inline]
    pub(crate) fn gc_set_mark(&self) {
        self.gc_internal.set(self.gc_internal.get() | GC_MARK);
    }
    /// Start a collection: no mark, zero internal references.
    #[inline]
    pub(crate) fn gc_reset(&self) {
        self.gc_internal.set(0);
    }
    #[inline]
    pub(crate) fn gc_refs(&self) -> u32 {
        self.gc_internal.get() & !GC_MARK
    }
    #[inline]
    pub(crate) fn gc_add_ref(&self) {
        self.gc_internal.set(self.gc_internal.get() + 1);
    }
    /// Drop the scratch reference count after collection, keeping the mark for the sweep.
    #[inline]
    fn gc_clear_refs(&self) {
        self.gc_internal.set(self.gc_internal.get() & GC_MARK);
    }

    /// Give this object an exotic kind together with its internal-slot value.
    pub(crate) fn set_exotic(&mut self, exotic: Exotic, payload: Option<Value>) {
        self.exotic = exotic;
        if let Some(v) = payload {
            self.props
                .insert(EXOTIC_SLOT, Property::data(v, false, false, false));
        }
    }

    /// The internal-slot value of a wrapper / error object (see [`Exotic`]).
    pub(crate) fn exotic_payload(&self) -> Option<Value> {
        match self.exotic {
            Exotic::None | Exotic::Array | Exotic::Arguments => None,
            _ => self.props.get(EXOTIC_SLOT).map(|p| p.value()),
        }
    }

    /// [[StringData]] of a String wrapper.
    pub(crate) fn str_wrap(&self) -> Option<crate::lstr::LStr> {
        if self.exotic != Exotic::StrWrap {
            return None;
        }
        match self.exotic_payload() {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    /// [[NumberData]] of a Number wrapper.
    pub(crate) fn num_wrap(&self) -> Option<f64> {
        if self.exotic != Exotic::NumWrap {
            return None;
        }
        match self.exotic_payload() {
            Some(Value::Num(n)) => Some(n),
            _ => None,
        }
    }

    /// [[BooleanData]] of a Boolean wrapper.
    pub(crate) fn bool_wrap(&self) -> Option<bool> {
        if self.exotic != Exotic::BoolWrap {
            return None;
        }
        match self.exotic_payload() {
            Some(Value::Bool(b)) => Some(b),
            _ => None,
        }
    }

    /// [[SymbolData]] of a Symbol wrapper.
    pub(crate) fn sym_wrap(&self) -> Option<Rc<SymbolData>> {
        if self.exotic != Exotic::SymWrap {
            return None;
        }
        match self.exotic_payload() {
            Some(Value::Sym(s)) => Some(s),
            _ => None,
        }
    }

    /// [[BigIntData]] of a BigInt wrapper.
    pub(crate) fn bigint_wrap(&self) -> Option<crate::bigint::JsBigInt> {
        if self.exotic != Exotic::BigIntWrap {
            return None;
        }
        match self.exotic_payload() {
            Some(Value::BigInt(n)) => Some(n),
            _ => None,
        }
    }

    /// The captured stack frames of an error object (see [`Exotic::Error`]).
    pub(crate) fn error_stack(&self) -> Option<crate::lstr::LStr> {
        if self.exotic != Exotic::Error {
            return None;
        }
        match self.exotic_payload() {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }

    pub(crate) fn new(proto: Option<Gc>) -> Gc {
        Self::new_with_capacity(proto, 0)
    }

    /// Allocate an ordinary object's named-property vector at its known final size. Constructor
    /// chunks derive a conservative straight-line field count, replacing the usual 1 → 2 → 4
    /// growth sequence with one exact allocation. The hint lives on shared code, not instances.
    pub(crate) fn new_with_capacity(proto: Option<Gc>, property_capacity: usize) -> Gc {
        Self::new_with_parts(proto, Props::with_capacity(property_capacity), Exotic::None)
    }

    /// Allocate an object around an already-finalized property map. Literal fast paths can build
    /// the map from moved stack values before allocation, avoiding an empty map plus RefCell
    /// replacement on every object.
    pub(crate) fn new_with_parts(proto: Option<Gc>, props: Props, exotic: Exotic) -> Gc {
        with_gc_state(|state| {
            state.live.set(state.live.get() + 1);
            Gc::alloc(
                state,
                RefCell::new(Object {
                    proto,
                    props,
                    call: Callable::None,
                    exotic,
                    extensible: true,
                    ic_plain: Cell::new(true),
                    is_constructor: false,
                    gc_internal: Cell::new(0),
                }),
            )
        })
    }
}

impl Drop for Object {
    fn drop(&mut self) {
        // `try_with` so a drop during thread-local teardown at process exit can't panic.
        let _ = try_with_gc_state(|state| state.live.set(state.live.get() - 1));
    }
}

// The GC is a refcount-based cycle collector (lumen has no tracing GC). Every heap object lives
// in its interpreter's slab (`heap::ObjHeap`), which the collector walks to enumerate objects;
// the live count is maintained via Object::new / Drop. `Interp::gc_collect` reclaims objects
// referenced only by other (also-unreachable) objects — see interpreter.rs.

/// One interpreter's heap bookkeeping: the object registry, the live count, and the scope
/// registry (every `Scope` weakly, so the collector can see env-involving cycles).
///
/// A state belongs to the thread that created it, but it is *not* thread-local in the strict
/// sense: a coroutine body runs on a pooled worker thread (see `coroutine.rs`) and must allocate
/// into, and tombstone out of, the registry of the interpreter that spawned it — otherwise an
/// object born on the worker is invisible to the driver's collector, and an object dropped on the
/// worker tombstones a slot in the wrong registry, leaving the driver's slot dangling. The
/// coroutine's strict ping-pong handoff is what makes sharing the (non-`Sync`) cells sound: at any
/// instant exactly one of the threads that can reach this state is running.
pub(crate) struct GcState {
    heap: RefCell<heap::ObjHeap>,
    live: Cell<i64>,
    scopes: RefCell<Vec<std::rc::Weak<RefCell<crate::interpreter::Scope>>>>,
    /// Every function the parser gave a lazy body, weakly: the collector's flush pass releases
    /// the bodies that went cold (see `Function::release_cold_body`). Dead entries are pruned by
    /// that pass; a `Weak` pins only the node's allocation, never a body.
    lazy_fns: RefCell<Vec<std::rc::Weak<crate::ast::Function>>>,
    /// The object-shape transition tree for this heap (see [`props::ShapeTable`]): objects flow
    /// between the driver and its coroutine workers, so their shapes must resolve on either.
    pub(in crate::value) shapes: RefCell<props::ShapeTable>,
}

// See the type-level comment: exclusivity comes from the coroutine handoff, not from these cells.
unsafe impl Send for GcState {}
unsafe impl Sync for GcState {}

impl GcState {
    fn new() -> Arc<GcState> {
        Arc::new(GcState {
            heap: RefCell::new(heap::ObjHeap::new()),
            live: Cell::new(0),
            scopes: RefCell::new(Vec::new()),
            lazy_fns: RefCell::new(Vec::new()),
            shapes: RefCell::new(props::ShapeTable::new()),
        })
    }
}

thread_local! {
    /// The state this thread allocates into. A driver thread owns its own; a coroutine worker
    /// points at its driver's for the duration of the job (`enter_gc_state`).
    static GC_STATE: RefCell<Arc<GcState>> = RefCell::new(GcState::new());
}

pub(in crate::value) fn with_gc_state<R>(f: impl FnOnce(&GcState) -> R) -> R {
    GC_STATE.with(|s| f(&s.borrow()))
}

fn try_with_gc_state<R>(f: impl FnOnce(&GcState) -> R) -> Result<R, std::thread::AccessError> {
    GC_STATE.try_with(|s| f(&s.borrow()))
}

/// A handle to the state the current thread allocates into, for handing to a coroutine worker.
pub(crate) fn gc_state_handle() -> Arc<GcState> {
    GC_STATE.with(|s| s.borrow().clone())
}

/// Make `state` the current thread's heap state, returning the previous one so the caller can
/// restore it. Only called at quiescent points (between coroutine jobs), when no object on this
/// thread is being allocated or dropped.
pub(crate) fn enter_gc_state(state: Arc<GcState>) -> Arc<GcState> {
    GC_STATE.with(|s| std::mem::replace(&mut *s.borrow_mut(), state))
}

/// Number of live heap objects right now.
pub fn live_objects() -> i64 {
    with_gc_state(|state| state.live.get())
}

/// Strong handles to every currently-live heap object. The slab borrow is held for the walk,
/// and taking a handle only increments counts, so no object can disappear mid-walk.
pub fn gc_snapshot() -> Vec<Gc> {
    with_gc_state(|state| {
        let heap = state.heap.borrow();
        let mut live = Vec::with_capacity(state.live.get().max(0) as usize);
        heap.for_each_live(|b| unsafe {
            (*b).strong.set((*b).strong.get() + 1);
            live.push(Gc(std::ptr::NonNull::new_unchecked(b)));
        });
        live
    })
}

/// Clear the scratch reference counts left in `gc_internal` by a collection, keeping the mark
/// for the sweep. Collection calls this after marking.
pub(crate) fn gc_restore_registry_slots() {
    with_gc_state(|state| {
        state.heap.borrow().for_each_live(|b| unsafe {
            (*b).value.borrow().gc_clear_refs();
        });
    });
}

/// Return empty slab chunks to the system (after a sweep).
pub(crate) fn gc_trim_heap() {
    with_gc_state(|state| state.heap.borrow_mut().trim());
}

/// Slab chunks currently mapped for this thread's heap.
#[cfg(test)]
pub(crate) fn gc_heap_chunks() -> usize {
    with_gc_state(|state| state.heap.borrow().chunk_count())
}

pub(crate) fn register_scope(e: &crate::interpreter::Env) {
    with_gc_state(|state| state.scopes.borrow_mut().push(Rc::downgrade(e)));
}

/// Registered scope entries (live + not-yet-purged dead weaks).
pub(crate) fn scope_registry_len() -> usize {
    with_gc_state(|state| state.scopes.borrow().len())
}

/// Purge dead weak entries, returning the live count. A dead `Weak` still pins its `RcBox`
/// allocation, so an interpreter that churns through scopes without allocating many objects
/// (which is what arms the main GC) must prune on scope volume too — see `Interp::gc_check`.
pub(crate) fn scope_registry_prune() -> usize {
    with_gc_state(|state| {
        let mut reg = state.scopes.borrow_mut();
        reg.retain(|w| w.strong_count() > 0);
        reg.len()
    })
}

/// The live scopes (purging dead weak entries as it goes).
pub(crate) fn scope_snapshot() -> Vec<crate::interpreter::Env> {
    with_gc_state(|state| {
        let mut reg = state.scopes.borrow_mut();
        let mut live = Vec::with_capacity(reg.len());
        reg.retain(|w| match w.upgrade() {
            Some(e) => {
                live.push(e);
                true
            }
            None => false,
        });
        live
    })
}

pub(crate) fn register_lazy_function(f: &Rc<crate::ast::Function>) {
    with_gc_state(|state| state.lazy_fns.borrow_mut().push(Rc::downgrade(f)));
}

/// Release every registered function's body that no read touched since the previous pass, and
/// drop the entries of functions that died. A flat pass over the registry with no allocation:
/// the registry is one `Weak` per lazily-parsed function in the program (~100k for a large
/// bundle), and each entry costs a strong-count check plus one flag test. Returns the number of
/// bodies released.
pub(crate) fn flush_cold_lazy_bodies() -> usize {
    with_gc_state(|state| {
        let mut reg = state.lazy_fns.borrow_mut();
        let mut released = 0;
        reg.retain(|w| match w.upgrade() {
            Some(f) => {
                if f.release_cold_body() {
                    released += 1;
                }
                true
            }
            None => false,
        });
        released
    })
}

pub(crate) fn lazy_function_registry_len() -> usize {
    with_gc_state(|state| state.lazy_fns.borrow().len())
}

/// The element type of a TypedArray.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TaKind {
    I8,
    U8,
    U8Clamped,
    I16,
    U16,
    I32,
    U32,
    F16,
    F32,
    F64,
    I64,
    U64,
}

impl TaKind {
    pub(crate) fn elsize(self) -> usize {
        match self {
            TaKind::I8 | TaKind::U8 | TaKind::U8Clamped => 1,
            TaKind::I16 | TaKind::U16 | TaKind::F16 => 2,
            TaKind::I32 | TaKind::U32 | TaKind::F32 => 4,
            TaKind::F64 | TaKind::I64 | TaKind::U64 => 8,
        }
    }
    /// Whether elements are BigInt (BigInt64Array / BigUint64Array) rather than Number.
    pub(crate) fn is_bigint(self) -> bool {
        matches!(self, TaKind::I64 | TaKind::U64)
    }
    /// Constructor / prototype name, e.g. "Int8Array".
    pub(crate) fn name(self) -> &'static str {
        match self {
            TaKind::I8 => "Int8Array",
            TaKind::U8 => "Uint8Array",
            TaKind::U8Clamped => "Uint8ClampedArray",
            TaKind::I16 => "Int16Array",
            TaKind::U16 => "Uint16Array",
            TaKind::I32 => "Int32Array",
            TaKind::U32 => "Uint32Array",
            TaKind::F16 => "Float16Array",
            TaKind::F32 => "Float32Array",
            TaKind::F64 => "Float64Array",
            TaKind::I64 => "BigInt64Array",
            TaKind::U64 => "BigUint64Array",
        }
    }
    /// Read a BigInt element (little-endian) from `b` (8 bytes) as an i128.
    pub(crate) fn read_bigint(self, b: &[u8]) -> i128 {
        let arr = [b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]];
        match self {
            TaKind::U64 => u64::from_le_bytes(arr) as i128,
            _ => i64::from_le_bytes(arr) as i128,
        }
    }
    /// Convert a BigInt (i128) to this element's 8 little-endian bytes, wrapping mod 2^64.
    pub(crate) fn write_bigint(self, n: i128) -> Vec<u8> {
        (n as u64).to_le_bytes().to_vec()
    }
    /// Read one element (little-endian) from `b` (which must be `elsize()` bytes) as a Number.
    pub(crate) fn read(self, b: &[u8]) -> f64 {
        match self {
            TaKind::I8 => b[0] as i8 as f64,
            TaKind::U8 | TaKind::U8Clamped => b[0] as f64,
            TaKind::I16 => i16::from_le_bytes([b[0], b[1]]) as f64,
            TaKind::U16 => u16::from_le_bytes([b[0], b[1]]) as f64,
            TaKind::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            TaKind::U32 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            TaKind::F16 => f16_to_f32(u16::from_le_bytes([b[0], b[1]])) as f64,
            TaKind::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
            TaKind::F64 => f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]),
            TaKind::I64 | TaKind::U64 => self.read_bigint(b) as f64,
        }
    }
    /// Convert a Number to this element type's little-endian bytes (JS integer-conversion rules).
    pub(crate) fn write(self, n: f64) -> Vec<u8> {
        let int = |n: f64| if n.is_finite() { n.trunc() as i64 } else { 0 };
        match self {
            TaKind::I8 => vec![int(n) as i8 as u8],
            TaKind::U8 => vec![int(n) as u8],
            TaKind::U8Clamped => {
                // ToUint8Clamp: round-half-to-even (0.5 → 0, 1.5 → 2, 2.5 → 2), clamped to [0,255].
                let c = if n.is_nan() || n <= 0.0 {
                    0.0
                } else if n >= 255.0 {
                    255.0
                } else {
                    let f = n.floor();
                    if f + 0.5 < n {
                        f + 1.0
                    } else if n < f + 0.5 {
                        f
                    } else if (f as i64) % 2 == 1 {
                        f + 1.0
                    } else {
                        f
                    }
                };
                vec![c as u8]
            }
            TaKind::I16 => (int(n) as i16).to_le_bytes().to_vec(),
            TaKind::U16 => (int(n) as u16).to_le_bytes().to_vec(),
            TaKind::I32 => (int(n) as i32).to_le_bytes().to_vec(),
            TaKind::U32 => (int(n) as u32).to_le_bytes().to_vec(),
            TaKind::F16 => f64_to_f16(n).to_le_bytes().to_vec(),
            TaKind::F32 => (n as f32).to_le_bytes().to_vec(),
            TaKind::F64 => n.to_le_bytes().to_vec(),
            TaKind::I64 | TaKind::U64 => self.write_bigint(int(n) as i128),
        }
    }
}

/// A TypedArray view's internal state (the engine's `[[ViewedArrayBuffer]]`/`[[ByteOffset]]`/
/// `[[ArrayLength]]`/`[[TypedArrayName]]`). Stored in an `Interp` side table keyed by object ptr.
#[derive(Clone, Copy)]
pub struct TaInfo {
    /// Pointer of the backing ArrayBuffer object (key into `Interp::array_buffers`).
    pub buffer: usize,
    pub offset: usize,
    pub len: usize,
    pub kind: TaKind,
    /// Length-tracking view (created on a resizable buffer with no explicit length): its length is
    /// recomputed from the buffer's current size rather than fixed at `len`.
    pub track: bool,
}

/// How a property key relates to a TypedArray's integer-indexed exotic behavior.
pub enum TaIndex {
    /// A valid in-range element index.
    Element(usize),
    /// A canonical numeric key that isn't a valid index (inert: get→undefined, set/define→no-op,
    /// has→false, delete→true; never stored, never reaches the prototype).
    Exotic,
    /// An ordinary string/symbol key (handled by the normal property machinery).
    Ordinary,
}

/// A property descriptor. A data property uses `value`/`writable`; an accessor uses the boxed
/// getter/setter pair. The low bits of `meta` hold the four descriptor flags; its aligned upper
/// bits point to an accessor pair only for accessor properties. Thus ordinary properties are 32
/// bytes and allocate no metadata.
pub struct Property {
    packed: PackedValue,
    meta: usize,
}

/// The boxed getter/setter pair of an accessor property.
#[repr(align(16))]
#[derive(Clone, Default)]
pub(crate) struct Accessors {
    pub get: Option<Value>,
    pub set: Option<Value>,
}

pub(crate) const PROP_ACCESSOR: usize = 1;
pub(crate) const PROP_WRITABLE: usize = 2;
pub(crate) const PROP_ENUMERABLE: usize = 4;
pub(crate) const PROP_CONFIGURABLE: usize = 8;
const PROP_FLAG_MASK: usize = 15;

impl Clone for Property {
    fn clone(&self) -> Self {
        let flags = self.meta & PROP_FLAG_MASK;
        let ptr = self.meta & !PROP_FLAG_MASK;
        let meta = if ptr == 0 {
            flags
        } else {
            let acc = unsafe { &*(ptr as *const Accessors) };
            Box::into_raw(Box::new(acc.clone())) as usize | flags
        };
        Property {
            packed: self.packed.clone(),
            meta,
        }
    }
}

impl Drop for Property {
    fn drop(&mut self) {
        let ptr = self.meta & !PROP_FLAG_MASK;
        if ptr != 0 {
            unsafe { drop(Box::from_raw(ptr as *mut Accessors)) };
        }
    }
}

impl Property {
    pub(crate) fn data(
        value: Value,
        writable: bool,
        enumerable: bool,
        configurable: bool,
    ) -> Property {
        let meta = (writable as usize) * PROP_WRITABLE
            | (enumerable as usize) * PROP_ENUMERABLE
            | (configurable as usize) * PROP_CONFIGURABLE;
        Property {
            packed: PackedValue::pack(value),
            meta,
        }
    }
    /// An accessor property (`accessor: true`, value `Undefined`, not writable).
    pub(crate) fn accessor_prop(
        get: Option<Value>,
        set: Option<Value>,
        enumerable: bool,
        configurable: bool,
    ) -> Property {
        let flags = PROP_ACCESSOR
            | (enumerable as usize) * PROP_ENUMERABLE
            | (configurable as usize) * PROP_CONFIGURABLE;
        let ptr = Box::into_raw(Box::new(Accessors { get, set })) as usize;
        debug_assert_eq!(ptr & PROP_FLAG_MASK, 0);
        Property {
            packed: PackedValue::pack(Value::Undefined),
            meta: ptr | flags,
        }
    }
    #[inline]
    fn accessors(&self) -> Option<&Accessors> {
        let ptr = self.meta & !PROP_FLAG_MASK;
        (ptr != 0).then(|| unsafe { &*(ptr as *const Accessors) })
    }
    #[inline]
    fn accessors_mut(&mut self) -> Option<&mut Accessors> {
        let ptr = self.meta & !PROP_FLAG_MASK;
        (ptr != 0).then(|| unsafe { &mut *(ptr as *mut Accessors) })
    }
    #[inline]
    pub(crate) fn accessor(&self) -> bool {
        self.meta & PROP_ACCESSOR != 0
    }
    #[inline]
    pub(crate) fn writable(&self) -> bool {
        self.meta & PROP_WRITABLE != 0
    }
    #[inline]
    pub(crate) fn enumerable(&self) -> bool {
        self.meta & PROP_ENUMERABLE != 0
    }
    #[inline]
    pub(crate) fn configurable(&self) -> bool {
        self.meta & PROP_CONFIGURABLE != 0
    }
    fn set_flag(&mut self, flag: usize, value: bool) {
        if value {
            self.meta |= flag;
        } else {
            self.meta &= !flag;
        }
    }
    pub(crate) fn set_accessor(&mut self, value: bool) {
        if !value {
            self.clear_accessors();
        }
        self.set_flag(PROP_ACCESSOR, value);
    }
    pub(crate) fn set_writable(&mut self, value: bool) {
        self.set_flag(PROP_WRITABLE, value);
    }
    pub(crate) fn set_enumerable(&mut self, value: bool) {
        self.set_flag(PROP_ENUMERABLE, value);
    }
    pub(crate) fn set_configurable(&mut self, value: bool) {
        self.set_flag(PROP_CONFIGURABLE, value);
    }
    pub(crate) fn into_value(mut self) -> Value {
        self.take_value()
    }
    #[inline]
    pub(crate) fn value(&self) -> Value {
        self.packed.unpack()
    }
    #[inline]
    pub(crate) fn set_value(&mut self, value: Value) {
        self.packed = PackedValue::pack(value);
    }
    #[inline]
    pub(crate) fn replace_value(&mut self, value: Value) -> Value {
        let old = std::mem::replace(&mut self.packed, PackedValue::pack(value));
        old.into_value()
    }
    #[inline]
    pub(crate) fn take_value(&mut self) -> Value {
        self.replace_value(Value::Undefined)
    }
    #[inline]
    pub(crate) fn getter(&self) -> Option<&Value> {
        self.accessors().and_then(|a| a.get.as_ref())
    }
    #[inline]
    pub(crate) fn setter(&self) -> Option<&Value> {
        self.accessors().and_then(|a| a.set.as_ref())
    }
    pub(crate) fn set_getter(&mut self, g: Option<Value>) {
        if let Some(a) = self.accessors_mut() {
            a.get = g;
        } else if let Some(g) = g {
            let flags = self.meta & PROP_FLAG_MASK;
            let ptr = Box::into_raw(Box::new(Accessors {
                get: Some(g),
                set: None,
            })) as usize;
            self.meta = ptr | flags;
        }
    }
    pub(crate) fn set_setter(&mut self, s: Option<Value>) {
        if let Some(a) = self.accessors_mut() {
            a.set = s;
        } else if let Some(s) = s {
            let flags = self.meta & PROP_FLAG_MASK;
            let ptr = Box::into_raw(Box::new(Accessors {
                get: None,
                set: Some(s),
            })) as usize;
            self.meta = ptr | flags;
        }
    }
    /// Drop the accessor pair (used when a define converts an accessor back to a data property).
    pub(crate) fn clear_accessors(&mut self) {
        let ptr = self.meta & !PROP_FLAG_MASK;
        if ptr != 0 {
            unsafe { drop(Box::from_raw(ptr as *mut Accessors)) };
            self.meta &= PROP_FLAG_MASK;
        }
    }
    /// A default plain data property: writable, enumerable, configurable.
    pub(crate) fn plain(value: Value) -> Property {
        Property::data(value, true, true, true)
    }
    /// A non-enumerable method/builtin property: writable + configurable, not enumerable.
    pub(crate) fn builtin(value: Value) -> Property {
        Property::data(value, true, false, true)
    }
}

pub(crate) mod gc_edges;
mod heap;
mod props;
pub(crate) use props::FnMaps;
pub use props::Props;
pub(crate) use props::{bump_proto_epoch, fn_key, proto_epoch, shape_table_census};
pub(crate) use props::{jit_props_layout, MIRROR_ALL_I32, MIRROR_HOLE, MIRROR_OK};

/// A canonical array-index property key (`"0"`, `"42"` — decimal, no leading zeros, fits u32).
#[inline(always)]
pub(crate) fn canonical_index(k: &str) -> Option<u32> {
    let bytes = k.as_bytes();
    let &first = bytes.first()?;
    // Named properties dominate. Reject them after one byte instead of setting up the full
    // iterator/parser path; this function sits in every generic property lookup.
    if !first.is_ascii_digit() {
        return None;
    }
    if first == b'0' {
        return (bytes.len() == 1).then_some(0);
    }
    if !bytes[1..].iter().all(u8::is_ascii_digit) {
        return None;
    }
    k.parse::<u32>().ok().filter(|&n| n != u32::MAX)
}

/// Convenience: define a plain own data property by key/value.
pub fn set_data(obj: &Gc, key: &str, value: Value) {
    obj.borrow_mut().props.insert(key, Property::plain(value));
}

/// Convenience: define a non-enumerable builtin property by key/value.
pub fn set_builtin(obj: &Gc, key: &str, value: Value) {
    obj.borrow_mut().props.insert(key, Property::builtin(value));
}

/// IEEE-754 half-precision (binary16) to single-precision conversion.
pub fn f16_to_f32(h: u16) -> f32 {
    let sign = (h as u32 & 0x8000) << 16;
    let exp = (h >> 10) & 0x1f;
    let mant = (h & 0x3ff) as u32;
    let bits = if exp == 0 {
        if mant == 0 {
            sign
        } else {
            // Subnormal: normalize into a single-precision normal number.
            let mut e: i32 = -1;
            let mut m = mant;
            loop {
                e += 1;
                m <<= 1;
                if m & 0x400 != 0 {
                    break;
                }
            }
            let m = m & 0x3ff;
            sign | (((127 - 15 - e) as u32) << 23) | (m << 13)
        }
    } else if exp == 0x1f {
        sign | 0x7f80_0000 | (mant << 13)
    } else {
        sign | (((exp as u32) + 127 - 15) << 23) | (mant << 13)
    };
    f32::from_bits(bits)
}

/// IEEE-754 double-precision to half-precision (binary16), round-to-nearest-even, rounding **once**.
/// Going through `f32` first would double-round — e.g. `2^-25 + ε` collapses to an exact tie at
/// `f32` and then rounds to zero instead of up to the smallest subnormal.
pub fn f64_to_f16(value: f64) -> u16 {
    let x = value.to_bits();
    let sign = ((x >> 48) & 0x8000) as u16;
    let exp = ((x >> 52) & 0x7ff) as i32;
    let mant = x & 0x000f_ffff_ffff_ffff; // 52-bit fraction
    if exp == 0x7ff {
        return if mant != 0 {
            sign | 0x7e00 // NaN
        } else {
            sign | 0x7c00 // infinity
        };
    }
    if exp == 0 && mant == 0 {
        return sign; // signed zero
    }
    let half_exp = exp - 1023 + 15;
    if half_exp >= 0x1f {
        return sign | 0x7c00; // overflow → infinity
    }
    if half_exp <= 0 {
        // Subnormal half (or underflow to zero). Drop the low bits of the full significand,
        // rounding to nearest even. `exp == 0` doubles are far below f16 range → they fall out as 0.
        let m = if exp == 0 { mant } else { mant | (1u64 << 52) };
        let shift = 43 - half_exp; // 52-bit fraction → 10-bit fraction, minus the exponent deficit
        if shift >= 64 {
            return sign;
        }
        let mut h = (m >> shift) as u16;
        let round_bit = (m >> (shift - 1)) & 1;
        let sticky = (m & ((1u64 << (shift - 1)) - 1)) != 0;
        if round_bit != 0 && (sticky || (h & 1) != 0) {
            h += 1;
        }
        return sign | h;
    }
    let mut h = (((half_exp as u32) << 10) | ((mant >> 42) as u32)) as u16;
    let round_bit = (mant >> 41) & 1;
    let sticky = (mant & ((1u64 << 41) - 1)) != 0;
    if round_bit != 0 && (sticky || (h & 1) != 0) {
        h = h.wrapping_add(1); // carry into exponent is intentional
    }
    sign | h
}

// ----- layout facts for the optimizing tier's inline reads (`bytecode::jit::layout`) ---------

/// Byte offset of a [`Property`]'s NaN-boxed value word.
pub(crate) const PROPERTY_PACKED_OFFSET: usize = std::mem::offset_of!(Property, packed);
/// Byte offset of a [`Property`]'s flag / accessor-pointer word.
pub(crate) const PROPERTY_META_OFFSET: usize = std::mem::offset_of!(Property, meta);

/// `(object, borrow flag)`: byte offsets from a `Gc` handle word (the `GcBox` address) to the
/// `Object` inside its `RefCell` and to that `RefCell`'s borrow counter (an `isize`: 0 = free,
/// > 0 = shared borrows, < 0 = mutably borrowed). std's `RefCell` field order is not public, so
/// both are measured once from a real cell; `None` when the probe is inconclusive.
pub(crate) fn jit_gc_offsets() -> Option<(usize, usize)> {
    static OFFSETS: std::sync::OnceLock<Option<(usize, usize)>> = std::sync::OnceLock::new();
    *OFFSETS.get_or_init(|| {
        // Never dropped: `Object::drop` would decrement a live count this object never joined.
        // Its parts own no allocation (empty props, no call target), so nothing leaks.
        let cell = std::mem::ManuallyDrop::new(RefCell::new(Object {
            proto: None,
            props: Props::new(),
            call: Callable::None,
            exotic: Exotic::None,
            extensible: true,
            ic_plain: Cell::new(true),
            is_constructor: false,
            gc_internal: Cell::new(0),
        }));
        let base = &*cell as *const RefCell<Object> as usize;
        let value = cell.as_ptr() as usize - base;
        const W: usize = std::mem::size_of::<isize>();
        let words = std::mem::size_of::<RefCell<Object>>() / W;
        let read = |w: usize| unsafe { std::ptr::read_volatile((base + w * W) as *const isize) };
        let outside =
            |w: usize| w * W + W <= value || w * W >= value + std::mem::size_of::<Object>();
        let free: Vec<usize> = (0..words).filter(|&w| outside(w) && read(w) == 0).collect();
        let shared: Vec<usize> = {
            let _r = cell.borrow();
            free.iter().copied().filter(|&w| read(w) == 1).collect()
        };
        let excl: Vec<usize> = {
            let _w = cell.borrow_mut();
            shared.iter().copied().filter(|&w| read(w) < 0).collect()
        };
        match excl[..] {
            [w] => Some((GC_VALUE_OFFSET + value, GC_VALUE_OFFSET + w * W)),
            _ => None,
        }
    })
}
