//! Lumen's language-neutral native-binding framework.
//!
//! Every native in every Lumen language is a plain Rust fn or struct, declared once with these
//! attributes; each engine (JS in `lumen`, Python in `lumen-py`) implements [`Host`] once and
//! exposes every declaration. The macros know no language: they emit a neutral [`FnDesc`] /
//! [`ClassDesc`] / [`ModuleDesc`] and a thunk generic over `H: Host`.
//!
//! ```ignore
//! use lumen_bind::{NativeError, NativeResult};
//!
//! #[lumen_bind::module(name = "geometry")]
//! pub mod geometry {
//!     use super::*;
//!
//!     /// Exposed to every host: JS `geometry.clamp(x, lo, hi)`, Python `geometry.clamp(x, lo, hi)`.
//!     #[op]
//!     pub fn clamp(x: f64, lo: f64, hi: f64) -> f64 {
//!         x.max(lo).min(hi)
//!     }
//!
//!     /// Positional-only `a`, `b`; keyword-only `rel_tol` with a default.
//!     #[op]
//!     pub fn close(a: f64, b: f64, #[kwonly] #[default(1e-09)] rel_tol: f64) -> bool {
//!         (a - b).abs() <= rel_tol * a.abs().max(b.abs())
//!     }
//!
//!     #[class]
//!     pub struct Point { x: f64, y: f64 }
//!
//!     #[methods]
//!     impl Point {
//!         #[constructor]
//!         fn new(x: f64, y: f64) -> Point { Point { x, y } }
//!         #[getter]
//!         fn x(&self) -> f64 { self.x }
//!         fn scale(&mut self, k: f64) { self.x *= k; self.y *= k; }
//!         #[proto(repr)]
//!         fn repr(&self) -> String { format!("Point({}, {})", self.x, self.y) }
//!     }
//! }
//! // JS:     ctx.module_object::<geometry::Module>()  (or engine.define_module)
//! // Python: lumen_py's native module table lists `geometry::Module`.
//! ```
//!
//! # Declarations
//! - `#[op(options)]` on a fn: generates a module of the same name with `Op` (implements
//!   [`Native`]) and `DESC`.
//! - `#[class(options)]` on a struct + `#[methods]` on its impl block. Members: `#[constructor]`,
//!   `#[getter]`, `#[setter]` (`set_x` sets `x`), `#[proto(name)]` (see [`PROTOCOLS`]),
//!   `#[method(options)]` (plain methods need no attribute), `#[classmethod]` (receives the
//!   class as `This<_>`), `#[skip]`. A member without a receiver (`&self`, `&mut self`,
//!   `This<_>`) is static.
//! - `#[module(options)]` on an inline `mod`: every `#[op]`, `#[class]`, `#[constant(name = ..)]`
//!   (a `const`; `enumerable` makes the property enumerable, as a Web IDL attribute of the global
//!   object is) and `#[init]` (`fn(&mut Ctx, &Value)`, runs once on the new module object, for
//!   host-specific extras such as exception types) inside is registered by one declaration
//!   (`<mod>::Module`). A constant's value may be an instance of a class of the same module
//!   (`const CRYPTO: Crypto = Crypto;`); hosts register the module's classes first.
//! - `#[constant(name = ..)]` on an associated `const` inside a `#[methods]` impl: a class
//!   constant (a Web IDL `const`). The JS host defines it on the constructor and on the
//!   prototype, enumerable, non-writable and non-configurable. The Python host does not read
//!   class constants ([`Methods::constants`] is empty for it unless it implements it).
//!
//! Options: `name = ".."` (verbatim for every host), `rename(js = "..", py = "..")`,
//! `only(js, ..)` / `skip(py, ..)` (rare), `coerce` (the host's lenient conversions), `async`
//! (run off the script thread, return a promise; [`SpawnHost`] hosts only),
//! `hint(py(key = "..", flag))`: opaque per-host data the macro passes through unread ([`Hints`]).
//! Classes also take `module = ".."` (the public module), `generic` and `extends = Base`.
//! `skip(host)` on a class keeps it usable as a value type while hiding it from that host's
//! module listing.
//!
//! `extends = Base` makes the class a subclass of another `#[class]`: the struct must hold the
//! base instance in a field named `base`, and receivers of the base's methods accept the
//! subclass. The JS host links the class to the base class (prototype chain, `instanceof`);
//! the Python host does not read it yet and uses `hint(py(base = ".."))` instead. Example: `DomPermissionStatus` (`extends = DomEventTarget`) in
//! `lumen-html-js/src/browser_services.rs`.
//!
//! The macros validate only the shape of a hint (`flag` or `key = "value"`); the keys are the
//! host's. The Python host reads, on an op / member: `text_signature = ".."` (`""`: no
//! `__text_signature__`), `arg_style = "parse"` / `"unpack"` (`PyArg_ParseTuple` /
//! `PyArg_UnpackTuple` error wording), `arg_name = ".."` (the name in argument errors),
//! `aliases = "a, b"` (extra names for the same native);
//! on a class: `unhashable` (`__hash__ = None`), `native_iter` (the constructor returns a step
//! closure the VM drives), `final` (no subclasses), `base = "module.Class"` (a Python base
//! class) and `shared` (members installed into several core types). See `lumen_py::bind::args` and `lumen_py::bind::class`.
//! The JS host recognizes `hint(js(webidl))` on a class to make named operations
//! and attributes enumerable as required by Web IDL; ordinary native classes
//! keep JavaScript class descriptors. Symbol iteration hooks stay non-enumerable. On an `#[op]`
//! the same hint makes the installed property enumerable (a Web IDL operation of the global
//! object such as `atob`). Further JS hints:
//! - `hint(js(symbol_for = "key"))` on an instance method installs it (non-enumerable) under the
//!   registry symbol `Symbol.for("key")` instead of a string name
//!   (`nodejs.util.inspect.custom`);
//! - `hint(js(missing_message = "..", missing_code = ".."))` on an op or member replaces the
//!   `TypeError` a missing required argument throws with that message and `err.code`.
//!
//! # Lazy globals (JS host)
//! `ctx.install_module_lazy::<M>()` (`Engine::define_lazy_globals`, `lumen_host::lazy_globals`)
//! publishes everything a module declares as globals that are built on first access: each name
//! is an accessor on the global object (enumerable only for a `webidl` op or an `enumerable`
//! constant, configurable) whose getter creates the class, function or constant, replaces the
//! accessor with the data property (writable, configurable, same enumerability, so an interface
//! object has the descriptor of an eager one) and returns it; a setter replaces it with the
//! assigned value. A name the realm already defines is left alone, `#[init]` runs immediately,
//! and a script that redefines or deletes the accessor first is never overwritten.
//!
//! Without `name`/`rename`, each host derives its own name (JS camelCases, Python keeps
//! `snake_case`), its own arity / `length`, `__text_signature__` and argument-error wording.
//!
//! # Parameters
//! Named parameters are positional-only unless marked `#[kw]` (positional or keyword) or
//! `#[kwonly]`. `#[default(expr)]` gives a default; a trailing `Option<T>` is optional (`None`), and so is a
//! trailing `Passed<T>`, which also tells an omitted argument from an explicit `None` / `null`.
//! `#[varargs] rest: Vec<T>` / `&[Value]` takes the remaining positional arguments,
//! `#[varkw] kw: Vec<(String, T)>` the extra keywords. Injected (not script arguments):
//! `&mut Ctx` (the engine context; ties the fn to the host whose `Host::Ctx` it is),
//! `This<T>` (the receiver), `&State<T>` / `&mut State<T>` ([`StateHost`]).
//!
//! # Types
//! Neutral types convert in every host through one definition each ([`convert`]): integers
//! (checked or wrapped as the host defines), [`BigInt`], `f64`, `bool`, `&str` / `String` /
//! `Cow<str>`, byte buffers (`&[u8]` and `&mut [u8]` zero-copy, `Vec<u8>` copied / adopted),
//! `Option`, `Vec`, tuples, `#[class]` types (`&T`, `&mut T`, by value as results), and
//! `Result<T, E>` with `E` = [`NativeError`] (mapped once per host), `String` or `io::Error`.
//! A host's own value types (JS `Value`, Python `Obj`) pass through for that host only.
//!
//! # Porting a hand-written native (Python `crates/lumen-py/src/builtins/*`)
//! 1. Find the module's natives in its name table (`("floor", math_floor)`) and its
//!    registration in `builtins/modules.rs`.
//! 2. Wrap the module in `#[lumen_bind::module(name = "math")] pub mod math { use super::*; .. }`
//!    and delete the name table; register `math::Module` in the native module list instead.
//! 3. For each native, replace `fn f(it: &mut Interp, a: &[Value], kw: &[(Obj, Value)]) -> R<Value>`
//!    with a typed fn: one parameter per argument with its Rust type (`x: f64`, `s: &str`,
//!    `data: &[u8]`, `n: i64`, `v: &Value` when it really is any object). Delete the manual
//!    arity checks, `kw.is_empty()` checks and conversions: the host derives CPython's exact
//!    messages (`floor() takes exactly one argument (2 given)`, `f() argument 'x' must be str,
//!    not int`) from the declaration.
//! 4. Mirror CPython's signature: positional-only by default; `#[kw]` / `#[kwonly]` for
//!    keyword-capable parameters, `#[default(..)]` with CPython's default as a Rust literal
//!    (it is also shown in `__text_signature__`), `Option<T>` for `=None`.
//! 5. Take `it: &mut Ctx` only when the fn calls back into Python; return
//!    `NativeResult<T>` / `Result<T, NativeError>` for neutral errors and `R<T>` for Python
//!    exceptions already raised.
//! 6. Classes: `#[class(module = "collections", name = "deque")]` on the state struct,
//!    `#[methods]` on its impl; dunders become `#[proto(len)]`, `#[proto(getitem)]`, ...;
//!    `__new__` becomes `#[constructor]`; methods that call back into Python take
//!    `slf: This<Py<Self>>` and borrow the state only around Rust-side access.
//! 7. Shared concepts get one core: if the logic already exists for JS (or belongs to
//!    `lumen-common`), move the language-neutral part there and bind it from both.
//! 8. Byte buffers: `&[u8]` / `&mut [u8]` borrow `bytes`, `bytearray` and contiguous
//!    `memoryview`s without copying. Natives that keep a buffer across calls hold an export of
//!    its [`lumen_common::buffer::ByteStore`]; while one is alive, resizing raises
//!    `BufferError: Existing exports of data: object cannot be re-sized`.
//! 9. Run the corpus (`cargo test --release -p lumen-py --test corpus`) and compare error
//!    messages with `python3`.
//!
//! # Implementing a host
//! Implement [`Host`]: value / error / context types, the argument primitives (`to_f64`,
//! `to_str`, `to_bytes`, ...), the result primitives (`from_f64`, `from_list`, ...), `bind`
//! (the host's calling convention and its arity errors), class borrowing and registration.
//! Add `FromArg` / `IntoRet` impls for the host's own value types. Nothing in the macros
//! changes for a new host.

mod convert;
mod desc;
mod host;
mod trace;

pub use convert::{
    CtorRet, Elem, Flag, FromArg, FromRest, FromVarKw, IntoError, IntoRet, Lenient, NextRet,
    OneOrNumberPair, Passed,
};
pub use desc::{
    camel_case, flags, setter_property, ClassDesc, CodePtr, FnDesc, Hints, ModuleDesc, Owner,
    Param, ParamKind, Role, Scalar, ScalarEntry, Slot, CLASS_GENERIC, PROTOCOLS,
};
pub use host::{
    Class, ClassItem, ConstItem, FnItem, Host, Inheritance, IntKind, Make, Methods, Module,
    ModuleItems, Native, SpawnHost, State, StateHost, This,
};
pub use lumen_bind_macros::{class, methods, module, op};
pub use lumen_common::bigint::BigInt;
pub use lumen_common::native::{Data, ErrorKind, NativeError, NativeResult};
pub use trace::{Trace, Visit};

/// Support for generated code. Not a stable API.
#[doc(hidden)]
pub mod __private {
    use super::*;
    pub use crate::host::__this as this;

    /// Autoref probes behind the `gc_trace` / `gc_clear` a `#[class]` generates: a field whose
    /// type implements [`Trace`] is reported, any other field is skipped (no trait bound on the
    /// struct's field types).
    pub struct Probe<'a, T: ?Sized>(pub &'a T);
    pub struct ProbeMut<'a, T: ?Sized>(pub &'a mut T);

    pub trait ViaTrace {
        fn __lumen_trace(&self, v: &mut dyn Visit);
    }
    impl<T: Trace + ?Sized> ViaTrace for Probe<'_, T> {
        #[inline]
        fn __lumen_trace(&self, v: &mut dyn Visit) {
            self.0.trace(v)
        }
    }
    pub trait ViaNone {
        #[inline]
        fn __lumen_trace(&self, _v: &mut dyn Visit) {}
    }
    impl<T: ?Sized> ViaNone for &Probe<'_, T> {}

    pub trait ViaTraceMut {
        fn __lumen_clear(&mut self);
    }
    impl<T: Trace + ?Sized> ViaTraceMut for ProbeMut<'_, T> {
        #[inline]
        fn __lumen_clear(&mut self) {
            self.0.clear()
        }
    }
    pub trait ViaNoneMut {
        #[inline]
        fn __lumen_clear(&mut self) {}
    }
    impl<T: ?Sized> ViaNoneMut for &mut ProbeMut<'_, T> {}

    #[inline(always)]
    pub fn arg<'a, H: Host, T: FromArg<'a, H>>(
        cx: &'a H::Cx<'_>,
        v: Option<&'a H::Value>,
        at: Slot,
    ) -> Result<T, H::Error> {
        match v {
            Some(v) => T::from_arg(cx, v, at),
            None => T::from_missing(cx, at),
        }
    }

    #[inline(always)]
    pub fn rest<'a, H: Host, T: FromRest<'a, H>>(
        cx: &'a H::Cx<'_>,
        first: u32,
    ) -> Result<T, H::Error> {
        T::from_rest(cx, H::rest(cx), first)
    }

    #[inline(always)]
    pub fn varkw<'a, H: Host, T: FromVarKw<'a, H>>(cx: &'a H::Cx<'_>) -> Result<T, H::Error> {
        T::from_varkw(cx)
    }

    #[inline(always)]
    pub fn ctor<H: Host, T: Class, R: CtorRet<H, T>>(
        cx: &H::Cx<'_>,
        r: R,
    ) -> Result<H::Value, H::Error> {
        r.into_ctor(cx)
    }

    /// `CALLBACKS` bit of named parameter `i`.
    pub const fn callback_bit(is_callback: bool, i: u32) -> u32 {
        if is_callback && i < 32 {
            1 << i
        } else {
            0
        }
    }

    pub fn constant<H: Host, T: IntoRet<H>>(ctx: &mut H::Ctx, v: T) -> Result<H::Value, H::Error> {
        v.into_ret(ctx)
    }
}
