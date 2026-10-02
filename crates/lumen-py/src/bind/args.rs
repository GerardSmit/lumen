//! What CPython derives from a C signature, derived here from a neutral [`FnDesc`]: the Python
//! name, the calling convention ([`Conv`]), every argument-error message word for word, the
//! keyword binder and `__text_signature__`.
//!
//! The message family depends on the calling convention CPython would use for the same
//! signature: `METH_NOARGS`, `METH_O`, positional-only Argument Clinic
//! (`_PyArg_CheckPositional`), keyword Argument Clinic (`_PyArg_UnpackKeywords`), a slot wrapper,
//! `PyArg_ParseTuple` (`hint(py(arg_style = "parse"))`) or `PyArg_UnpackTuple`
//! (`hint(py(arg_style = "unpack"))`, worded as the positional-only Clinic).
//!
//! Python hints (`hint(py(..))` on an `#[op]` / member): `text_signature = ".."` (`""`: none),
//! `arg_style = "parse" | "unpack"`, `arg_name = ".."` (the name in argument errors), `aliases = "a, b"`.

use crate::object::*;
use crate::vm::Interp;
use lumen_bind::{setter_property, FnDesc, ParamKind, Role};

pub const HOST: &str = "py";

/// The CPython calling convention a signature maps to (it selects the error wording).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Conv {
    /// `f() takes no arguments (1 given)`.
    NoArgs,
    /// `f() takes exactly one argument (2 given)`.
    O,
    /// `f expected at most 2 arguments, got 3`.
    Positional,
    /// `_PyArg_UnpackKeywords`.
    Keywords,
    /// A special method behind a type slot: `expected 1 argument, got 0`.
    Slot,
    /// `PyArg_ParseTuple`: `insert() takes exactly 2 arguments (1 given)`.
    Parse,
}

/// The protocols CPython reaches through a type slot (their argument errors come from the slot
/// wrapper).
const SLOT_PROTOS: &[&str] = &[
    "len", "getitem", "setitem", "delitem", "contains", "iter", "next", "repr", "str", "hash", "bool", "eq", "ne", "lt",
    "le", "gt", "ge", "add", "radd", "iadd", "sub", "rsub", "isub", "mul", "rmul", "imul", "and", "rand", "iand", "or",
    "ror", "ior", "xor", "rxor", "ixor", "index", "int", "float", "neg", "pos", "abs", "invert",
];

/// Whether CPython exposes the member as a slot wrapper (`<slot wrapper '__init__' ..>`), whose
/// receiver errors read `descriptor '__init__' requires a 'X' object but received a 'Y'`.
pub fn is_slot_wrapper(d: &FnDesc) -> bool {
    matches!(d.role, Role::Proto(p) if p == "init" || SLOT_PROTOS.contains(&p))
}

/// `__len__` for `len`, ... (every neutral protocol is a dunder of the same name).
fn dunder(p: &'static str) -> &'static str {
    macro_rules! table {
        ($($p:literal)*) => {
            match p { $($p => concat!("__", $p, "__"),)* _ => p }
        };
    }
    table!("init" "len" "getitem" "setitem" "delitem" "contains" "iter" "next" "reversed" "repr" "str" "hash"
        "bool" "eq" "ne" "lt" "le" "gt" "ge" "add" "radd" "iadd" "sub" "rsub" "isub" "mul" "rmul" "imul"
        "and" "rand" "iand" "or" "ror" "ior" "xor" "rxor" "ixor" "neg" "pos" "abs" "invert" "index" "int"
        "float" "call" "copy" "deepcopy" "reduce" "sizeof" "enter" "exit")
}

/// The attribute name a fn gets in Python.
pub fn py_name(d: &'static FnDesc) -> &'static str {
    if let Some(n) = d.fixed_name(HOST) {
        return n;
    }
    match d.role {
        Role::Constructor => "__new__",
        Role::Proto(p) => dunder(p),
        Role::Setter => setter_property(d.name),
        _ => d.name,
    }
}

/// Extra names bound to the same native (`hint(py(aliases = "__rmul__"))`).
pub fn aliases(d: &'static FnDesc) -> impl Iterator<Item = &'static str> {
    d.hint(HOST, "aliases").unwrap_or("").split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// The name of the class a member belongs to (`deque`).
pub fn class_name(d: &FnDesc) -> &'static str {
    d.class().map_or("", |c| c.name_for(HOST))
}

/// `collections.deque` (the bare name for `builtins`).
pub fn class_qualname(c: &lumen_bind::ClassDesc) -> String {
    match c.module {
        Some(m) if m != "builtins" => format!("{}.{}", m, c.name_for(HOST)),
        _ => c.name_for(HOST).to_string(),
    }
}

/// Whether the native's first positional argument is its receiver (self, or the class of
/// `__new__` / a class method).
#[inline]
pub fn takes_receiver(d: &FnDesc) -> bool {
    d.role.has_receiver() || d.role == Role::Constructor || d.has(lumen_bind::flags::CLASS_RECV)
}

/// The runtime shape of a Python signature, derived on the slow paths (errors and keyword
/// binding) only.
pub struct PySig {
    /// The bare name used by Argument Clinic messages (`isclose`; the class name for
    /// `__new__` / `__init__`).
    pub name: String,
    /// The owner shown in `METH_O`-style messages: the module of a function (`math.floor()`),
    /// the class of a method (`deque.append()`); empty for none.
    pub owner: String,
    pub conv: Conv,
    /// Every named parameter, in order.
    pub names: Vec<&'static str>,
    pub posonly: usize,
    pub maxpos: usize,
    pub minpos: usize,
    /// Bit `i` set: `names[i]` is required.
    pub required: u64,
    pub varargs: bool,
    pub varkw: bool,
}

impl PySig {
    /// `of(d)`, derived once per desc: keyword calls bind through it on every call.
    pub fn cached(d: &'static FnDesc) -> &'static PySig {
        thread_local! {
            static CACHE: std::cell::RefCell<std::collections::HashMap<usize, &'static PySig>> = Default::default();
        }
        let key = d as *const FnDesc as usize;
        CACHE.with(|c| {
            if let Some(s) = c.borrow().get(&key) {
                return *s;
            }
            let s: &'static PySig = Box::leak(Box::new(PySig::of(d)));
            c.borrow_mut().insert(key, s);
            s
        })
    }

    pub fn of(d: &'static FnDesc) -> PySig {
        let named: Vec<_> = d.named().collect();
        let posonly = named.iter().filter(|p| p.kind == ParamKind::PosOnly).count();
        let maxpos = d.max_pos as usize;
        let minpos = d.min_pos as usize;
        let varargs = d.has_varargs();
        let varkw = d.has_varkw();
        let required = named.iter().enumerate().filter(|(_, p)| !p.optional()).fold(0u64, |m, (i, _)| m | (1u64 << i.min(63)));
        let n = named.len();
        let all_pos = posonly == n;
        let conv = match d.role {
            _ if d.hint(HOST, "arg_style") == Some("parse") => Conv::Parse,
            _ if d.hint(HOST, "arg_style") == Some("unpack") => Conv::Positional,
            Role::Getter => Conv::NoArgs,
            Role::Setter => Conv::O,
            Role::Constructor if all_pos && !varkw => Conv::Positional,
            Role::Constructor => Conv::Keywords,
            Role::Proto(p) if SLOT_PROTOS.contains(&p) && all_pos && !varargs && !varkw => Conv::Slot,
            _ if n == 0 && !varargs && !varkw => Conv::NoArgs,
            _ if n == 1 && posonly == 1 && minpos == 1 && !varargs && !varkw => Conv::O,
            _ if all_pos && !varkw => Conv::Positional,
            _ => Conv::Keywords,
        };
        let pyname = py_name(d);
        let (name, owner) = match d.role {
            Role::Constructor | Role::Proto("init") => (class_name(d).to_string(), String::new()),
            _ if d.class().is_some() => (pyname.to_string(), class_name(d).to_string()),
            _ => (pyname.to_string(), d.module().unwrap_or("builtins").to_string()),
        };
        let name = d.hint(HOST, "arg_name").map_or(name, str::to_string);
        PySig { name, owner, conv, names: named.iter().map(|p| p.name).collect(), posonly, maxpos, minpos, required, varargs, varkw }
    }

    /// `math.floor` / `deque.append` / `count`.
    pub fn qualname(&self) -> String {
        if self.owner.is_empty() || self.owner == "builtins" {
            self.name.clone()
        } else {
            format!("{}.{}", self.owner, self.name)
        }
    }

    fn minposonly(&self) -> usize {
        self.minpos.min(self.posonly)
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// The arity error for `given` positional arguments (no keywords involved).
#[cold]
#[inline(never)]
pub fn arity_error(it: &mut Interp, sig: &PySig, given: usize) -> Obj {
    let msg = match sig.conv {
        Conv::NoArgs => format!("{}() takes no arguments ({} given)", sig.qualname(), given),
        Conv::O => format!("{}() takes exactly one argument ({} given)", sig.qualname(), given),
        Conv::Positional => {
            let (min, max) = (sig.minpos, if sig.varargs { usize::MAX } else { sig.maxpos });
            if given < min {
                let q = if min == max { "" } else { "at least " };
                format!("{} expected {}{} argument{}, got {}", sig.name, q, min, plural(min), given)
            } else {
                let q = if min == max { "" } else { "at most " };
                format!("{} expected {}{} argument{}, got {}", sig.name, q, max, plural(max), given)
            }
        }
        Conv::Parse => {
            let (min, max) = (sig.minpos, if sig.varargs { usize::MAX } else { sig.maxpos });
            let q = if min == max { "exactly" } else if given < min { "at least" } else { "at most" };
            let n = if given < min { min } else { max };
            format!("{}() takes {} {} argument{} ({} given)", sig.name, q, n, plural(n), given)
        }
        Conv::Keywords => return keywords_error(it, sig, given, 0).unwrap_or_else(|| it.type_error("invalid arguments")),
        Conv::Slot => {
            let (min, max) = (sig.minpos, sig.maxpos);
            let q = if min == max { "" } else if given < min { "at least " } else { "at most " };
            let n = if given < min { min } else { max };
            format!("expected {}{} argument{}, got {}", q, n, plural(n), given)
        }
    };
    it.type_error(&msg)
}

/// `f() takes no keyword arguments`.
#[cold]
#[inline(never)]
pub fn no_kwargs_error(it: &mut Interp, sig: &PySig) -> Obj {
    let msg = if sig.conv == Conv::Slot {
        format!("wrapper {}() takes no keyword arguments", sig.name)
    } else {
        format!("{}() takes no keyword arguments", sig.qualname())
    };
    it.type_error(&msg)
}

/// Count errors of `_PyArg_UnpackKeywords` (`nargs` positional, `nkw` keyword arguments).
fn keywords_error(it: &mut Interp, sig: &PySig, nargs: usize, nkw: usize) -> Option<Obj> {
    let maxargs = sig.names.len();
    let msg = if !sig.varargs && !sig.varkw && nargs + nkw > maxargs {
        format!(
            "{}() takes at most {} {}argument{} ({} given)",
            sig.name,
            maxargs,
            if nargs == 0 { "keyword " } else { "" },
            plural(maxargs),
            nargs + nkw
        )
    } else if !sig.varargs && nargs > sig.maxpos {
        if sig.maxpos == 0 {
            format!("{}() takes no positional arguments", sig.name)
        } else {
            format!(
                "{}() takes {} {} positional argument{} ({} given)",
                sig.name,
                if sig.minpos < sig.maxpos { "at most" } else { "exactly" },
                sig.maxpos,
                plural(sig.maxpos),
                nargs
            )
        }
    } else if nargs < sig.minposonly() {
        let m = sig.minposonly();
        format!(
            "{}() takes {} {} positional argument{} ({} given)",
            sig.name,
            if m < sig.maxpos { "at least" } else { "exactly" },
            m,
            plural(m),
            nargs
        )
    } else {
        return None;
    };
    Some(it.type_error(&msg))
}

#[cold]
#[inline(never)]
fn missing_error(it: &mut Interp, sig: &PySig, i: usize) -> Obj {
    let msg = format!("{}() missing required argument '{}' (pos {})", sig.name, sig.names[i], i + 1);
    it.type_error(&msg)
}

/// Binds a call that missed the inline positional fast path into `slots` (one per named
/// parameter), raising CPython's error for the signature's convention.
#[cold]
#[inline(never)]
pub fn bind_slow<'a>(
    it: &mut Interp,
    d: &'static FnDesc,
    args: &'a [Value],
    kw: &'a [(Obj, Value)],
    slots: &mut [Option<&'a Value>],
) -> R<()> {
    let sig = PySig::cached(d);
    if sig.conv != Conv::Keywords {
        // A `#[varkw]` collector receives the keywords and judges them itself.
        if !kw.is_empty() && !sig.varkw {
            return Err(no_kwargs_error(it, sig));
        }
        let n = args.len();
        if n < sig.minpos || (!sig.varargs && n > sig.maxpos) {
            return Err(arity_error(it, sig, n));
        }
        for (slot, a) in slots.iter_mut().zip(args) {
            *slot = Some(a);
        }
        return Ok(());
    }
    bind_keywords(it, sig, args, kw, slots)
}

/// `_PyArg_UnpackKeywords`: positional and keyword arguments into `slots`. Keywords not
/// matching a parameter are an error unless the signature has `**kwargs`.
fn bind_keywords<'a>(
    it: &mut Interp,
    sig: &PySig,
    args: &'a [Value],
    kw: &'a [(Obj, Value)],
    slots: &mut [Option<&'a Value>],
) -> R<()> {
    let nargs = args.len();
    if let Some(e) = keywords_error(it, sig, nargs, kw.len()) {
        return Err(e);
    }
    let npos = nargs.min(sig.maxpos);
    for (slot, a) in slots.iter_mut().zip(&args[..npos]) {
        *slot = Some(a);
    }
    let find = |name: &str| sig.names[sig.posonly..].iter().position(|n| *n == name).map(|i| i + sig.posonly);
    let mut leftover = false;
    for (k, v) in kw {
        match find(k.as_str_kind().unwrap_or("")) {
            Some(i) if i >= npos => slots[i] = Some(v),
            _ => leftover = true,
        }
    }
    for (i, slot) in slots.iter().enumerate() {
        if slot.is_none() && i < 64 && sig.required & (1 << i) != 0 {
            return Err(missing_error(it, sig, i));
        }
    }
    if leftover {
        for (k, _) in kw {
            let name = k.as_str_kind().unwrap_or("");
            if let Some(i) = find(name).filter(|&i| i < npos) {
                let msg = format!("argument for {}() given by name ('{}') and position ({})", sig.name, name, i + 1);
                return Err(it.type_error(&msg));
            }
        }
        if !sig.varkw {
            for (k, _) in kw {
                let name = k.as_str_kind().unwrap_or("");
                if find(name).is_none() {
                    let msg = format!("'{}' is an invalid keyword argument for {}()", name, sig.name);
                    return Err(it.type_error(&msg));
                }
            }
        }
    }
    Ok(())
}

/// Whether keyword `name` binds a named parameter (so a `**kwargs` collector skips it).
pub fn is_named_keyword(d: &FnDesc, name: &str) -> bool {
    d.named().any(|p| p.kind != ParamKind::PosOnly && p.name == name)
}

/// Argument `i` has the wrong type: `f() argument 'x' must be str, not int`.
#[cold]
#[inline(never)]
pub fn bad_argument(it: &mut Interp, d: &'static FnDesc, i: usize, what: &str, v: &Value) -> Obj {
    let sig = PySig::of(d);
    let display = if sig.conv == Conv::O {
        "argument".to_string()
    } else if i < sig.posonly {
        format!("argument {}", i + 1)
    } else {
        match sig.names.get(i) {
            Some(n) => format!("argument '{}'", n),
            None => "argument".to_string(),
        }
    };
    let t = it.type_name_of(v);
    it.type_error(&format!("{}() {} must be {}, not {}", sig.name, display, what, t))
}

/// `descriptor 'append' of 'collections.deque' object needs an argument`.
#[cold]
#[inline(never)]
pub fn needs_self(it: &mut Interp, d: &'static FnDesc) -> Obj {
    let owner = d.class().map(class_qualname).unwrap_or_default();
    match d.role {
        Role::Constructor => it.type_error(&format!("{}.__new__(): not enough arguments", class_name(d))),
        Role::Method => it.type_error(&format!("unbound method {}.{}() needs an argument", class_name(d), py_name(d))),
        _ => it.type_error(&format!("descriptor '{}' of '{}' object needs an argument", py_name(d), owner)),
    }
}

/// A Rust default as Python source (`true` -> `True`, `"x"` -> `'x'`).
fn py_default(text: &str) -> String {
    let t = text.trim();
    match t {
        "true" => "True".into(),
        "false" => "False".into(),
        "None" | "Value::None" => "None".into(),
        _ if t.len() >= 2 && t.starts_with('"') && t.ends_with('"') => format!("'{}'", &t[1..t.len() - 1]),
        _ => t.trim_end_matches("f64").trim_end_matches("_f64").replace(' ', ""),
    }
}

/// The signature CPython's slot wrapper shows for a special method: its parameter names are
/// fixed by the slot (`($self, value, /)`).
fn slot_text_sig(name: &str, nargs: usize) -> String {
    let names: &[&str] = match name {
        "__getitem__" | "__delitem__" | "__contains__" => &["key"],
        "__setitem__" => &["key", "value"],
        _ => &["value"],
    };
    let mut parts = vec!["$self"];
    parts.extend(names.iter().take(nargs));
    parts.push("/");
    format!("({})", parts.join(", "))
}

/// `__text_signature__` (`($module, x, /)`), or `None` (`hint(py(text_signature = ""))`).
pub fn text_signature(d: &'static FnDesc) -> Option<String> {
    match d.hint(HOST, "text_signature") {
        Some("") => return None,
        Some(s) => return Some(s.to_string()),
        None => {}
    }
    if matches!(d.role, Role::Getter | Role::Setter) {
        return None;
    }
    let sig = PySig::of(d);
    if sig.conv == Conv::Slot {
        return Some(slot_text_sig(py_name(d), sig.names.len()));
    }
    let prefix = match d.role {
        Role::Function => Some("$module"),
        Role::Method | Role::Proto(_) => Some("$self"),
        Role::Static if d.has(lumen_bind::flags::CLASS_RECV) => Some("$type"),
        _ => None,
    };
    let mut parts: Vec<String> = prefix.map(str::to_string).into_iter().collect();
    if prefix.is_some() && sig.posonly == 0 && !(sig.names.is_empty() && sig.varargs && !sig.varkw) {
        parts.push("/".into());
    }
    let mut star = false;
    let params = d.params;
    for (n, p) in params.iter().enumerate() {
        match p.kind {
            ParamKind::VarArgs => {
                parts.push(format!("*{}", p.name));
                star = true;
            }
            ParamKind::VarKw => parts.push(format!("**{}", p.name)),
            _ => {
                if p.kind == ParamKind::KwOnly && !star {
                    parts.push("*".into());
                    star = true;
                }
                parts.push(match p.default {
                    Some(dft) => format!("{}={}", p.name, py_default(dft)),
                    None => p.name.to_string(),
                });
            }
        }
        let next_posonly = params.get(n + 1).is_some_and(|x| x.kind == ParamKind::PosOnly);
        if p.kind == ParamKind::PosOnly && !next_posonly {
            parts.push("/".into());
        }
    }
    Some(format!("({})", parts.join(", ")))
}
