//! The neutral descriptors the macros emit: what a fn, class or module *is*, with no host
//! vocabulary. Each host derives its own names, arities, signatures and error wording from them.

/// How a script-visible parameter may be passed. Hosts without keywords (JS) pass every named
/// parameter positionally, in declaration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamKind {
    /// Positional only (the default).
    PosOnly,
    /// Positional or by keyword (`#[kw]`).
    PosOrKw,
    /// Keyword only (`#[kwonly]`).
    KwOnly,
    /// The rest of the positional arguments (`#[varargs]`).
    VarArgs,
    /// The keyword arguments not bound to a named parameter (`#[varkw]`).
    VarKw,
}

#[derive(Debug)]
pub struct Param {
    pub name: &'static str,
    pub kind: ParamKind,
    /// The default as written (`#[default(1e-09)]` gives `1e-09`), or `None` for an optional
    /// `Option<T>` parameter; absent for a required parameter. Hosts show it in signatures.
    pub default: Option<&'static str>,
}

impl Param {
    pub fn optional(&self) -> bool {
        self.default.is_some()
    }
    pub fn named(&self) -> bool {
        !matches!(self.kind, ParamKind::VarArgs | ParamKind::VarKw)
    }
}

/// What a fn is to its owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// A free function (an `#[op]`).
    Function,
    /// An instance method (`&self`, `&mut self` or a `This<_>` parameter).
    Method,
    /// A method of the class itself (no receiver).
    Static,
    Getter,
    Setter,
    /// Creates an instance (`#[constructor]`).
    Constructor,
    /// An instance method implementing a neutral protocol (`#[proto(len)]`); each host maps the
    /// name to its own hook (Python `__len__`, JS a `length` getter, ...). See [`PROTOCOLS`].
    Proto(&'static str),
}

impl Role {
    /// Whether the fn receives an instance.
    pub fn has_receiver(self) -> bool {
        matches!(self, Role::Method | Role::Getter | Role::Setter | Role::Proto(_))
    }
}

/// The neutral protocol names `#[proto(..)]` accepts.
pub const PROTOCOLS: &[&str] = &[
    "init", "len", "getitem", "setitem", "delitem", "contains", "iter", "next", "reversed", "repr", "str", "hash",
    "bool", "eq", "ne", "lt", "le", "gt", "ge", "add", "radd", "iadd", "sub", "rsub", "isub", "mul", "rmul", "imul",
    "and", "rand", "iand", "or", "ror", "ior", "xor", "rxor", "ixor", "neg", "pos", "abs", "invert", "index", "int",
    "float", "call", "copy", "deepcopy", "reduce", "sizeof", "enter", "exit", "await", "aiter", "anext",
    "truediv", "rtruediv", "itruediv", "floordiv", "rfloordiv", "ifloordiv", "mod", "rmod", "imod", "pow", "rpow",
    "ipow", "lshift", "rlshift", "ilshift", "rshift", "rrshift", "irshift", "matmul", "rmatmul", "imatmul", "divmod",
    "rdivmod", "getattribute", "setattr", "delattr",
];

/// The declaration a fn belongs to.
#[derive(Clone, Copy, Debug)]
pub enum Owner {
    None,
    /// The `#[module]` it was declared in (its name).
    Module(&'static str),
    Class(&'static ClassDesc),
}

/// `FnDesc::flags`.
pub mod flags {
    /// Lenient argument conversion (`#[op(coerce)]`): the host's own coercions (JS ToNumber /
    /// ToString / ToBoolean) instead of strict type checks.
    pub const COERCE: u32 = 1;
    /// Takes the host context (`&mut Ctx`): may run script code while it runs.
    pub const CTX: u32 = 2;
    /// Runs off the script thread (`#[op(async)]`) and returns a promise / future.
    pub const ASYNC: u32 = 4;
    /// Takes `&mut self`.
    pub const MUT_SELF: u32 = 8;
    /// Takes host state (`&State<T>` / `&mut State<T>`).
    pub const STATE: u32 = 16;
    /// A static member that receives the class as its receiver (`#[classmethod]`; `This<_>` is
    /// the class).
    pub const CLASS_RECV: u32 = 32;
}

/// `(host, key, value)` hints from `hint(host(key = "value", flag))`: host-specific tuning the
/// neutral description cannot express (a flag's value is empty). Only the named host reads them.
pub type Hints = &'static [(&'static str, &'static str, &'static str)];

fn hint_in(hints: Hints, host: &str, key: &str) -> Option<&'static str> {
    hints.iter().find(|(h, k, _)| *h == host && *k == key).map(|(_, _, v)| *v)
}

/// A bindable fn: generated as `<fn>::DESC` (ops) or per member (classes).
#[derive(Debug)]
pub struct FnDesc {
    /// The Rust name.
    pub name: &'static str,
    /// `name = ".."`: used verbatim by every host (no case conversion).
    pub explicit: Option<&'static str>,
    /// `rename(js = "..", py = "..")`: per-host names, over `explicit`.
    pub renames: &'static [(&'static str, &'static str)],
    /// `only(..)`: the hosts it is exposed to (empty: all).
    pub only: &'static [&'static str],
    /// `skip(..)`: hosts it is hidden from.
    pub skip: &'static [&'static str],
    pub hints: Hints,
    pub owner: Owner,
    pub role: Role,
    /// The doc comment.
    pub doc: Option<&'static str>,
    /// Script-visible parameters, in order.
    pub params: &'static [Param],
    /// Named parameters that may be passed positionally, and how many of those are required.
    pub max_pos: u16,
    pub min_pos: u16,
    pub flags: u32,
    /// The unboxed entry (emitted when every parameter and the result is a scalar).
    pub scalar: Option<ScalarEntry>,
}

impl FnDesc {
    pub fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }

    /// The value of hint `key` for `host` (`Some("")` for a flag).
    pub fn hint(&self, host: &str, key: &str) -> Option<&'static str> {
        hint_in(self.hints, host, key)
    }

    pub fn exposed_to(&self, host: &str) -> bool {
        exposed(self.only, self.skip, host)
    }

    /// The name a host must use when it does not derive one (`rename` for this host, else
    /// `name = ".."`).
    pub fn fixed_name(&self, host: &str) -> Option<&'static str> {
        fixed(self.renames, self.explicit, host)
    }

    pub fn class(&self) -> Option<&'static ClassDesc> {
        match self.owner {
            Owner::Class(c) => Some(c),
            _ => None,
        }
    }

    pub fn module(&self) -> Option<&'static str> {
        match self.owner {
            Owner::Module(m) => Some(m),
            _ => None,
        }
    }

    /// Named parameters (not `*args` / `**kwargs`), in order.
    pub fn named(&self) -> impl Iterator<Item = &'static Param> {
        self.params.iter().filter(|p| p.named())
    }

    pub fn has_varargs(&self) -> bool {
        self.params.iter().any(|p| p.kind == ParamKind::VarArgs)
    }

    pub fn has_varkw(&self) -> bool {
        self.params.iter().any(|p| p.kind == ParamKind::VarKw)
    }
}

/// `ClassDesc::flags`: the class is a generic container (`Deque[int]` in hosts with type
/// parameters on classes).
pub const CLASS_GENERIC: u32 = 1;

/// A `#[class]` struct.
#[derive(Debug)]
pub struct ClassDesc {
    pub name: &'static str,
    pub explicit: Option<&'static str>,
    pub renames: &'static [(&'static str, &'static str)],
    pub only: &'static [&'static str],
    pub skip: &'static [&'static str],
    pub hints: Hints,
    /// The public module the class reports (`module = ".."`, else the declaring `#[module]`).
    pub module: Option<&'static str>,
    pub doc: Option<&'static str>,
    pub flags: u32,
}

impl ClassDesc {
    pub fn exposed_to(&self, host: &str) -> bool {
        exposed(self.only, self.skip, host)
    }

    pub fn hint(&self, host: &str, key: &str) -> Option<&'static str> {
        hint_in(self.hints, host, key)
    }

    /// The class name for a host: `rename` for it, `name = ".."`, or the Rust name.
    pub fn name_for(&self, host: &str) -> &'static str {
        fixed(self.renames, self.explicit, host).unwrap_or(self.name)
    }
}

/// A `#[module]`.
#[derive(Debug)]
pub struct ModuleDesc {
    pub name: &'static str,
    pub explicit: Option<&'static str>,
    pub renames: &'static [(&'static str, &'static str)],
    pub doc: Option<&'static str>,
}

impl ModuleDesc {
    pub fn name_for(&self, host: &str) -> &'static str {
        fixed(self.renames, self.explicit, host).unwrap_or(self.name)
    }
}

fn exposed(only: &[&str], skip: &[&str], host: &str) -> bool {
    (only.is_empty() || only.contains(&host)) && !skip.contains(&host)
}

fn fixed(renames: &[(&'static str, &'static str)], explicit: Option<&'static str>, host: &str) -> Option<&'static str> {
    renames.iter().find(|(h, _)| *h == host).map(|(_, n)| *n).or(explicit)
}

/// `snake_case` -> `camelCase` (JS-style hosts derive member names this way).
pub fn camel_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut up = false;
    for (k, c) in s.chars().enumerate() {
        if c == '_' && k != 0 {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// The property a setter writes: `set_status` -> `status`.
pub fn setter_property(rust_name: &str) -> &str {
    rust_name.strip_prefix("set_").unwrap_or(rust_name)
}

// ---- unboxed entries ------------------------------------------------------------------------

/// An unboxed argument / result kind of a [`ScalarEntry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Scalar {
    F64,
    I32,
    U32,
    /// Passed and returned as an `i32` that is exactly 0 or 1.
    Bool,
    /// Result only: nothing.
    Void,
}

/// An untyped code pointer; the real type is `extern "C" fn(<args>) -> <ret>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CodePtr(pub *const ());
// SAFETY: a code address; carries no data.
unsafe impl Send for CodePtr {}
unsafe impl Sync for CodePtr {}

/// The unboxed entry of a fn whose parameters and result are all scalars: an `extern "C"`
/// function taking them directly (no context, cannot fail). A compiler that has proven the
/// argument kinds may call it instead of the boxed thunk; the conversions it implies are the
/// host's own (`i32` is the host's int32 conversion of its number type).
#[derive(Clone, Copy, Debug)]
pub struct ScalarEntry {
    pub args: &'static [Scalar],
    pub ret: Scalar,
    pub ptr: CodePtr,
}

/// Where a converted value came from; hosts word their argument errors with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
    arg: u32,
    elem: u32,
}

impl Slot {
    /// The receiver.
    pub const THIS: Slot = Slot { arg: u32::MAX, elem: u32::MAX };

    /// Named parameter `i` (0-based; for `*args`, `max_pos + k`).
    pub const fn arg(i: u32) -> Slot {
        Slot { arg: i, elem: u32::MAX }
    }

    /// Element `i` of the sequence at this slot.
    pub const fn elem(self, i: u32) -> Slot {
        Slot { arg: self.arg, elem: i }
    }

    pub fn is_this(self) -> bool {
        self.arg == u32::MAX
    }

    /// The argument index (`None` for the receiver).
    pub fn index(self) -> Option<u32> {
        (self.arg != u32::MAX).then_some(self.arg)
    }

    pub fn element(self) -> Option<u32> {
        (self.elem != u32::MAX).then_some(self.elem)
    }
}
