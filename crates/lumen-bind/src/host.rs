//! The traits an engine implements once ([`Host`], optionally [`StateHost`] / [`SpawnHost`]) and
//! the traits the macros implement per declaration ([`Native`], [`Class`], [`Methods`],
//! [`Module`]).

use crate::convert::{FromArg, IntoRet, NextRet};
use crate::desc::{ClassDesc, FnDesc, ModuleDesc, Slot};
use lumen_common::bigint::BigInt;
use lumen_common::native::NativeError;

/// The kind of a Rust integer parameter, for [`Host::to_int`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntKind {
    pub bits: u8,
    pub signed: bool,
    /// `isize` / `usize` (hosts with C-style messages name them differently).
    pub size: bool,
}

impl IntKind {
    pub const fn new(bits: u8, signed: bool, size: bool) -> IntKind {
        IntKind { bits, signed, size }
    }
    pub fn min(self) -> i128 {
        if self.signed {
            -(1i128 << (self.bits - 1))
        } else {
            0
        }
    }
    pub fn max(self) -> i128 {
        if self.signed {
            (1i128 << (self.bits - 1)) - 1
        } else {
            (1i128 << self.bits) - 1
        }
    }
}

/// A script engine the binding framework can expose natives to. Implemented once per engine;
/// every `#[op]`, `#[class]` and `#[module]` then works with it, provided the engine can convert
/// the types the declaration uses (a declaration taking one engine's value type is exposed to
/// that engine only: the gating is the trait bounds, nothing else).
///
/// Argument conversions take the per-call context `cx` and the [`Slot`] the value came from,
/// so the host words errors its own way. Result conversions take the engine context `ctx`
/// (they also run outside calls, e.g. when an async result settles).
pub trait Host: Sized + 'static {
    /// The name `only(..)`, `skip(..)` and `rename(..)` refer to (`"js"`, `"py"`).
    const NAME: &'static str;
    /// A script value.
    type Value: 'static;
    /// What a failing native returns (a thrown value / an exception).
    type Error;
    /// The engine context a native may take as `&mut Ctx`.
    type Ctx;
    /// The per-call context: the arguments and whatever a borrowed argument needs to stay valid.
    type Cx<'s>;
    /// A native function pointer of this engine.
    type Entry: Copy + 'static;

    /// The engine entry point that runs `N`.
    fn entry<N: Native<Self>>() -> Self::Entry;

    // ---- the call ------------------------------------------------------------------------------

    /// The named parameters' arguments, in declaration order (`None`: not passed). Hosts with
    /// keyword arguments bind them here and raise their own arity / keyword errors.
    fn bind<'c, const N: usize>(
        cx: &'c Self::Cx<'_>,
    ) -> Result<[Option<&'c Self::Value>; N], Self::Error>;
    /// The positional arguments past the named ones (`*args`).
    fn rest<'c>(cx: &'c Self::Cx<'_>) -> &'c [Self::Value];
    /// Keyword arguments not bound to a named parameter (`**kwargs`).
    fn varkw<'c>(cx: &'c Self::Cx<'_>) -> Vec<(&'c str, &'c Self::Value)>;
    /// The receiver (`this` / `self`); [`Host::absent`] when there is none.
    fn this<'c>(cx: &'c Self::Cx<'_>) -> &'c Self::Value;
    /// What a missing argument converts from (only reached by hosts whose binder lets a
    /// required argument be omitted: JS `undefined`).
    fn absent() -> &'static Self::Value;
    /// Runs `f` with the engine context (the `&mut Ctx` parameter). Borrowed arguments stay
    /// valid while `f` runs script code.
    fn with_ctx<R>(cx: &Self::Cx<'_>, f: impl FnOnce(&mut Self::Ctx) -> R) -> R;
    /// Converts the fn's result.
    fn ret<R: IntoRet<Self>>(cx: &Self::Cx<'_>, r: R) -> Result<Self::Value, Self::Error>;
    /// Converts the result of a `next` protocol method.
    fn ret_next<R: NextRet<Self>>(cx: &Self::Cx<'_>, r: R) -> Result<Self::Value, Self::Error>;
    /// Finishes a constructor: the instance holding `value`.
    fn construct<T: Class>(cx: &Self::Cx<'_>, value: T) -> Result<Self::Value, Self::Error>;

    // ---- arguments -------------------------------------------------------------------------------

    /// The host's "no value" (`None` / `null` / `undefined`): an `Option` argument's `None`.
    fn is_none(v: &Self::Value) -> bool;
    fn to_f64(cx: &Self::Cx<'_>, v: &Self::Value, at: Slot) -> Result<f64, Self::Error>;
    /// An integer of `kind`, checked or wrapped as the host defines.
    fn to_int(
        cx: &Self::Cx<'_>,
        v: &Self::Value,
        at: Slot,
        kind: IntKind,
    ) -> Result<i128, Self::Error>;
    fn to_bigint(cx: &Self::Cx<'_>, v: &Self::Value, at: Slot) -> Result<BigInt, Self::Error>;
    fn to_bool(cx: &Self::Cx<'_>, v: &Self::Value, at: Slot) -> Result<bool, Self::Error>;
    /// Whether `v` is the host's own `true` (no truthiness coercion).
    fn is_true(v: &Self::Value) -> bool;
    fn to_str<'c>(
        cx: &'c Self::Cx<'_>,
        v: &'c Self::Value,
        at: Slot,
    ) -> Result<&'c str, Self::Error>;
    /// The bytes of a byte buffer, borrowed (zero-copy) until the call returns.
    fn to_bytes<'c>(
        cx: &'c Self::Cx<'_>,
        v: &'c Self::Value,
        at: Slot,
    ) -> Result<&'c [u8], Self::Error>;
    /// The bytes of a writable byte buffer, borrowed exclusively until the call returns.
    // Exclusivity comes from the borrow guard the host parks in `cx`, not from `&mut cx`.
    #[allow(clippy::mut_from_ref)]
    fn to_bytes_mut<'c>(
        cx: &'c Self::Cx<'_>,
        v: &'c Self::Value,
        at: Slot,
    ) -> Result<&'c mut [u8], Self::Error>;
    fn to_byte_vec(cx: &Self::Cx<'_>, v: &Self::Value, at: Slot) -> Result<Vec<u8>, Self::Error>;
    /// A byte-slice argument at its position in argument order, before later arguments convert
    /// (and possibly run script code); the slice itself comes later from `to_bytes` /
    /// `to_bytes_mut`. Python exports the buffer here, as CPython does while parsing arguments.
    #[inline(always)]
    fn reserve_bytes(cx: &Self::Cx<'_>, v: &Self::Value, at: Slot) -> Result<(), Self::Error> {
        let _ = (cx, v, at);
        Ok(())
    }
    /// The elements of a sequence, held by `cx` until the call returns.
    fn to_seq<'c>(
        cx: &'c Self::Cx<'_>,
        v: &'c Self::Value,
        at: Slot,
    ) -> Result<&'c [Self::Value], Self::Error>;
    /// A `#[class]` instance, borrowed shared until the call returns.
    fn class_ref<'c, T: Class>(
        cx: &'c Self::Cx<'_>,
        v: &'c Self::Value,
        at: Slot,
    ) -> Result<&'c T, Self::Error>;
    /// A `#[class]` instance, borrowed exclusively until the call returns; a conflicting borrow
    /// (re-entrant use of the same instance) is an error, never aliasing.
    #[allow(clippy::mut_from_ref)]
    fn class_mut<'c, T: Class>(
        cx: &'c Self::Cx<'_>,
        v: &'c Self::Value,
        at: Slot,
    ) -> Result<&'c mut T, Self::Error>;

    // ---- results ---------------------------------------------------------------------------------

    /// What a fn returning `()` returns.
    fn unit(ctx: &mut Self::Ctx) -> Self::Value;
    /// `Option::None` as a result.
    fn none(ctx: &mut Self::Ctx) -> Self::Value;
    fn from_bool(ctx: &mut Self::Ctx, b: bool) -> Self::Value;
    fn from_f64(ctx: &mut Self::Ctx, x: f64) -> Self::Value;
    fn from_int(ctx: &mut Self::Ctx, n: i128) -> Result<Self::Value, Self::Error>;
    fn from_bigint(ctx: &mut Self::Ctx, n: BigInt) -> Self::Value;
    fn from_str(ctx: &mut Self::Ctx, s: &str) -> Self::Value;
    fn from_string(ctx: &mut Self::Ctx, s: String) -> Self::Value;
    /// Owned bytes (adopted, not copied, where the host can).
    fn from_bytes(ctx: &mut Self::Ctx, b: Vec<u8>) -> Self::Value;
    fn from_list(ctx: &mut Self::Ctx, items: Vec<Self::Value>) -> Self::Value;
    fn from_tuple(ctx: &mut Self::Ctx, items: Vec<Self::Value>) -> Self::Value;
    /// A new instance of `#[class]` `T` (its class must be registered with this engine).
    fn new_instance<T: Class>(ctx: &mut Self::Ctx, value: T) -> Result<Self::Value, Self::Error>;
    /// The step result of an iterator (`None`: exhausted).
    fn iter_step(
        ctx: &mut Self::Ctx,
        item: Option<Self::Value>,
    ) -> Result<Self::Value, Self::Error>;
    fn error(ctx: &mut Self::Ctx, e: NativeError) -> Self::Error;

    // ---- registration ------------------------------------------------------------------------------

    /// This engine's class object for `T` (created and its members installed on first use).
    fn class_object<T: Methods<Self>>(ctx: &mut Self::Ctx) -> Result<Self::Value, Self::Error>;
    /// A fresh module object holding everything `M` declares.
    fn module_object<M: Module<Self>>(ctx: &mut Self::Ctx) -> Result<Self::Value, Self::Error>;
}

/// Hosts with typed per-engine state (`&State<T>` / `&mut State<T>` parameters).
pub trait StateHost: Host {
    /// Runs `f` with the state slot `T`; an error when the embedder never installed one.
    fn with_state<T: 'static, R>(
        cx: &Self::Cx<'_>,
        f: impl FnOnce(&mut State<T>) -> R,
    ) -> Result<R, Self::Error>;
}

/// Hosts that can run a native off the script thread (`#[op(async)]`): the result settles a
/// promise / future the call returns.
pub trait SpawnHost: Host {
    fn spawn_blocking<R: IntoRet<Self> + Send + 'static>(
        cx: &Self::Cx<'_>,
        work: impl FnOnce() -> R + Send + 'static,
    ) -> Result<Self::Value, Self::Error>;
    /// The call's result: the promise from `spawn_blocking`, or an argument error as a
    /// rejected promise.
    fn async_ret(
        cx: &Self::Cx<'_>,
        r: Result<Self::Value, Self::Error>,
    ) -> Result<Self::Value, Self::Error>;
}

// ---- per-declaration traits ----------------------------------------------------------------------

/// A bindable fn (generated per `#[op]` and per class member).
pub trait Native<H: Host>: 'static {
    const DESC: &'static FnDesc;
    /// Bit `i`: named parameter `i` is a callback the fn calls only while it runs (see
    /// [`FromArg::CALLBACK`]).
    const CALLBACKS: u32 = 0;
    fn call(cx: &H::Cx<'_>) -> Result<H::Value, H::Error>;
}

/// A `#[class]` struct.
pub trait Class: 'static {
    const DESC: &'static ClassDesc;

    /// Reports the references the instance's state holds (for a host's cycle collector);
    /// `#[class]` implements it from the fields whose types implement [`crate::Trace`].
    fn gc_trace(&self, v: &mut dyn crate::Visit) {
        let _ = v;
    }

    /// Drops the references the instance's state holds, breaking the cycles it is part of.
    fn gc_clear(&mut self) {}

    fn view(&self, ty: std::any::TypeId) -> Option<&dyn std::any::Any>
    where
        Self: Sized,
    {
        (ty == std::any::TypeId::of::<Self>()).then_some(self)
    }
    fn view_mut(&mut self, ty: std::any::TypeId) -> Option<&mut dyn std::any::Any>
    where
        Self: Sized,
    {
        if ty == std::any::TypeId::of::<Self>() {
            Some(self)
        } else {
            None
        }
    }
}

/// Generated base-class registration for each host.
pub trait Inheritance<H: Host>: Class {
    fn base_class(ctx: &mut H::Ctx) -> Result<Option<H::Value>, H::Error>;
}

/// The members of a class for host `H` (generated by `#[methods]`).
pub trait Methods<H: Host>: Class {
    fn members(out: &mut Vec<FnItem<H>>);
    /// The class constants (`#[constant]` consts inside the `#[methods]` impl).
    fn constants(_out: &mut Vec<ConstItem<H>>) {}
    fn base_class(_ctx: &mut H::Ctx) -> Result<Option<H::Value>, H::Error> {
        Ok(None)
    }
}

/// A `#[module]` for host `H`.
pub trait Module<H: Host>: 'static {
    const DESC: &'static ModuleDesc;
    fn items(out: &mut ModuleItems<H>);
}

/// A fn ready to install.
pub struct FnItem<H: Host> {
    pub desc: &'static FnDesc,
    pub entry: H::Entry,
    pub callbacks: u32,
}

impl<H: Host> Clone for FnItem<H> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<H: Host> Copy for FnItem<H> {}

impl<H: Host> FnItem<H> {
    pub fn of<N: Native<H>>() -> FnItem<H> {
        FnItem {
            desc: N::DESC,
            entry: H::entry::<N>(),
            callbacks: N::CALLBACKS,
        }
    }
}

pub type Make<H> = fn(&mut <H as Host>::Ctx) -> Result<<H as Host>::Value, <H as Host>::Error>;

pub struct ClassItem<H: Host> {
    pub desc: &'static ClassDesc,
    pub object: Make<H>,
}

pub struct ConstItem<H: Host> {
    pub name: &'static str,
    pub value: Make<H>,
    /// `#[constant(.., enumerable)]` on a module constant: the property is enumerable
    /// (Web IDL attributes of the global object). Class constants ignore it.
    pub enumerable: bool,
}

/// Everything a module declares.
pub struct ModuleItems<H: Host> {
    pub functions: Vec<FnItem<H>>,
    pub classes: Vec<ClassItem<H>>,
    pub constants: Vec<ConstItem<H>>,
    /// `#[init]`: runs last, with the module object.
    pub init: Option<fn(&mut H::Ctx, &H::Value) -> Result<(), H::Error>>,
}

impl<H: Host> Default for ModuleItems<H> {
    fn default() -> Self {
        ModuleItems {
            functions: Vec::new(),
            classes: Vec::new(),
            constants: Vec::new(),
            init: None,
        }
    }
}

impl<H: Host> ModuleItems<H> {
    pub fn of<M: Module<H>>() -> ModuleItems<H> {
        let mut m = ModuleItems::default();
        M::items(&mut m);
        m
    }
}

// ---- neutral parameter types ---------------------------------------------------------------------

/// The receiver as a parameter, converted like an argument: `this: This<Value>`,
/// `slf: This<Py<Self>>`. A class member with a `This<_>` parameter is an instance member.
pub struct This<T>(pub T);

impl<T> std::ops::Deref for This<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

/// Typed per-engine state as a parameter (`&State<T>` / `&mut State<T>`), on hosts that
/// implement [`StateHost`].
#[repr(transparent)]
pub struct State<T>(T);

impl<T> State<T> {
    /// The slot as a `State` (for [`StateHost`] implementations).
    pub fn from_mut(t: &mut T) -> &mut State<T> {
        // SAFETY: `State<T>` is `repr(transparent)` over `T`.
        unsafe { &mut *(t as *mut T as *mut State<T>) }
    }
}

impl<T> std::ops::Deref for State<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> std::ops::DerefMut for State<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

#[doc(hidden)]
pub fn __this<'a, H: Host, T: FromArg<'a, H>>(cx: &'a H::Cx<'_>) -> Result<This<T>, H::Error> {
    T::from_arg(cx, H::this(cx), Slot::THIS).map(This)
}
