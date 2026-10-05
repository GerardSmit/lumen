//! The language-neutral error of a native binding (`#[op]` / `#[methods]` fns, see the
//! `lumen-bind` crate). An op that returns `Result<T, NativeError>` works unchanged in every
//! language backend: each language crate maps the [`ErrorKind`] to its own exception
//! (Python: `TypeError`, `ValueError`, `OverflowError`, `OSError`...; JS: `TypeError`,
//! `RangeError`, `Error`...). The mapping lives next to each language's conversion traits.

use std::borrow::Cow;
use std::fmt;

/// What went wrong, in terms both languages can express.
///
/// | kind            | Python              | JS           |
/// |-----------------|---------------------|--------------|
/// | `Type`          | `TypeError`         | `TypeError`  |
/// | `Value`         | `ValueError`        | `RangeError` |
/// | `Overflow`      | `OverflowError`     | `RangeError` |
/// | `Index`         | `IndexError`        | `RangeError` |
/// | `Key`           | `KeyError`          | `Error`      |
/// | `ZeroDivision`  | `ZeroDivisionError` | `RangeError` |
/// | `Runtime`       | `RuntimeError`      | `Error`      |
/// | `Buffer`        | `BufferError`       | `TypeError`  |
/// | `Memory`        | `MemoryError`       | `RangeError` |
/// | `Os(errno)`     | `OSError` subclass  | `Error`      |
/// | `NotImplemented`| `NotImplementedError` | `Error`    |
/// | `Named(n)`      | `RuntimeError`      | the error class `n` (`SyntaxError`, `DataCloneError`, ...) |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Type,
    Value,
    Overflow,
    Index,
    Key,
    ZeroDivision,
    Runtime,
    Buffer,
    Memory,
    /// An OS error with its `errno`.
    Os(i32),
    NotImplemented,
    /// An error class a language knows by name (a JS `DOMException` / `Error` subtype such as
    /// `SyntaxError` or `DataCloneError`); languages without it use their generic runtime error.
    Named(&'static str),
}

/// A neutral value: what a dynamically shaped native result or an error property holds. Every
/// language maps it to its own value (`None` is JS `undefined` / Python `None`).
#[derive(Clone, Debug, PartialEq, Default)]
pub enum Data {
    #[default]
    None,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    List(Vec<Data>),
}

macro_rules! data_from {
    ($($t:ty => $v:ident $(as $c:ty)?),* $(,)?) => {$(
        impl From<$t> for Data {
            fn from(x: $t) -> Data {
                Data::$v(x $(as $c)?)
            }
        }
    )*};
}
data_from!(bool => Bool, i64 => Int, i32 => Int as i64, u32 => Int as i64, f64 => Float, String => Str, Vec<u8> => Bytes);

impl From<&str> for Data {
    fn from(s: &str) -> Data {
        Data::Str(s.to_owned())
    }
}

impl<T: Into<Data>> From<Option<T>> for Data {
    fn from(o: Option<T>) -> Data {
        o.map_or(Data::None, Into::into)
    }
}

impl From<Vec<Data>> for Data {
    fn from(v: Vec<Data>) -> Data {
        Data::List(v)
    }
}

/// A native error: a kind, a message, an optional machine-readable code (Node's `err.code`, e.g.
/// `ERR_OUT_OF_RANGE`; ignored by Python) and further named properties (JS: own properties of
/// the error object; Python: attributes of the exception), e.g. `errno`, `syscall`.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeError {
    pub kind: ErrorKind,
    pub message: Cow<'static, str>,
    pub code: Option<Cow<'static, str>>,
    pub props: Vec<(Cow<'static, str>, Data)>,
}

macro_rules! ctors {
    ($($f:ident => $k:ident),* $(,)?) => {
        $(
            #[inline]
            pub fn $f(message: impl Into<Cow<'static, str>>) -> NativeError {
                NativeError::new(ErrorKind::$k, message)
            }
        )*
    };
}

impl NativeError {
    pub fn new(kind: ErrorKind, message: impl Into<Cow<'static, str>>) -> NativeError {
        NativeError {
            kind,
            message: message.into(),
            code: None,
            props: Vec::new(),
        }
    }

    ctors! {
        type_error => Type,
        value_error => Value,
        overflow => Overflow,
        index_error => Index,
        key_error => Key,
        zero_division => ZeroDivision,
        runtime => Runtime,
        buffer_error => Buffer,
        memory => Memory,
        not_implemented => NotImplemented,
    }

    pub fn os(errno: i32, message: impl Into<Cow<'static, str>>) -> NativeError {
        NativeError::new(ErrorKind::Os(errno), message)
    }

    pub fn with_code(mut self, code: impl Into<Cow<'static, str>>) -> NativeError {
        self.code = Some(code.into());
        self
    }

    /// A named class of error (see [`ErrorKind::Named`]).
    pub fn named(class: &'static str, message: impl Into<Cow<'static, str>>) -> NativeError {
        NativeError::new(ErrorKind::Named(class), message)
    }

    /// Attach a property (`err.errno`, `err.syscall`, ...).
    pub fn with_prop(
        mut self,
        name: impl Into<Cow<'static, str>>,
        value: impl Into<Data>,
    ) -> NativeError {
        self.props.push((name.into(), value.into()));
        self
    }
}

impl fmt::Display for NativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for NativeError {}

/// An `Os` error: the OS errno when there is one, and a POSIX code (`ENOENT`) from the kind.
impl From<std::io::Error> for NativeError {
    fn from(e: std::io::Error) -> NativeError {
        use std::io::ErrorKind as K;
        let code = match e.kind() {
            K::NotFound => Some("ENOENT"),
            K::PermissionDenied => Some("EACCES"),
            K::AlreadyExists => Some("EEXIST"),
            K::ConnectionRefused => Some("ECONNREFUSED"),
            K::ConnectionReset => Some("ECONNRESET"),
            K::ConnectionAborted => Some("ECONNABORTED"),
            K::TimedOut => Some("ETIMEDOUT"),
            K::BrokenPipe => Some("EPIPE"),
            K::InvalidInput => Some("EINVAL"),
            K::AddrInUse => Some("EADDRINUSE"),
            K::WouldBlock => Some("EAGAIN"),
            K::Interrupted => Some("EINTR"),
            K::Unsupported => Some("ENOTSUP"),
            K::DirectoryNotEmpty => Some("ENOTEMPTY"),
            K::IsADirectory => Some("EISDIR"),
            K::NotADirectory => Some("ENOTDIR"),
            _ => None,
        };
        NativeError {
            kind: ErrorKind::Os(e.raw_os_error().unwrap_or(0)),
            message: e.to_string().into(),
            code: code.map(Cow::Borrowed),
            props: Vec::new(),
        }
    }
}

/// `Result<T, NativeError>`.
pub type NativeResult<T> = Result<T, NativeError>;
