//! The conversion traits and their neutral impls: one definition per Rust type, written against
//! the [`Host`] primitives, so the type converts in every engine. Engine-specific types (a JS
//! `Value`, a Python `Obj`) implement the traits for their own host only.

use crate::desc::Slot;
use crate::host::{Class, Host};
use lumen_common::bigint::BigInt;
use lumen_common::native::{ErrorKind, NativeError};
use std::borrow::Cow;

/// A parameter type. `'a` is the lifetime of the call context: `&'a str`, byte slices and class
/// borrows point into the arguments or into storage the context holds until the call returns.
pub trait FromArg<'a, H: Host>: Sized {
    /// The parameter is a callback the fn calls only while it runs and never keeps; hosts may
    /// optimize the call site (JS: `SyncFn`).
    const CALLBACK: bool = false;

    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error>;

    /// The argument was not passed (and the parameter has no default).
    #[inline]
    fn from_missing(cx: &'a H::Cx<'_>, at: Slot) -> Result<Self, H::Error> {
        Self::from_arg(cx, H::absent(), at)
    }

    /// Borrowing conversions (byte slices, class references) run after every other argument,
    /// whose conversions may run script code; this runs at the argument's own position instead,
    /// before any later argument's conversion, for what must happen in argument order (a
    /// buffer export). Only called when a later argument converts early.
    #[inline(always)]
    fn reserve(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<(), H::Error> {
        let _ = (cx, v, at);
        Ok(())
    }
}

/// A `#[varargs]` parameter type.
pub trait FromRest<'a, H: Host>: Sized {
    /// `first` is the slot of the first rest argument.
    fn from_rest(cx: &'a H::Cx<'_>, vals: &'a [H::Value], first: u32) -> Result<Self, H::Error>;
}

/// A `#[varkw]` parameter type.
pub trait FromVarKw<'a, H: Host>: Sized {
    fn from_varkw(cx: &'a H::Cx<'_>) -> Result<Self, H::Error>;
}

/// A result type. `Err` values of a `Result` go through [`IntoError`].
pub trait IntoRet<H: Host> {
    /// Whether the conversion may run script code (getters, user hooks). Hosts that lend
    /// borrowed buffers out of their tables around script code use it; keep the default `true`
    /// unless the conversion only allocates.
    const MAY_RUN: bool = true;
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error>;
}

/// An error type a fn may return in `Result<T, E>`.
pub trait IntoError<H: Host> {
    fn into_error(self, ctx: &mut H::Ctx) -> H::Error;
}

/// The result of a `next` protocol method: `Option<T>` (`None` ends the iteration), or a
/// `Result` of one.
pub trait NextRet<H: Host> {
    fn into_next(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error>;
}

/// What a `#[constructor]` of class `T` may return: the class value, a `Result` of it, or a
/// host-specific instance builder (a host implements this for its own types).
pub trait CtorRet<H: Host, T> {
    fn into_ctor(self, cx: &H::Cx<'_>) -> Result<H::Value, H::Error>;
}

/// Types that may be elements of a `Vec<T>` argument or result (a list / array). Everything
/// but `u8`: a `Vec<u8>` is a byte buffer.
pub trait Elem {}

// ---- arguments ---------------------------------------------------------------------------------

impl<'a, H: Host> FromArg<'a, H> for f64 {
    #[inline(always)]
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        H::to_f64(cx, v, at)
    }
}

impl<'a, H: Host> FromArg<'a, H> for f32 {
    #[inline]
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        H::to_f64(cx, v, at).map(|x| x as f32)
    }
}

macro_rules! int_arg {
    ($($t:ty => $bits:expr, $signed:expr, $size:expr;)*) => {$(
        impl<'a, H: Host> FromArg<'a, H> for $t {
            #[inline(always)]
            fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
                H::to_int(cx, v, at, crate::host::IntKind::new($bits, $signed, $size)).map(|n| n as $t)
            }
        }
    )*};
}

int_arg! {
    i8 => 8, true, false;
    u8 => 8, false, false;
    i16 => 16, true, false;
    u16 => 16, false, false;
    i32 => 32, true, false;
    u32 => 32, false, false;
    i64 => 64, true, false;
    u64 => 64, false, false;
    isize => 64, true, true;
    usize => 64, false, true;
}

impl<'a, H: Host> FromArg<'a, H> for BigInt {
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        H::to_bigint(cx, v, at)
    }
}

impl<'a, H: Host> FromArg<'a, H> for bool {
    #[inline(always)]
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        H::to_bool(cx, v, at)
    }
}

impl<'a, H: Host> FromArg<'a, H> for &'a str {
    #[inline(always)]
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        H::to_str(cx, v, at)
    }
}

impl<'a, H: Host> FromArg<'a, H> for String {
    #[inline]
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        H::to_str(cx, v, at).map(str::to_owned)
    }
}

impl<'a, H: Host> FromArg<'a, H> for Cow<'a, str> {
    #[inline]
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        H::to_str(cx, v, at).map(Cow::Borrowed)
    }
}

impl<'a, H: Host> FromArg<'a, H> for &'a [u8] {
    #[inline(always)]
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        H::to_bytes(cx, v, at)
    }

    #[inline(always)]
    fn reserve(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<(), H::Error> {
        H::reserve_bytes(cx, v, at)
    }
}

impl<'a, H: Host> FromArg<'a, H> for &'a mut [u8] {
    #[inline(always)]
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        H::to_bytes_mut(cx, v, at)
    }

    #[inline(always)]
    fn reserve(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<(), H::Error> {
        H::reserve_bytes(cx, v, at)
    }
}

impl<'a, H: Host> FromArg<'a, H> for Vec<u8> {
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        H::to_byte_vec(cx, v, at)
    }
}

/// An optional parameter that tells "not passed" (`Passed(None)`) apart from every passed
/// value, the host's "no value" included (`Option<T>` maps both to `None`).
pub struct Passed<T>(pub Option<T>);

impl<'a, H: Host, T: FromArg<'a, H>> FromArg<'a, H> for Passed<T> {
    const CALLBACK: bool = T::CALLBACK;

    #[inline(always)]
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        T::from_arg(cx, v, at).map(|v| Passed(Some(v)))
    }

    #[inline(always)]
    fn from_missing(_: &'a H::Cx<'_>, _: Slot) -> Result<Self, H::Error> {
        Ok(Passed(None))
    }

    #[inline(always)]
    fn reserve(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<(), H::Error> {
        T::reserve(cx, v, at)
    }
}

impl<'a, H: Host, T: FromArg<'a, H>> FromArg<'a, H> for Option<T> {
    const CALLBACK: bool = T::CALLBACK;

    /// The host's "no value" converts to `None`.
    #[inline(always)]
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        if H::is_none(v) {
            Ok(None)
        } else {
            T::from_arg(cx, v, at).map(Some)
        }
    }

    #[inline(always)]
    fn from_missing(_: &'a H::Cx<'_>, _: Slot) -> Result<Self, H::Error> {
        Ok(None)
    }

    #[inline(always)]
    fn reserve(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<(), H::Error> {
        if H::is_none(v) {
            Ok(())
        } else {
            T::reserve(cx, v, at)
        }
    }
}

impl<'a, H: Host, T: FromArg<'a, H> + Elem> FromArg<'a, H> for Vec<T> {
    /// A sequence, element by element.
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        let items = H::to_seq(cx, v, at)?;
        items
            .iter()
            .enumerate()
            .map(|(k, e)| T::from_arg(cx, e, at.elem(k as u32)))
            .collect()
    }
}

impl<'a, H: Host, T: Class> FromArg<'a, H> for &'a T {
    #[inline]
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        H::class_ref(cx, v, at)
    }
}

impl<'a, H: Host, T: Class> FromArg<'a, H> for &'a mut T {
    #[inline]
    fn from_arg(cx: &'a H::Cx<'_>, v: &'a H::Value, at: Slot) -> Result<Self, H::Error> {
        H::class_mut(cx, v, at)
    }
}

impl<'a, H: Host, T: FromArg<'a, H>> FromRest<'a, H> for Vec<T> {
    fn from_rest(cx: &'a H::Cx<'_>, vals: &'a [H::Value], first: u32) -> Result<Self, H::Error> {
        vals.iter()
            .enumerate()
            .map(|(k, v)| T::from_arg(cx, v, Slot::arg(first + k as u32)))
            .collect()
    }
}

impl<'a, H: Host> FromRest<'a, H> for &'a [H::Value] {
    #[inline]
    fn from_rest(_: &'a H::Cx<'_>, vals: &'a [H::Value], _: u32) -> Result<Self, H::Error> {
        Ok(vals)
    }
}

impl<'a, H: Host, T: FromArg<'a, H>> FromVarKw<'a, H> for Vec<(String, T)> {
    fn from_varkw(cx: &'a H::Cx<'_>) -> Result<Self, H::Error> {
        H::varkw(cx)
            .into_iter()
            .map(|(k, v)| Ok((k.to_owned(), T::from_arg(cx, v, Slot::arg(u32::MAX - 1))?)))
            .collect()
    }
}

// ---- results -----------------------------------------------------------------------------------

impl<H: Host> IntoRet<H> for () {
    const MAY_RUN: bool = false;
    #[inline(always)]
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(H::unit(ctx))
    }
}

impl<H: Host> IntoRet<H> for bool {
    const MAY_RUN: bool = false;
    #[inline(always)]
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(H::from_bool(ctx, self))
    }
}

impl<H: Host> IntoRet<H> for f64 {
    const MAY_RUN: bool = false;
    #[inline(always)]
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(H::from_f64(ctx, self))
    }
}

impl<H: Host> IntoRet<H> for f32 {
    const MAY_RUN: bool = false;
    #[inline(always)]
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(H::from_f64(ctx, self as f64))
    }
}

macro_rules! int_ret {
    ($($t:ty),*) => {$(
        impl<H: Host> IntoRet<H> for $t {
            const MAY_RUN: bool = false;
            #[inline(always)]
            fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
                H::from_int(ctx, self as i128)
            }
        }
    )*};
}
int_ret!(i8, u8, i16, u16, i32, u32, i64, u64, isize, usize, i128);

impl<H: Host> IntoRet<H> for BigInt {
    const MAY_RUN: bool = false;
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(H::from_bigint(ctx, self))
    }
}

impl<H: Host> IntoRet<H> for String {
    const MAY_RUN: bool = false;
    #[inline]
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(H::from_string(ctx, self))
    }
}

impl<H: Host> IntoRet<H> for &str {
    const MAY_RUN: bool = false;
    #[inline]
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(H::from_str(ctx, self))
    }
}

impl<H: Host> IntoRet<H> for &String {
    const MAY_RUN: bool = false;
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(H::from_str(ctx, self))
    }
}

impl<H: Host> IntoRet<H> for Cow<'_, str> {
    const MAY_RUN: bool = false;
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(match self {
            Cow::Borrowed(s) => H::from_str(ctx, s),
            Cow::Owned(s) => H::from_string(ctx, s),
        })
    }
}

impl<H: Host> IntoRet<H> for char {
    const MAY_RUN: bool = false;
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        let mut b = [0u8; 4];
        Ok(H::from_str(ctx, self.encode_utf8(&mut b)))
    }
}

impl<H: Host> IntoRet<H> for Vec<u8> {
    const MAY_RUN: bool = false;
    #[inline]
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(H::from_bytes(ctx, self))
    }
}

impl<H: Host> IntoRet<H> for Box<[u8]> {
    const MAY_RUN: bool = false;
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(H::from_bytes(ctx, self.into_vec()))
    }
}

impl<H: Host> IntoRet<H> for &[u8] {
    const MAY_RUN: bool = false;
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(H::from_bytes(ctx, self.to_vec()))
    }
}

impl<H: Host> IntoRet<H> for &mut [u8] {
    const MAY_RUN: bool = false;
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        Ok(H::from_bytes(ctx, self.to_vec()))
    }
}

impl<H: Host, T: IntoRet<H>> IntoRet<H> for Option<T> {
    const MAY_RUN: bool = T::MAY_RUN;
    #[inline]
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        match self {
            Some(v) => v.into_ret(ctx),
            None => Ok(H::none(ctx)),
        }
    }
}

impl<H: Host, T: IntoRet<H>, E: IntoError<H>> IntoRet<H> for Result<T, E> {
    const MAY_RUN: bool = T::MAY_RUN;
    #[inline]
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        match self {
            Ok(v) => v.into_ret(ctx),
            Err(e) => Err(e.into_error(ctx)),
        }
    }
}

impl<H: Host, T: IntoRet<H> + Elem> IntoRet<H> for Vec<T> {
    const MAY_RUN: bool = T::MAY_RUN;
    /// A list / array.
    fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        let mut out = Vec::with_capacity(self.len());
        for v in self {
            out.push(v.into_ret(ctx)?);
        }
        Ok(H::from_list(ctx, out))
    }
}

macro_rules! tuple_ret {
    ($($n:ident),+) => {
        impl<'a,H:Host,$($n:FromArg<'a,H>),+> FromArg<'a,H> for ($($n,)+) {
            #[allow(unused_assignments)]
            fn from_arg(cx:&'a H::Cx<'_>,value:&'a H::Value,at:Slot)->Result<Self,H::Error> {
                let items=H::to_seq(cx,value,at)?;
                if items.len()!=[$(stringify!($n)),+].len() {
                    return Err(H::with_ctx(cx,|ctx|H::error(ctx,NativeError::value_error("tuple has the wrong number of elements"))));
                }
                let mut index=0u32;
                Ok(($({let position=index;index+=1;$n::from_arg(cx,&items[position as usize],at.elem(position))?},)+))
            }
        }
        impl<H: Host, $($n: IntoRet<H>),+> IntoRet<H> for ($($n,)+) {
            const MAY_RUN: bool = false $(|| $n::MAY_RUN)+;
            /// A tuple (hosts without tuples: an array).
            #[allow(non_snake_case)]
            fn into_ret(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
                let ($($n,)+) = self;
                let out = vec![$($n.into_ret(ctx)?),+];
                Ok(H::from_tuple(ctx, out))
            }
        }
        impl<$($n: Elem),+> Elem for ($($n,)+) {}
    };
}
tuple_ret!(A);
tuple_ret!(A, B);
tuple_ret!(A, B, C);
tuple_ret!(A, B, C, D);
tuple_ret!(A, B, C, D, E);

impl<H: Host, T: IntoRet<H>> NextRet<H> for Option<T> {
    #[inline]
    fn into_next(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        let item = match self {
            Some(v) => Some(v.into_ret(ctx)?),
            None => None,
        };
        H::iter_step(ctx, item)
    }
}

impl<H: Host, T: NextRet<H>, E: IntoError<H>> NextRet<H> for Result<T, E> {
    #[inline]
    fn into_next(self, ctx: &mut H::Ctx) -> Result<H::Value, H::Error> {
        match self {
            Ok(v) => v.into_next(ctx),
            Err(e) => Err(e.into_error(ctx)),
        }
    }
}

impl<H: Host, T: Class> CtorRet<H, T> for T {
    #[inline]
    fn into_ctor(self, cx: &H::Cx<'_>) -> Result<H::Value, H::Error> {
        H::construct(cx, self)
    }
}

impl<H: Host, T: Class, R: CtorRet<H, T>, E: IntoError<H>> CtorRet<H, T> for Result<R, E> {
    #[inline]
    fn into_ctor(self, cx: &H::Cx<'_>) -> Result<H::Value, H::Error> {
        match self {
            Ok(r) => r.into_ctor(cx),
            Err(e) => Err(H::with_ctx(cx, |ctx| e.into_error(ctx))),
        }
    }
}

// ---- errors ------------------------------------------------------------------------------------

impl<H: Host> IntoError<H> for NativeError {
    fn into_error(self, ctx: &mut H::Ctx) -> H::Error {
        H::error(ctx, self)
    }
}

/// A plain message: a runtime error (`Error` in JS, `RuntimeError` in Python).
impl<H: Host> IntoError<H> for String {
    fn into_error(self, ctx: &mut H::Ctx) -> H::Error {
        H::error(ctx, NativeError::new(ErrorKind::Runtime, self))
    }
}

impl<H: Host> IntoError<H> for &'static str {
    fn into_error(self, ctx: &mut H::Ctx) -> H::Error {
        H::error(ctx, NativeError::new(ErrorKind::Runtime, self))
    }
}

/// An OS error: its errno and code (`ENOENT`) when known.
impl<H: Host> IntoError<H> for std::io::Error {
    fn into_error(self, ctx: &mut H::Ctx) -> H::Error {
        H::error(ctx, NativeError::from(self))
    }
}

macro_rules! elem {
    ($($t:ty),*) => {$( impl Elem for $t {} )*};
}
elem!(
    f64,
    f32,
    i8,
    i16,
    u16,
    i32,
    u32,
    i64,
    u64,
    isize,
    usize,
    i128,
    bool,
    char,
    String,
    BigInt,
    Vec<u8>
);
impl Elem for &str {}
impl Elem for Cow<'_, str> {}
impl<T: Elem> Elem for Option<T> {}
impl<T: Elem> Elem for Vec<T> {}
