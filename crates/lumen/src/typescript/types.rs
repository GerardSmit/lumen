//! The checker's type model and its (sound) assignability relation.
//!
//! Differences from TypeScript's relation, each required by the sound subset
//! (docs/typed-tier.md §4.2):
//! - `any` is only a top type: everything is assignable *to* it, but it is assignable only to
//!   `any` / `unknown` (row 1). Functions that mention `any` are rejected by the checker anyway.
//! - Mutable arrays and tuples are invariant; `readonly T[]` is covariant (row 8).
//! - Function types are contravariant in every parameter, including methods, and arity must
//!   match; an optional parameter counts as `T | undefined` (row 9).
//! - Classes are nominal (`Instance`), related only through `extends` (row 10).
//! - Literal types are widened before variance comparisons (§4.3).

use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Any,
    Unknown,
    Never,
    Void,
    Undefined,
    Null,
    Boolean,
    Number,
    BigInt,
    String,
    Symbol,
    /// The `object` keyword.
    NonPrimitive,
    StringLiteral(String),
    NumberLiteral(f64),
    BooleanLiteral(bool),
    BigIntLiteral(String),
    /// Polymorphic `this` in a type position.
    This,
    /// A named type before resolution (`Foo`, `Array<T>`, `A.B`).
    Reference {
        name: String,
        arguments: Vec<Type>,
    },
    Array(Box<Type>),
    ReadonlyArray(Box<Type>),
    Tuple(Vec<Type>),
    Union(Vec<Type>),
    Intersection(Vec<Type>),
    Object(ObjectType),
    Function(Box<FnType>),
    /// A construct outside the checker's scope (conditional, mapped, `keyof`, `typeof`, indexed
    /// access, template literal, `infer`, `import(...)`, …). Holds a short description. It never
    /// causes an error by itself: it behaves as an unchecked dynamic value.
    Opaque(String),
    /// A nominal class instance (resolved by the checker; index into the class list).
    Instance(u32),
    /// A type parameter in scope (resolved by the checker). Erased: `unknown` inside the body.
    Param(String),
    /// A value from outside the checked program (a global, an import, a lib call result). Any
    /// operation on it is dynamic; flowing into a typed location inserts a check (§6.2).
    External,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ObjectType {
    pub props: Vec<Property>,
    pub index: Vec<IndexSignature>,
    pub calls: Vec<FnType>,
    pub constructs: Vec<FnType>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub name: String,
    pub optional: bool,
    pub readonly: bool,
    /// Declared with method syntax (`m(): T`).
    pub method: bool,
    pub ty: Type,
}

#[derive(Debug, Clone, PartialEq)]
pub struct IndexSignature {
    pub key: Type,
    pub value: Type,
    pub readonly: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FnType {
    pub type_params: Vec<TypeParam>,
    pub this: Option<Type>,
    pub params: Vec<Param>,
    pub ret: Type,
    pub predicate: Option<Predicate>,
    /// A construct signature (`new (...) => T`).
    pub construct: bool,
}

impl FnType {
    pub fn simple(params: Vec<Type>, ret: Type) -> FnType {
        FnType {
            type_params: Vec::new(),
            this: None,
            params: params
                .into_iter()
                .enumerate()
                .map(|(i, ty)| Param {
                    name: format!("p{i}"),
                    ty,
                    optional: false,
                    rest: false,
                })
                .collect(),
            ret,
            predicate: None,
            construct: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: Type,
    pub optional: bool,
    pub rest: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeParam {
    pub name: String,
    pub constraint: Option<Type>,
    pub default: Option<Type>,
}

/// `x is T`, `asserts x is T`, `asserts x`, `this is T`.
#[derive(Debug, Clone, PartialEq)]
pub struct Predicate {
    pub param: String,
    pub ty: Option<Type>,
    pub asserts: bool,
}

impl Type {
    pub fn union(members: Vec<Type>) -> Type {
        let mut out: Vec<Type> = Vec::new();
        for m in members {
            match m {
                Type::Union(inner) => {
                    for t in inner {
                        if !out.contains(&t) {
                            out.push(t);
                        }
                    }
                }
                Type::Never => {}
                t => {
                    if !out.contains(&t) {
                        out.push(t);
                    }
                }
            }
        }
        if out.contains(&Type::Any) {
            return Type::Any;
        }
        if out.contains(&Type::BooleanLiteral(true)) && out.contains(&Type::BooleanLiteral(false)) {
            out.retain(|t| !matches!(t, Type::BooleanLiteral(_)));
            if !out.contains(&Type::Boolean) {
                out.push(Type::Boolean);
            }
        }
        // Literals absorbed by their base type.
        let has = |out: &Vec<Type>, t: &Type| out.contains(t);
        let (n, s, b) = (
            has(&out, &Type::Number),
            has(&out, &Type::String),
            has(&out, &Type::Boolean),
        );
        out.retain(|t| {
            !((n && matches!(t, Type::NumberLiteral(_)))
                || (s && matches!(t, Type::StringLiteral(_)))
                || (b && matches!(t, Type::BooleanLiteral(_))))
        });
        match out.len() {
            0 => Type::Never,
            1 => out.pop().unwrap(),
            _ => Type::Union(out),
        }
    }

    pub fn members(&self) -> Vec<&Type> {
        match self {
            Type::Union(ms) => ms.iter().collect(),
            t => vec![t],
        }
    }

    pub fn contains_any(&self) -> bool {
        match self {
            Type::Any => true,
            Type::Reference { arguments, .. } => arguments.iter().any(Type::contains_any),
            Type::Array(t) | Type::ReadonlyArray(t) => t.contains_any(),
            Type::Tuple(ts) | Type::Union(ts) | Type::Intersection(ts) => {
                ts.iter().any(Type::contains_any)
            }
            Type::Object(o) => {
                o.props.iter().any(|p| p.ty.contains_any())
                    || o.index.iter().any(|i| i.value.contains_any())
                    || o.calls
                        .iter()
                        .chain(&o.constructs)
                        .any(FnType::contains_any)
            }
            Type::Function(f) => f.contains_any(),
            _ => false,
        }
    }

    /// Whether the type can hold `undefined` (or `null` when `null` is set).
    pub fn admits(&self, t: &Type) -> bool {
        self.members().iter().any(|m| {
            *m == t
                || matches!(
                    m,
                    Type::Unknown | Type::Any | Type::External | Type::Opaque(_)
                )
                || (matches!(t, Type::Undefined) && matches!(m, Type::Void))
        })
    }

    pub fn without_nullish(&self) -> Type {
        Type::union(
            self.members()
                .into_iter()
                .filter(|m| !matches!(m, Type::Null | Type::Undefined | Type::Void))
                .cloned()
                .collect(),
        )
    }

    /// Replaces literal types with their base types, recursively.
    pub fn widen(&self) -> Type {
        match self {
            Type::StringLiteral(_) => Type::String,
            Type::NumberLiteral(_) => Type::Number,
            Type::BooleanLiteral(_) => Type::Boolean,
            Type::BigIntLiteral(_) => Type::BigInt,
            Type::Array(t) => Type::Array(Box::new(t.widen())),
            Type::ReadonlyArray(t) => Type::ReadonlyArray(Box::new(t.widen())),
            Type::Tuple(ts) => Type::Tuple(ts.iter().map(Type::widen).collect()),
            Type::Union(ts) => Type::union(ts.iter().map(Type::widen).collect()),
            Type::Intersection(ts) => Type::Intersection(ts.iter().map(Type::widen).collect()),
            Type::Reference { name, arguments } => Type::Reference {
                name: name.clone(),
                arguments: arguments.iter().map(Type::widen).collect(),
            },
            Type::Object(o) => Type::Object(ObjectType {
                props: o
                    .props
                    .iter()
                    .map(|p| Property {
                        ty: p.ty.widen(),
                        ..p.clone()
                    })
                    .collect(),
                index: o
                    .index
                    .iter()
                    .map(|i| IndexSignature {
                        key: i.key.widen(),
                        value: i.value.widen(),
                        readonly: i.readonly,
                    })
                    .collect(),
                calls: o.calls.iter().map(FnType::widen).collect(),
                constructs: o.constructs.iter().map(FnType::widen).collect(),
            }),
            Type::Function(f) => Type::Function(Box::new(f.widen())),
            t => t.clone(),
        }
    }
}

impl FnType {
    pub fn contains_any(&self) -> bool {
        self.params.iter().any(|p| p.ty.contains_any())
            || self.ret.contains_any()
            || self.this.as_ref().is_some_and(Type::contains_any)
    }
    pub fn widen(&self) -> FnType {
        FnType {
            params: self
                .params
                .iter()
                .map(|p| Param {
                    ty: p.ty.widen(),
                    ..p.clone()
                })
                .collect(),
            ret: self.ret.widen(),
            this: self.this.as_ref().map(Type::widen),
            ..self.clone()
        }
    }
    /// A parameter's type as seen by the callee (`T | undefined` when optional).
    pub fn param_type(p: &Param) -> Type {
        if p.optional {
            Type::union(vec![p.ty.clone(), Type::Undefined])
        } else {
            p.ty.clone()
        }
    }
}

/// Class relations the assignability relation needs from the checker.
pub trait ClassHierarchy {
    fn parent(&self, class: u32) -> Option<u32>;
    /// The instance's public members as a structural type (for class → interface).
    fn instance_members(&self, class: u32) -> Option<ObjectType>;
}

struct NoClasses;
impl ClassHierarchy for NoClasses {
    fn parent(&self, _: u32) -> Option<u32> {
        None
    }
    fn instance_members(&self, _: u32) -> Option<ObjectType> {
        None
    }
}

/// Whether a value of `source` can be assigned to a location of `target` under the sound
/// subset's rules (see the module docs). Named references are compared nominally; the checker
/// resolves aliases, interfaces and classes before calling [`assignable_in`].
pub fn is_assignable(source: &Type, target: &Type) -> bool {
    assignable_in(source, target, &NoClasses)
}

/// Two types are equivalent: mutually assignable after widening literals.
pub fn equivalent_in(a: &Type, b: &Type, cx: &dyn ClassHierarchy) -> bool {
    let (a, b) = (a.widen(), b.widen());
    assignable_in(&a, &b, cx) && assignable_in(&b, &a, cx)
}

fn normalize(t: &Type) -> Option<Type> {
    match t {
        Type::Reference { name, arguments } if arguments.len() == 1 => match name.as_str() {
            "Array" => Some(Type::Array(Box::new(arguments[0].clone()))),
            "ReadonlyArray" => Some(Type::ReadonlyArray(Box::new(arguments[0].clone()))),
            _ => None,
        },
        _ => None,
    }
}

pub fn assignable_in(source: &Type, target: &Type, cx: &dyn ClassHierarchy) -> bool {
    if let Some(s) = normalize(source) {
        return assignable_in(&s, target, cx);
    }
    if let Some(t) = normalize(target) {
        return assignable_in(source, &t, cx);
    }
    if source == target && !matches!(source, Type::External) {
        return true;
    }
    if matches!(target, Type::Any | Type::Unknown) || matches!(source, Type::Never) {
        return true;
    }
    // Sound: `any` and external values need a check before they may flow anywhere narrower.
    if matches!(source, Type::Any | Type::External) {
        return false;
    }
    match (source, target) {
        (Type::Boolean, Type::Union(_)) => {
            assignable_in(&Type::BooleanLiteral(true), target, cx)
                && assignable_in(&Type::BooleanLiteral(false), target, cx)
        }
        (Type::Union(sources), _) => sources.iter().all(|s| assignable_in(s, target, cx)),
        (_, Type::Union(targets)) => targets.iter().any(|t| assignable_in(source, t, cx)),
        (_, Type::Intersection(targets)) => targets.iter().all(|t| assignable_in(source, t, cx)),
        (Type::Intersection(sources), _) => sources.iter().any(|s| assignable_in(s, target, cx)),
        (Type::StringLiteral(_), Type::String)
        | (Type::NumberLiteral(_), Type::Number)
        | (Type::BooleanLiteral(_), Type::Boolean)
        | (Type::BigIntLiteral(_), Type::BigInt)
        | (Type::Undefined, Type::Void) => true,
        // Mutable arrays are invariant (row 8).
        (Type::Array(s), Type::Array(t)) => equivalent_in(s, t, cx),
        (Type::Array(s) | Type::ReadonlyArray(s), Type::ReadonlyArray(t)) => {
            assignable_in(s, t, cx)
        }
        (Type::Tuple(ss), Type::Array(t)) => ss.iter().all(|s| equivalent_in(s, t, cx)),
        (Type::Tuple(ss), Type::ReadonlyArray(t)) => ss.iter().all(|s| assignable_in(s, t, cx)),
        (Type::Tuple(ss), Type::Tuple(ts)) => {
            ss.len() == ts.len() && ss.iter().zip(ts).all(|(s, t)| equivalent_in(s, t, cx))
        }
        (
            Type::Reference {
                name: sn,
                arguments: sa,
            },
            Type::Reference {
                name: tn,
                arguments: ta,
            },
        ) => {
            // Variance of a generic reference is unknown here, so it is invariant.
            sn == tn
                && sa.len() == ta.len()
                && sa.iter().zip(ta).all(|(s, t)| equivalent_in(s, t, cx))
        }
        (Type::Instance(s), Type::Instance(t)) => {
            let mut c = Some(*s);
            let mut depth = 0;
            while let Some(id) = c {
                if id == *t {
                    return true;
                }
                depth += 1;
                if depth > 256 {
                    return false;
                }
                c = cx.parent(id);
            }
            false
        }
        (Type::Instance(s), Type::Object(t)) => cx
            .instance_members(*s)
            .is_some_and(|members| object_assignable(&members, t, cx)),
        (Type::Object(s), Type::Object(t)) => object_assignable(s, t, cx),
        (Type::Function(f), Type::Object(t)) if t.props.is_empty() && t.calls.len() == 1 => {
            fn_assignable(f, &t.calls[0], cx)
        }
        (
            Type::Object(_)
            | Type::Instance(_)
            | Type::Array(_)
            | Type::ReadonlyArray(_)
            | Type::Tuple(_)
            | Type::Function(_),
            Type::NonPrimitive,
        ) => true,
        (Type::Function(s), Type::Function(t)) => fn_assignable(s, t, cx),
        _ => false,
    }
}

fn fn_assignable(s: &FnType, t: &FnType, cx: &dyn ClassHierarchy) -> bool {
    if s.params.len() != t.params.len() || s.construct != t.construct {
        return false;
    }
    let params_ok = s.params.iter().zip(&t.params).all(|(sp, tp)| {
        sp.rest == tp.rest && assignable_in(&FnType::param_type(tp), &FnType::param_type(sp), cx)
    });
    let this_ok = match (&s.this, &t.this) {
        (Some(st), Some(tt)) => assignable_in(tt, st, cx),
        (Some(st), None) => matches!(st, Type::Unknown | Type::Any),
        _ => true,
    };
    params_ok
        && this_ok
        && (matches!(t.ret, Type::Void) || assignable_in(&s.ret, &t.ret, cx))
        && (t.predicate.is_none() || s.predicate == t.predicate)
}

fn object_assignable(source: &ObjectType, target: &ObjectType, cx: &dyn ClassHierarchy) -> bool {
    let props_ok = target.props.iter().all(|expected| {
        match source.props.iter().find(|p| p.name == expected.name) {
            Some(actual) => {
                (!actual.optional || expected.optional)
                    && assignable_in(&actual.ty, &expected.ty, cx)
            }
            None => expected.optional,
        }
    });
    let index_ok = target.index.iter().all(|ti| {
        source
            .index
            .iter()
            .any(|si| assignable_in(&si.value, &ti.value, cx))
            || (source.index.is_empty()
                && source
                    .props
                    .iter()
                    .all(|p| assignable_in(&p.ty, &ti.value, cx)))
    });
    let calls_ok = target
        .calls
        .iter()
        .all(|tc| source.calls.iter().any(|sc| fn_assignable(sc, tc, cx)));
    props_ok && index_ok && calls_ok
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = |f: &mut fmt::Formatter<'_>, ts: &[Type], sep: &str| -> fmt::Result {
            for (i, t) in ts.iter().enumerate() {
                if i > 0 {
                    f.write_str(sep)?;
                }
                if matches!(t, Type::Function(_)) && sep != ", " {
                    write!(f, "({t})")?;
                } else {
                    write!(f, "{t}")?;
                }
            }
            Ok(())
        };
        match self {
            Type::Any => f.write_str("any"),
            Type::Unknown => f.write_str("unknown"),
            Type::Never => f.write_str("never"),
            Type::Void => f.write_str("void"),
            Type::Undefined => f.write_str("undefined"),
            Type::Null => f.write_str("null"),
            Type::Boolean => f.write_str("boolean"),
            Type::Number => f.write_str("number"),
            Type::BigInt => f.write_str("bigint"),
            Type::String => f.write_str("string"),
            Type::Symbol => f.write_str("symbol"),
            Type::NonPrimitive => f.write_str("object"),
            Type::StringLiteral(s) => write!(f, "{s:?}"),
            Type::NumberLiteral(n) => write!(f, "{n}"),
            Type::BooleanLiteral(b) => write!(f, "{b}"),
            Type::BigIntLiteral(b) => f.write_str(b),
            Type::This => f.write_str("this"),
            Type::Reference { name, arguments } => {
                f.write_str(name)?;
                if !arguments.is_empty() {
                    f.write_str("<")?;
                    list(f, arguments, ", ")?;
                    f.write_str(">")?;
                }
                Ok(())
            }
            Type::Array(t) | Type::ReadonlyArray(t) => {
                if matches!(self, Type::ReadonlyArray(_)) {
                    f.write_str("readonly ")?;
                }
                if matches!(
                    **t,
                    Type::Union(_) | Type::Function(_) | Type::Intersection(_)
                ) {
                    write!(f, "({t})[]")
                } else {
                    write!(f, "{t}[]")
                }
            }
            Type::Tuple(ts) => {
                f.write_str("[")?;
                list(f, ts, ", ")?;
                f.write_str("]")
            }
            Type::Union(ts) => list(f, ts, " | "),
            Type::Intersection(ts) => list(f, ts, " & "),
            Type::Object(o) => {
                f.write_str("{ ")?;
                for p in &o.props {
                    let q = if p.optional { "?" } else { "" };
                    let r = if p.readonly { "readonly " } else { "" };
                    write!(f, "{r}{}{q}: {}; ", p.name, p.ty)?;
                }
                for i in &o.index {
                    write!(f, "[key: {}]: {}; ", i.key, i.value)?;
                }
                for c in &o.calls {
                    write!(f, "{c}; ")?;
                }
                f.write_str("}")
            }
            Type::Function(func) => write!(f, "{func}"),
            Type::Opaque(what) => write!(f, "/*{what}*/unknown"),
            Type::Instance(id) => write!(f, "class#{id}"),
            Type::Param(name) => f.write_str(name),
            Type::External => f.write_str("/*external*/unknown"),
        }
    }
}

impl fmt::Display for FnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.construct {
            f.write_str("new ")?;
        }
        f.write_str("(")?;
        let mut first = true;
        if let Some(this) = &self.this {
            write!(f, "this: {this}")?;
            first = false;
        }
        for p in &self.params {
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            let dots = if p.rest { "..." } else { "" };
            let q = if p.optional { "?" } else { "" };
            write!(f, "{dots}{}{q}: {}", p.name, p.ty)?;
        }
        write!(f, ") => {}", self.ret)
    }
}
