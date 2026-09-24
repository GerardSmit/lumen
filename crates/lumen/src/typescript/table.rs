//! The engine-facing type-facts table (docs/typed-tier.md §3.1, milestone H1 front-end half).
//!
//! Everything here is keyed by byte offsets into the source, which the location-preserving
//! strip keeps identical in the text the engine compiles. The engine only ever sees
//! [`TKind`]s: the coarse projection of a TypeScript type onto what it can represent or check
//! in O(1).

use super::ast::FnKind;
use std::fmt;

/// Tag bits for [`TKind::Tags`]: bit `n` is the engine's `TAG_*` value `n`
/// (`crates/lumen/src/bytecode/jit/mod.rs`), so a check is `(1 << tag) & mask != 0`.
pub mod tag {
    pub const UNDEFINED: u16 = 1 << 0;
    pub const NULL: u16 = 1 << 2;
    pub const BOOL: u16 = 1 << 3;
    pub const NUM: u16 = 1 << 4;
    pub const BIGINT: u16 = 1 << 5;
    pub const STR: u16 = 1 << 6;
    pub const SYM: u16 = 1 << 7;
    pub const OBJ: u16 = 1 << 8;
    pub const ALL: u16 = UNDEFINED | NULL | BOOL | NUM | BIGINT | STR | SYM | OBJ;
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TKind {
    /// No information: no check, no assumption.
    Any,
    Num,
    Bool,
    Str,
    BigInt,
    Undef,
    Null,
    Sym,
    /// A union of the above (and/or `OBJ` for "some object") as a tag bitmask.
    Tags(u16),
    /// A nominal class instance (index into [`TypeTable::classes`]); only for classes whose
    /// layout is sound (their instances carry a typed shape).
    Class(u32),
    /// `C | null`.
    NullableClass(u32),
    /// `C | <primitive tags>`, e.g. `C | undefined` from an optional parameter.
    ClassOr(u32, u16),
    /// `number[]` / `readonly number[]`: flat f64 storage via the array mirror.
    NumArray,
    /// `T[]` for other `T`: only "is an array" is checked; elements are checked at use.
    Array(Box<TKind>),
    /// A callable with signature id (index into [`TypeTable::sigs`]).
    Func(u32),
    /// Any object (structural types): tag check only; property reads are checked at use.
    Object,
}

impl TKind {
    /// Whether a boundary check for this kind is O(1) and meaningful (row 2's "checkable").
    pub fn is_cast_checkable(&self) -> bool {
        !matches!(self, TKind::Any | TKind::Object | TKind::Func(_))
    }
}

impl fmt::Display for TKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tags = |f: &mut fmt::Formatter<'_>, m: u16| -> fmt::Result {
            let names = [
                (tag::NUM, "number"),
                (tag::STR, "string"),
                (tag::BOOL, "boolean"),
                (tag::BIGINT, "bigint"),
                (tag::SYM, "symbol"),
                (tag::OBJ, "object"),
                (tag::NULL, "null"),
                (tag::UNDEFINED, "undefined"),
            ];
            let parts: Vec<&str> = names
                .iter()
                .filter(|(b, _)| m & b != 0)
                .map(|(_, n)| *n)
                .collect();
            f.write_str(&parts.join("|"))
        };
        match self {
            TKind::Any => f.write_str("any"),
            TKind::Num => f.write_str("number"),
            TKind::Bool => f.write_str("boolean"),
            TKind::Str => f.write_str("string"),
            TKind::BigInt => f.write_str("bigint"),
            TKind::Undef => f.write_str("undefined"),
            TKind::Null => f.write_str("null"),
            TKind::Sym => f.write_str("symbol"),
            TKind::Tags(m) => tags(f, *m),
            TKind::Class(c) => write!(f, "class#{c}"),
            TKind::NullableClass(c) => write!(f, "class#{c}|null"),
            TKind::ClassOr(c, m) => {
                write!(f, "class#{c}|")?;
                tags(f, *m)
            }
            TKind::NumArray => f.write_str("number[]"),
            TKind::Array(k) => write!(f, "{k}[]"),
            TKind::Func(s) => write!(f, "fn#{s}"),
            TKind::Object => f.write_str("object"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteFact {
    /// A property access on a receiver of static class `class` reads/writes the full-layout
    /// slot `index` (inherited fields first).
    Field { class: u32, index: u16 },
    /// An element access; the element is checked against the kind at use (`NumArray` reads
    /// need only the bounds check). Out of bounds exits to bytecode (row 7).
    Elem(TKind),
    /// A call to the function whose table key (start offset) is given: a direct call to its
    /// typed entry.
    Callee(u32),
    /// A boundary check the checker inserted: the value of the expression must satisfy the
    /// kind, or the call exits to bytecode (§6.2).
    Check(TKind),
}

/// A fact about one expression. `start..end` is the expression's byte range (trimmed to its
/// tokens) in the source, which is also its range in the stripped text.
///
/// Anchors: `Field` sites cover the property name token (`.x` → `x`); `Elem` sites the whole
/// `a[i]` expression; `Callee` sites the whole call; `Check` sites the whole checked
/// expression (for `e as T` / `e!` that is `e` itself, the erased syntax excluded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    pub start: u32,
    pub end: u32,
    pub fact: SiteFact,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Signature {
    pub params: Vec<TKind>,
    pub ret: TKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FnTypes {
    pub name: String,
    pub kind: FnKind,
    /// The key again (`FnSource::Range.start`); for a constructor this is the class start.
    pub start: u32,
    /// The engine's `FnSource::Range.end`.
    pub end: u32,
    /// One per declared parameter: the kind of the *argument* as passed (an optional or
    /// defaulted parameter includes `undefined`; a rest parameter is its array).
    pub params: Vec<TKind>,
    /// The parameters' names, for reports (`_` for a destructuring pattern).
    pub param_names: Vec<String>,
    pub this: TKind,
    pub ret: TKind,
    /// Parameters (their in-body kind, after defaults) and `let`/`const`/`var` bindings,
    /// keyed by the binding identifier's start offset.
    pub locals: Vec<(u32, TKind)>,
    pub sites: Vec<Site>,
    pub sound: bool,
    pub sig: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClassLayout {
    pub name: String,
    /// The `class` keyword offset (the engine keys the class and its constructor here).
    pub start: u32,
    pub end: u32,
    /// Parent class (index into `classes`) when it is a class of the same file.
    pub parent: Option<u32>,
    /// Own instance fields in definition order: name, kind, readonly.
    pub fields: Vec<(String, TKind, bool)>,
    /// Own methods (including accessors): name and the method's table key.
    pub methods: Vec<(String, u32)>,
    /// All fields typed, always initialized to their kind, parent sound (§5.3).
    pub sound: bool,
}

impl ClassLayout {
    /// Fields including inherited ones, in instance slot order.
    pub fn full_fields<'a>(&'a self, all: &'a [ClassLayout]) -> Vec<&'a (String, TKind, bool)> {
        let mut out: Vec<&(String, TKind, bool)> = match self.parent {
            Some(p) if (p as usize) < all.len() => all[p as usize].full_fields(all),
            _ => Vec::new(),
        };
        for f in &self.fields {
            if !out.iter().any(|o| o.0 == f.0) {
                out.push(f);
            }
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Subject {
    /// A function, by table key.
    Fn(u32),
    /// A class, by index.
    Class(u32),
}

/// Why a function or class is not sound: the first reason found, with its location.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundnessNote {
    pub subject: Subject,
    pub at: u32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TypeTable {
    /// Sorted by offset.
    pub fns: Vec<(u32, FnTypes)>,
    pub classes: Vec<ClassLayout>,
    pub sigs: Vec<Signature>,
    pub report: Vec<SoundnessNote>,
}

impl TypeTable {
    /// The facts for the function whose `FnSource::Range.start` is `offset`.
    pub fn fn_at(&self, offset: u32) -> Option<&FnTypes> {
        self.fns
            .binary_search_by_key(&offset, |(o, _)| *o)
            .ok()
            .map(|i| &self.fns[i].1)
    }

    /// The class whose `class` keyword is at `offset`.
    pub fn class_at(&self, offset: u32) -> Option<(u32, &ClassLayout)> {
        self.classes
            .iter()
            .enumerate()
            .find(|(_, c)| c.start == offset)
            .map(|(i, c)| (i as u32, c))
    }

    pub fn note_for(&self, subject: Subject) -> Option<&SoundnessNote> {
        self.report.iter().find(|n| n.subject == subject)
    }

    pub fn sound_count(&self) -> (usize, usize) {
        (
            self.fns.iter().filter(|(_, f)| f.sound).count(),
            self.fns.len(),
        )
    }
}
